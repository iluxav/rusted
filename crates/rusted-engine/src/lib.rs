//! The rusted execution engine: QuickJS (via rquickjs) behind an [`Executor`]
//! trait. Fresh Runtime+Context per invocation; in-engine limits (uncatchable
//! wall interrupt, heap cap, stack cap) plus host-side output cap; structured
//! console logs. QuickJS was chosen over Boa by measurement — see the README.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use rquickjs::{
    AsyncContext, AsyncRuntime, Context, Ctx, Exception, Function, Module, Promise, Runtime, Value,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;

pub mod outbound;
pub use outbound::OutboundPolicy;

#[derive(Debug, Clone)]
pub struct Limits {
    pub wall_ms: u64,
    pub memory_bytes: usize,
    pub max_output_bytes: usize,
    /// Outbound `fetch()` allowance for one invocation.
    pub outbound: OutboundPolicy,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_ms: 200,
            memory_bytes: 32 * 1024 * 1024,
            max_output_bytes: 256 * 1024,
            outbound: OutboundPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub headers: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
    /// Captures from the function's declared route path, e.g. `{id}`.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    pub body: String,
}

impl HttpRequest {
    /// A bare POST with a JSON body — the common case in tests and manual invocation.
    pub fn post_json(body: impl Into<String>) -> Self {
        Self {
            method: "POST".into(),
            headers: BTreeMap::new(),
            query: BTreeMap::new(),
            params: BTreeMap::new(),
            body: body.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Outcome {
    /// Handler completed; payload is the response string.
    Success(String),
    /// A resource limit was enforced; payload names the limit.
    Terminated(String),
    /// The script threw or failed to load; payload is the message.
    Error(String),
}

#[derive(Debug, Clone)]
pub struct InvocationResult {
    pub outcome: Outcome,
    /// Explicit content type when the handler used `context.json`/`context.text`;
    /// None for bare returns (the server may sniff).
    pub content_type: Option<String>,
    /// Status the handler asked for, already validated.
    pub status: Option<u16>,
    /// Response headers the handler set, already vetted.
    pub headers: BTreeMap<String, String>,
    pub logs: Vec<LogEntry>,
    /// JS stack for a thrown error — owner-facing debugging only; endpoint
    /// callers never see it.
    pub stack: Option<String>,
    /// Total time including engine setup (context creation, parse, compile).
    pub wall: Duration,
    pub cpu: Duration,
    /// Pure handler execution: the call and promise settlement, excluding setup.
    /// Zero when setup itself failed.
    pub exec_wall: Duration,
    /// Outbound `fetch()` calls this invocation attempted.
    pub outbound_used: u32,
}

/// Deployment intent a script declares about itself via `export const http`.
/// Unknown keys are rejected so typos fail at verify time instead of silently
/// deploying with defaults.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub methods: Option<Vec<String>>,
    #[serde(default)]
    pub path: Option<String>,
    /// Callable without an API key even when the server requires auth —
    /// what an OAuth callback or webhook target needs, since the third
    /// party calling it cannot present a key.
    #[serde(default)]
    pub public: bool,
}

/// One tool's deploy-time metadata. At inspect time the handler function is
/// checked for presence on the same snapshot this was serialized from, then
/// dropped by JSON serialization (functions don't survive JSON.stringify) —
/// execution looks the handler up again in the live module at call time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub public: bool,
    #[serde(default)]
    pub tools: std::collections::BTreeMap<String, ToolConfig>,
}

/// What a module declares itself to be. The export name is the declaration:
/// `http` (or nothing) needs a default handler; `mcp` forbids one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Surface {
    Http(HttpConfig),
    Mcp(McpConfig),
}

/// Runtime needs a module declares via `export const config`, independent of
/// which surface it serves: secret names the host must decrypt into
/// `context.env`, durable state, and object-storage bindings. Unknown keys are
/// rejected so typos fail at verify time instead of silently deploying without.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Durable JSON state (`context.state`). Optional, and must be exactly
    /// `true` when present — `state: false` is a contradiction worth refusing.
    #[serde(default)]
    pub state: Option<bool>,
    /// Object-storage bindings (`context.objects.<NAME>`), keyed by binding
    /// name.
    #[serde(default)]
    pub objects: std::collections::BTreeMap<String, ObjectBinding>,
}

impl RuntimeConfig {
    pub fn wants_state(&self) -> bool {
        self.state == Some(true)
    }
}

/// One declared object-storage binding: where it points and which of the
/// owner's secrets hold the credentials. The credentials themselves are
/// resolved host-side per invocation and never reach JavaScript, stored
/// metadata, or `context.env`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ObjectBinding {
    /// Exact origin of the S3-compatible endpoint, e.g.
    /// `https://<account>.r2.cloudflarestorage.com`. No path, query or
    /// fragment — anything more specific belongs to the bucket and keys.
    pub endpoint: String,
    #[serde(default = "default_region")]
    pub region: String,
    pub bucket: String,
    /// Upper bound on a single object, enforced before a PUT is signed.
    pub max_object_bytes: u64,
    /// Secret-vault entry holding the access key id.
    pub access_key_id_secret: String,
    /// Secret-vault entry holding the secret access key.
    pub secret_access_key_secret: String,
}

fn default_region() -> String {
    "auto".to_string()
}

/// S3's own ceiling for a single non-multipart PUT.
pub const MAX_OBJECT_BYTES_CEILING: u64 = 5 * 1024 * 1024 * 1024;
/// Bindings per module — a bound, like tools and secrets.
pub const MAX_OBJECT_BINDINGS: usize = 8;

/// Binding names look like environment variables: `[A-Z][A-Z0-9_]{0,63}`.
pub fn valid_binding_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// An exact origin: scheme://host[:port], nothing after. The deploy-time
/// allowlist compares against exactly this shape, so anything looser is
/// refused before it can be compared wrongly.
pub fn valid_endpoint_origin(endpoint: &str) -> bool {
    let rest = match endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
    {
        Some(rest) => rest,
        None => return false,
    };
    let (host, port) = match rest.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (rest, None),
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return false;
    }
    match port {
        None => true,
        Some(port) => {
            !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

fn vet_runtime_config(config: &RuntimeConfig) -> Result<(), String> {
    vet_secrets(&config.secrets)?;
    if config.state == Some(false) {
        return Err("config.state must be exactly true when present — omit it instead".to_string());
    }
    if config.objects.len() > MAX_OBJECT_BINDINGS {
        return Err(format!(
            "too many object bindings: {} (max {MAX_OBJECT_BINDINGS})",
            config.objects.len()
        ));
    }
    for (name, binding) in &config.objects {
        if !valid_binding_name(name) {
            return Err(format!(
                "invalid object binding name {name:?}: 1-64 chars of A-Z, 0-9, '_', starting with a letter"
            ));
        }
        if !valid_endpoint_origin(&binding.endpoint) {
            return Err(format!(
                "binding {name}: endpoint must be an exact origin like https://host or https://host:port"
            ));
        }
        if !(3..=63).contains(&binding.bucket.len())
            || !binding
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        {
            return Err(format!(
                "binding {name}: bucket names are 3-63 chars of a-z, 0-9, '.', '-'"
            ));
        }
        if binding.max_object_bytes == 0 || binding.max_object_bytes > MAX_OBJECT_BYTES_CEILING {
            return Err(format!(
                "binding {name}: maxObjectBytes must be between 1 and {MAX_OBJECT_BYTES_CEILING} (5 GiB)"
            ));
        }
        for secret in [
            &binding.access_key_id_secret,
            &binding.secret_access_key_secret,
        ] {
            if !valid_secret_name(secret) {
                return Err(format!(
                    "binding {name}: {secret:?} is not a valid secret name"
                ));
            }
            // Storage credentials are used by the host binding only; letting
            // the module also read them via context.env would defeat the
            // point of never placing them in JavaScript.
            if config.secrets.contains(secret) {
                return Err(format!(
                    "binding {name}: {secret} is a storage credential and cannot also be listed in config.secrets"
                ));
            }
        }
    }
    Ok(())
}

/// Which declared capabilities the host is actually supplying for one
/// invocation. The glue attaches `context.state` / `context.objects.<NAME>`
/// for exactly these and nothing else, so an undeclared capability is absent
/// rather than present-but-broken.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Capabilities {
    pub state: bool,
    pub objects: Vec<String>,
}

impl Capabilities {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn from_config(config: &RuntimeConfig) -> Self {
        Self {
            state: config.wants_state(),
            objects: config.objects.keys().cloned().collect(),
        }
    }

    fn to_glue_json(&self) -> String {
        if !self.state && self.objects.is_empty() {
            return String::new();
        }
        serde_json::json!({ "state": self.state, "objects": self.objects }).to_string()
    }
}

/// Everything [`Executor::inspect`] learns about a module: the surface it
/// declares and the runtime config that applies to either surface.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspection {
    pub surface: Surface,
    pub config: RuntimeConfig,
}

pub const MAX_TOOLS: usize = 32;
/// Enough for any sane handler; a bound so a module cannot demand the host
/// decrypt an unbounded list per invocation.
pub const MAX_SECRETS: usize = 32;

/// Environment-variable shape: uppercase letters, digits and underscores, not
/// starting with a digit. Shared with the server's secret store so a name that
/// deploys is a name that can be set.
pub fn valid_secret_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn vet_secrets(secrets: &[String]) -> Result<(), String> {
    if secrets.len() > MAX_SECRETS {
        return Err(format!(
            "too many secrets: {} (max {MAX_SECRETS})",
            secrets.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for name in secrets {
        if !valid_secret_name(name) {
            return Err(format!(
                "invalid secret name {name:?}: 1-64 chars of A-Z, 0-9, '_', not starting with a digit"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("secret {name} is listed twice"));
        }
    }
    Ok(())
}

pub trait Executor: Send + Sync {
    fn execute(&self, source: &str, request: &HttpRequest, limits: &Limits) -> InvocationResult;

    /// Parse + compile + check the default export, without invoking. Err is a
    /// human-readable compile/shape error.
    fn verify(&self, source: &str) -> Result<(), String>;

    /// Like [`Executor::verify`], but reads which surface the module declares
    /// (`export const http` / `export const mcp`), its configuration, and the
    /// runtime config (`export const config`).
    fn inspect(&self, source: &str) -> Result<Inspection, String>;
}

/// Installed before module evaluation so top-level `console.log` works and is
/// captured. Log entries are capped at 100 × 1KB.
const FETCH_PRELUDE: &str = r#"(() => {
  globalThis.fetch = async (url, init) => {
    const raw = await globalThis.__rustedFetch(JSON.stringify({
      url: String(url),
      method: init && init.method,
      headers: (init && init.headers) || {},
      body: init && init.body != null ? String(init.body) : null,
    }));
    const r = JSON.parse(raw);
    if (r.error) throw new Error(r.error);
    return {
      ok: r.ok,
      status: r.status,
      headers: r.headers,
      text: async () => r.body,
      json: async () => JSON.parse(r.body),
    };
  };
})()"#;

/// `context.inbox` is attached by the glue; this defines what it calls.
const INBOX_PRELUDE: &str = r#"(() => {
  globalThis.__rustedInbox = {
    get: async (name) => {
      const raw = await globalThis.__rustedInboxGet(String(name));
      const r = JSON.parse(raw);
      if (r.error) throw new Error(r.error);
      return r.messages;
    },
  };
})()"#;

/// Builds the `context.state` / `context.objects` views over the host natives,
/// shared by [`GLUE`] and [`TOOL_GLUE`] so the two cannot drift. An empty caps
/// string — undeclared capabilities, ad-hoc runs — yields undefined for both.
const CAPS_PRELUDE: &str = r#"(() => {
  globalThis.__rustedSealApi = () => {
    if (!globalThis.__rustedSealOp) return { seal: undefined, open: undefined };
    const call = async (op) => {
      const r = JSON.parse(await globalThis.__rustedSealOp(JSON.stringify(op)));
      if (r && r.error) throw new Error(r.error);
      return r;
    };
    return {
      seal: async (payload, options) =>
        (await call({ op: "seal", payload, ...(options || {}) })).sealed,
      open: async (sealed, options) => {
        const r = await call({ op: "open", sealed: String(sealed), ...(options || {}) });
        return r.valid ? r.payload : null;
      },
    };
  };
  globalThis.__rustedCaps = (capsJson) => {
    if (!capsJson) return { state: undefined, objects: undefined };
    const caps = JSON.parse(capsJson);
    const parse = (raw) => {
      const r = JSON.parse(raw);
      if (r && r.error) throw new Error(r.error);
      return r;
    };
    const stateOp = async (op) => parse(await globalThis.__rustedStateOp(JSON.stringify(op)));
    const objectOp = async (b, op) => parse(await globalThis.__rustedObjectOp(b, JSON.stringify(op)));
    const state = caps.state ? {
      get: async (key) => (await stateOp({ op: "get", key: String(key) })).entry,
      compareAndSet: (key, expectedVersion, value) =>
        stateOp({ op: "cas", key: String(key), expectedVersion, value }),
      delete: (key, expectedVersion) =>
        stateOp({ op: "delete", key: String(key), expectedVersion }),
      list: (options) => stateOp({ op: "list", ...(options || {}) }),
    } : undefined;
    const objects = caps.objects && caps.objects.length
      ? Object.fromEntries(caps.objects.map((b) => [b, {
          presignPut: (key, options) =>
            objectOp(b, { op: "presignPut", key: String(key), ...(options || {}) }),
          presignGet: (key, options) =>
            objectOp(b, { op: "presignGet", key: String(key), ...(options || {}) }),
          head: async (key) => (await objectOp(b, { op: "head", key: String(key) })).head,
          delete: async (key) => (await objectOp(b, { op: "delete", key: String(key) })).deleted,
          list: (options) => objectOp(b, { op: "list", ...(options || {}) }),
        }]))
      : undefined;
    return { state, objects };
  };
})()"#;

const CONSOLE_PRELUDE: &str = r#"(() => {
  const logs = [];
  const fmt = (a) => { try { return typeof a === "string" ? a : JSON.stringify(a); } catch (_) { return String(a); } };
  const push = (level) => (...args) => {
    if (logs.length >= 100) return;
    let m = args.map(fmt).join(" ");
    if (m.length > 1024) m = m.slice(0, 1024) + "…";
    logs.push({ level, message: m });
  };
  globalThis.console = { log: push("log"), info: push("log"), warn: push("warn"), error: push("error") };
  globalThis.__rustedLogs = logs;
})()"#;

/// Adapts `handler(request, context)` to
/// `(handler, requestJson, envJson, capsJson) -> Promise<envelopeJson>`.
/// The envelope carries the response (or error) plus the logs collected by the
/// console prelude, so the host only ever marshals strings. `envJson` is the
/// decrypted secrets the host chose to lend; empty means `context.env` stays
/// undefined, so a handler that was not granted secrets fails saying so.
/// `capsJson` names the declared capabilities the host is supplying —
/// `context.state` and `context.objects` exist for exactly those.
const GLUE: &str = r#"(handler, requestJson, envJson, capsJson) => {
  const req = JSON.parse(requestJson);
  const logs = globalThis.__rustedLogs || [];
  const request = {
    method: req.method,
    headers: req.headers,
    query: req.query,
    params: req.params || {},
    body: req.body,
    json: async () => JSON.parse(req.body),
    cookies: (() => {
      const jar = {};
      for (const part of String(req.headers.cookie || "").split(";")) {
        const eq = part.indexOf("=");
        if (eq < 0) continue;
        const name = part.slice(0, eq).trim();
        // First occurrence wins, matching how servers conventionally read.
        if (name && !(name in jar)) jar[name] = part.slice(eq + 1).trim();
      }
      return jar;
    })(),
  };
  const respond = (body, contentType, init) => ({
    __rustedResponse: true,
    body,
    contentType,
    status: init && init.status,
    headers: (init && init.headers) || {},
  });
  const caps = globalThis.__rustedCaps(capsJson);
  const sealApi = globalThis.__rustedSealApi();
  const context = {
    json: (o, init) => respond(JSON.stringify(o), "application/json", init),
    text: (s, init) => respond(String(s), "text/plain; charset=utf-8", init),
    // Absent when the host lends no services — reading it then is a clearer
    // failure than a function that silently finds nothing.
    inbox: globalThis.__rustedInbox,
    // Only the secrets the module asked for via `export const config`.
    env: envJson ? JSON.parse(envJson) : undefined,
    // OS-backed CSPRNG — Math.random() must never mint credentials.
    randomBytes: (n) => new Uint8Array(globalThis.__rustedRandomBytes(n)),
    randomBase64Url: (n) => globalThis.__rustedRandomBase64Url(n),
    // Native digest/codec primitives, so credential handling needs neither an
    // npm crypto package nor interpreter-speed loops.
    sha256: (data) => new Uint8Array(globalThis.__rustedSha256(data)),
    toBase64Url: (bytes) => globalThis.__rustedToBase64Url(bytes),
    fromBase64Url: (raw) => new Uint8Array(globalThis.__rustedFromBase64Url(raw)),
    toHex: (bytes) => globalThis.__rustedToHex(bytes),
    fromHex: (raw) => new Uint8Array(globalThis.__rustedFromHex(raw)),
    timingSafeEqual: (a, b) => globalThis.__rustedTimingSafeEqual(a, b),
    // Host-side authenticated encryption keyed by a vault secret; absent
    // where there is no vault to key it from.
    seal: sealApi.seal,
    open: sealApi.open,
    // HTTP ergonomics: the patterns every browser-facing handler repeats.
    redirect: (url, init) => respond("", null, {
      status: (init && init.status) || 302,
      headers: { ...((init && init.headers) || {}), location: String(url) },
    }),
    setCookie: (name, value, options) => {
      const o = options || {};
      const parts = [String(name) + "=" + String(value ?? "")];
      parts.push("Path=" + (o.path || "/"));
      if (o.maxAge !== undefined) parts.push("Max-Age=" + Math.max(0, Math.floor(o.maxAge)));
      if (o.httpOnly !== false) parts.push("HttpOnly");
      parts.push("SameSite=" + (o.sameSite || "Lax"));
      if (o.secure !== false) parts.push("Secure");
      if (o.domain) parts.push("Domain=" + o.domain);
      return parts.join("; ");
    },
    formEncode: (values) => Object.entries(values || {})
      .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(String(v)))
      .join("&"),
    // Present only when declared via `export const config` and supplied by
    // the host — an undeclared capability is absent, not broken.
    state: caps.state,
    objects: caps.objects,
  };
  return Promise.resolve(handler(request, context)).then(
    (r) => {
      let body, contentType = null, status = null, headers = {};
      if (r !== null && typeof r === "object" && r.__rustedResponse === true) {
        body = String(r.body);
        contentType = r.contentType;
        status = r.status ?? null;
        headers = r.headers || {};
      } else if (typeof r === "string") {
        body = r;
      } else {
        body = JSON.stringify(r === undefined ? null : r);
      }
      return JSON.stringify({ ok: true, response: body, contentType, status, headers, logs });
    },
    (e) => JSON.stringify({
      ok: false,
      error: e instanceof Error ? e.message : String(e),
      stack: e && e.stack ? String(e.stack) : null,
      logs,
    })
  );
}"#;

/// Adapts one mcp tool to `(namespace, toolName, argsJson, envJson) -> Promise<envelopeJson>`.
/// The handler is looked up in the live module namespace at call time — inspect
/// checked a snapshot at deploy time, but what executes is whatever the module
/// evaluates to now, so the `typeof` re-check here is the invariant that holds.
/// Emits the same envelope shape as [`GLUE`] so `outcome_from_envelope` reads
/// both. A tool returns a value, not an HTTP response, so the context has no
/// `json`/`text` helpers and the content type is decided by the value's shape.
const TOOL_GLUE: &str = r#"(ns, toolName, argsJson, envJson, capsJson) => {
  const logs = globalThis.__rustedLogs || [];
  const tool = ns.mcp && ns.mcp.tools ? ns.mcp.tools[toolName] : undefined;
  const caps = globalThis.__rustedCaps(capsJson);
  const sealApi = globalThis.__rustedSealApi();
  const context = {
    // Absent when the host lends no services — reading it then is a clearer
    // failure than a function that silently finds nothing.
    inbox: globalThis.__rustedInbox,
    // Only the secrets the module asked for via `export const config`.
    env: envJson ? JSON.parse(envJson) : undefined,
    // OS-backed CSPRNG — Math.random() must never mint credentials.
    randomBytes: (n) => new Uint8Array(globalThis.__rustedRandomBytes(n)),
    randomBase64Url: (n) => globalThis.__rustedRandomBase64Url(n),
    // Native digest/codec primitives, so credential handling needs neither an
    // npm crypto package nor interpreter-speed loops.
    sha256: (data) => new Uint8Array(globalThis.__rustedSha256(data)),
    toBase64Url: (bytes) => globalThis.__rustedToBase64Url(bytes),
    fromBase64Url: (raw) => new Uint8Array(globalThis.__rustedFromBase64Url(raw)),
    toHex: (bytes) => globalThis.__rustedToHex(bytes),
    fromHex: (raw) => new Uint8Array(globalThis.__rustedFromHex(raw)),
    timingSafeEqual: (a, b) => globalThis.__rustedTimingSafeEqual(a, b),
    // Host-side authenticated encryption keyed by a vault secret; absent
    // where there is no vault to key it from.
    seal: sealApi.seal,
    open: sealApi.open,
    formEncode: (values) => Object.entries(values || {})
      .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(String(v)))
      .join("&"),
    // Present only when declared via `export const config` and supplied by
    // the host — an undeclared capability is absent, not broken.
    state: caps.state,
    objects: caps.objects,
  };
  return Promise.resolve()
    .then(() => {
      if (!tool || typeof tool.handler !== "function")
        throw new Error("unknown tool: " + toolName);
      return tool.handler(JSON.parse(argsJson), context);
    })
    .then((value) => {
      let body, contentType;
      if (typeof value === "string") {
        body = value;
        contentType = "text/plain";
      } else {
        body = value === undefined ? "null" : JSON.stringify(value);
        contentType = "application/json";
      }
      return JSON.stringify({ ok: true, response: body, contentType, status: null, headers: {}, logs });
    })
    .catch((e) => JSON.stringify({
      ok: false,
      error: e instanceof Error ? e.message : String(e),
      stack: e && e.stack ? String(e.stack) : null,
      logs,
    }));
}"#;

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    response: String,
    #[serde(default, rename = "contentType")]
    content_type: Option<String>,
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    error: String,
    #[serde(default)]
    stack: Option<String>,
    #[serde(default)]
    logs: Vec<LogEntry>,
}

/// Compiled modules, keyed by the sha256 of their source. Parsing a large
/// bundle dominates a cold invocation (~3.5 ms for 100 KB), and the bytecode
/// is context-independent, so it is produced once and loaded into each fresh
/// context. Memory-only and process-local: bytecode is tied to this exact
/// QuickJS build, so it must never outlive the binary that made it.
#[derive(Default)]
struct BytecodeCache {
    entries: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

/// Bounded so a workload of many distinct scripts can't grow it without limit.
const BYTECODE_CACHE_CAP: usize = 512;

pub struct QuickJsExecutor {
    bytecode: BytecodeCache,
}

impl QuickJsExecutor {
    pub fn new() -> Self {
        Self {
            bytecode: BytecodeCache::default(),
        }
    }

    /// Compiled form of `source`, compiling and caching it on first sight.
    fn bytecode_for(&self, source: &str) -> Result<Arc<Vec<u8>>, String> {
        let key = hex::encode(sha2::Sha256::digest(source.as_bytes()));
        if let Some(hit) = self.bytecode.entries.lock().unwrap().get(&key) {
            return Ok(hit.clone());
        }
        let compiled = Arc::new(compile(source)?);
        let mut entries = self.bytecode.entries.lock().unwrap();
        if entries.len() >= BYTECODE_CACHE_CAP {
            entries.clear();
        }
        entries.insert(key, compiled.clone());
        Ok(compiled)
    }
}

/// Parses `source` in a throwaway context and returns its bytecode.
fn compile(source: &str) -> Result<Vec<u8>, String> {
    let rt = Runtime::new().expect("quickjs runtime");
    let ctx = Context::full(&rt).expect("quickjs context");
    ctx.with(|c| {
        let declared =
            Module::declare(c.clone(), "handler", source).map_err(|e| exception_message(&c, e))?;
        declared
            .write(Default::default())
            .map_err(|e| exception_message(&c, e))
    })
}

impl Default for QuickJsExecutor {
    fn default() -> Self {
        Self::new()
    }
}

fn exception_message(ctx: &Ctx<'_>, e: rquickjs::Error) -> String {
    match e {
        rquickjs::Error::Exception => {
            let v = ctx.catch();
            if let Some(ex) = v.as_exception() {
                ex.message().unwrap_or_else(|| format!("{ex:?}"))
            } else if let Some(s) = v.as_string() {
                s.to_string().unwrap_or_default()
            } else {
                format!("{v:?}")
            }
        }
        other => other.to_string(),
    }
}

/// What the handler asked the response to look like, beyond its body.
#[derive(Debug, Default, Clone)]
struct Response {
    content_type: Option<String>,
    status: Option<u16>,
    headers: BTreeMap<String, String>,
}

/// Headers a handler must never set: these frame the response itself, and
/// letting a function rewrite them invites smuggling and corrupt replies.
const RESERVED_HEADERS: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "host",
];

/// Enough for anything reasonable; a bound so a handler can't grow the
/// response headers without limit.
const MAX_HEADERS: usize = 32;
const MAX_HEADER_NAME: usize = 64;
const MAX_HEADER_VALUE: usize = 1024;

/// Checks what the handler asked for. Returning an error rather than silently
/// dropping it: a status of 999 is a bug, and a bug that vanishes is worse
/// than one that says so.
fn vet_response(
    status: Option<u16>,
    headers: BTreeMap<String, String>,
) -> Result<(Option<u16>, BTreeMap<String, String>), String> {
    if let Some(status) = status {
        if !(200..=599).contains(&status) {
            return Err(format!("invalid response status {status}: use 200 to 599"));
        }
    }
    if headers.len() > MAX_HEADERS {
        return Err(format!(
            "too many response headers: {} (limit {MAX_HEADERS})",
            headers.len()
        ));
    }
    let mut vetted = BTreeMap::new();
    for (name, value) in headers {
        let lower = name.trim().to_ascii_lowercase();
        if lower.is_empty() || lower.len() > MAX_HEADER_NAME {
            return Err(format!("invalid response header name: {name:?}"));
        }
        // A newline in either half would let a handler inject a second header
        // or split the response.
        if !lower
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!("invalid response header name: {name:?}"));
        }
        if RESERVED_HEADERS.contains(&lower.as_str()) {
            return Err(format!(
                "{lower} is set by the platform and cannot be overridden"
            ));
        }
        if value.len() > MAX_HEADER_VALUE {
            return Err(format!("response header {lower} is too long"));
        }
        if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(format!("response header {lower} contains a line break"));
        }
        vetted.insert(lower, value);
    }
    Ok((status, vetted))
}

/// Engine-level failures that mean "a limit fired" rather than "the script is
/// wrong". `expired` says whether the wall deadline passed, which is the only
/// way to read an unsettled promise correctly.
fn classify(msg: String) -> Outcome {
    classify_with(msg, false)
}

/// A failure QuickJS could not describe: no message and no stack.
///
/// Exhausting the heap leaves nothing to build an `Error` with, so the thrown
/// value arrives as a bare `null` and the cause is lost. Reporting that
/// verbatim gives a developer nothing to act on.
///
/// Asking the allocator afterwards does not work — unwinding frees the heap
/// before the outcome is inspected, and it reads ~110 KB of 32 MB. A tracking
/// allocator would give a true high-water mark, but that means `unsafe` code on
/// every allocation in a runtime that executes other people's scripts, which is
/// too much risk for an error message.
///
/// So this matches the signature. Throwing a non-`Error` value produces the
/// same shape, which the wording acknowledges rather than papering over.
fn is_valueless_failure(msg: &str, stack: Option<&String>) -> bool {
    stack.is_none() && matches!(msg.trim(), "null" | "undefined" | "")
}

/// Turns a settled handler into an outcome.
///
/// Shared by the blocking and async executors on purpose: this is where the
/// output cap, header vetting and rejection classification live, and two copies
/// would drift. Callers hand in `Ok(envelope_json)` or `Err(message)`, having
/// already turned any engine error into text.
#[allow(clippy::type_complexity)]
fn outcome_from_envelope(
    finished: Result<String, String>,
    expired: bool,
    limits: &Limits,
) -> (Outcome, Response, Vec<LogEntry>, Option<String>) {
    let envelope_json = match finished {
        Ok(envelope_json) => envelope_json,
        Err(message) => {
            return (
                classify_with(message, expired),
                Response::default(),
                Vec::new(),
                None,
            )
        }
    };
    match serde_json::from_str::<Envelope>(&envelope_json) {
        Ok(env) if env.ok => {
            if env.response.len() > limits.max_output_bytes {
                return (
                    Outcome::Terminated(format!(
                        "output limit: {} > {} bytes",
                        env.response.len(),
                        limits.max_output_bytes
                    )),
                    Response::default(),
                    env.logs,
                    None,
                );
            }
            match vet_response(env.status, env.headers) {
                Ok((status, headers)) => (
                    Outcome::Success(env.response),
                    Response {
                        content_type: env.content_type,
                        status,
                        headers,
                    },
                    env.logs,
                    None,
                ),
                Err(message) => (Outcome::Error(message), Response::default(), env.logs, None),
            }
        }
        // Rejections also classify: an uncatchable stack overflow surfaces here
        // as a rejection, and it is a limit, not a bug.
        Ok(env) => (
            classify_with(env.error, expired),
            Response::default(),
            env.logs,
            env.stack,
        ),
        Err(e) => (
            Outcome::Error(format!("malformed envelope: {e}")),
            Response::default(),
            Vec::new(),
            None,
        ),
    }
}

fn classify_with(msg: String, expired: bool) -> Outcome {
    let lower = msg.to_lowercase();
    // rquickjs reports an unsettled promise as a "dead lock". After the
    // deadline that is simply the timeout; before it, the handler awaited
    // something that never resolves — a timeout either way, not a crash.
    if lower.contains("dead lock") || lower.contains("deadlock") {
        return Outcome::Terminated(if expired {
            "wall deadline: the handler was still running when time ran out".to_string()
        } else {
            "the handler never finished: it awaited a promise that never settles".to_string()
        });
    }
    // The engine resolves no modules, so an `import` fails with a bare "could
    // not load module". That names the symptom and not the rule, and a caller
    // sending ad-hoc code — an agent, usually — just tries another package.
    // Deployed functions never see this: the CLI bundles imports away first.
    if lower.contains("could not load module") {
        return Outcome::Error(format!(
            "{msg} — imports are not resolved at runtime. Send self-contained code \
             with the dependency inlined, or deploy with `rusted push`, which bundles \
             imports first. Node built-ins (node:fs, node:crypto) do not exist here at all."
        ));
    }
    if expired || lower.contains("interrupted") {
        Outcome::Terminated(format!("wall deadline: {msg}"))
    } else if lower.contains("out of memory") {
        Outcome::Terminated(format!("memory limit: {msg}"))
    } else if lower.contains("stack overflow") || lower.contains("call stack size exceeded") {
        Outcome::Terminated(format!("stack limit: {msg}"))
    } else {
        Outcome::Error(msg)
    }
}

/// The runtime plus a flag the interrupt handler raises when it fires. An
/// interrupt inside a promise job leaves that promise unsettled, and the
/// engine can only tell that apart from a genuinely stuck promise by asking
/// whether the deadline passed.
/// Things the host lends a handler beyond the network.
///
/// The engine deliberately reaches nothing on its own — no filesystem, no
/// database, no process — so anything more than `fetch` has to be handed in.
/// Passing a trait rather than a connection keeps it that way: the engine can
/// call this and nothing else, and the implementation decides whose data it is
/// allowed to see.
pub trait HostServices: Send + Sync {
    /// Messages waiting in one of the owner's inboxes, as a JSON string.
    ///
    /// Scoping is the caller's job: the implementation is built per invocation
    /// with the function owner baked in, so a handler cannot name its way into
    /// somebody else's inbox.
    fn inbox_get(
        &self,
        name: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>;

    /// Whether this host offers inbox reads at all. When false,
    /// `context.inbox` stays undefined — the local dev server lends state and
    /// objects but has no inbox store, and absent beats present-but-broken.
    fn offers_inbox(&self) -> bool {
        true
    }

    /// One durable-state operation, JSON in (`{"op":"get","key":…}` and
    /// friends), JSON result out. Scoped like the inbox: the implementation
    /// carries the owner and function name, never the request.
    fn state_op(
        &self,
        op_json: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        let _ = op_json;
        Box::pin(async { Err("durable state is not available on this host".to_string()) })
    }

    /// One object-storage operation against a named binding, JSON in and out.
    fn object_op(
        &self,
        binding: String,
        op_json: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        let _ = (binding, op_json);
        Box::pin(async { Err("object storage is not available on this host".to_string()) })
    }

    /// One `context.seal`/`context.open` operation: authenticated encryption
    /// performed host-side, keyed by one of the owner's vault secrets — the
    /// key material never enters JavaScript.
    fn seal_op(
        &self,
        op_json: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        let _ = op_json;
        Box::pin(async { Err("sealing is not available on this host".to_string()) })
    }
}

/// Sums the CPU actually burned by a future.
///
/// `ThreadTime` measures the calling thread, and an async task may be polled on
/// a different thread each time — so timing the whole future would attribute
/// other tasks' work to this one, or miss its own. Each individual `poll` does
/// run start-to-finish on one thread, so measuring per poll and summing is
/// correct where measuring once is not.
struct CpuMetered<F> {
    inner: F,
    cpu: Duration,
}

impl<F: Future> Future for CpuMetered<F> {
    type Output = (F::Output, Duration);

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        // Safety: neither field is moved; `inner` is only projected in place.
        let this = unsafe { self.get_unchecked_mut() };
        let started = cpu_time::ThreadTime::now();
        let polled = unsafe { Pin::new_unchecked(&mut this.inner) }.poll(cx);
        this.cpu += started.elapsed();
        match polled {
            Poll::Ready(out) => Poll::Ready((out, this.cpu)),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn cpu_metered<F: Future>(inner: F) -> CpuMetered<F> {
    CpuMetered {
        inner,
        cpu: Duration::ZERO,
    }
}

/// Attaches `context.inbox` when the host offers it — and the state/object
/// natives the glue exposes only for declared capabilities. Without services
/// none of the globals are defined, so a handler that reaches for them fails
/// saying so.
fn install_services<'js>(ctx: &Ctx<'js>, services: Arc<dyn HostServices>) -> Result<(), String> {
    if services.offers_inbox() {
        let inbox_services = services.clone();
        let native = Function::new(
            ctx.clone(),
            rquickjs::function::Async(move |name: String| {
                let services = inbox_services.clone();
                async move {
                    match services.inbox_get(name).await {
                        Ok(messages) => format!("{{\"messages\":{messages}}}"),
                        Err(e) => serde_json::json!({ "error": e }).to_string(),
                    }
                }
            }),
        )
        .map_err(|e| exception_message(ctx, e))?;
        ctx.globals()
            .set("__rustedInboxGet", native)
            .map_err(|e| exception_message(ctx, e))?;
        ctx.eval::<(), _>(INBOX_PRELUDE)
            .map_err(|e| exception_message(ctx, e))?;
    }

    let state_services = services.clone();
    let state_native = Function::new(
        ctx.clone(),
        rquickjs::function::Async(move |op_json: String| {
            let services = state_services.clone();
            async move {
                match services.state_op(op_json).await {
                    Ok(result) => result,
                    Err(e) => serde_json::json!({ "error": e }).to_string(),
                }
            }
        }),
    )
    .map_err(|e| exception_message(ctx, e))?;
    ctx.globals()
        .set("__rustedStateOp", state_native)
        .map_err(|e| exception_message(ctx, e))?;

    let object_services = services.clone();
    let object_native = Function::new(
        ctx.clone(),
        rquickjs::function::Async(move |binding: String, op_json: String| {
            let services = object_services.clone();
            async move {
                match services.object_op(binding, op_json).await {
                    Ok(result) => result,
                    Err(e) => serde_json::json!({ "error": e }).to_string(),
                }
            }
        }),
    )
    .map_err(|e| exception_message(ctx, e))?;
    ctx.globals()
        .set("__rustedObjectOp", object_native)
        .map_err(|e| exception_message(ctx, e))?;

    let seal_native = Function::new(
        ctx.clone(),
        rquickjs::function::Async(move |op_json: String| {
            let services = services.clone();
            async move {
                match services.seal_op(op_json).await {
                    Ok(result) => result,
                    Err(e) => serde_json::json!({ "error": e }).to_string(),
                }
            }
        }),
    )
    .map_err(|e| exception_message(ctx, e))?;
    ctx.globals()
        .set("__rustedSealOp", seal_native)
        .map_err(|e| exception_message(ctx, e))
}

/// The async twin of [`restricted_runtime`], with the same three limits. They
/// are set through the async API but mean exactly the same thing; the spike in
/// `tests/async_spike.rs` pins that they still fire.
async fn restricted_async_runtime(limits: &Limits) -> (AsyncRuntime, Arc<AtomicBool>) {
    let rt = AsyncRuntime::new().expect("quickjs runtime");
    rt.set_memory_limit(limits.memory_bytes).await;
    rt.set_max_stack_size(256 * 1024).await;
    let deadline = Instant::now() + Duration::from_millis(limits.wall_ms);
    let expired = Arc::new(AtomicBool::new(false));
    let flag = expired.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        if Instant::now() >= deadline {
            flag.store(true, Ordering::Relaxed);
            return true;
        }
        false
    })))
    .await;
    (rt, expired)
}

/// `fetch` that suspends instead of blocking. The JavaScript is identical —
/// `FETCH_PRELUDE` awaits either way — but here the worker thread is released
/// while the network is busy.
fn install_fetch_async<'js>(
    ctx: &Ctx<'js>,
    budget: Arc<outbound::OutboundBudget>,
) -> Result<(), String> {
    let native = Function::new(
        ctx.clone(),
        rquickjs::function::Async(move |payload: String| {
            let budget = budget.clone();
            async move {
                let request: outbound::FetchRequest = match serde_json::from_str(&payload) {
                    Ok(request) => request,
                    Err(e) => {
                        return serde_json::json!({ "error": format!("bad fetch arguments: {e}") })
                            .to_string()
                    }
                };
                serde_json::to_string(&budget.perform_async(request).await).unwrap_or_else(|e| {
                    serde_json::json!({ "error": format!("fetch failed: {e}") }).to_string()
                })
            }
        }),
    )
    .map_err(|e| exception_message(ctx, e))?;
    ctx.globals()
        .set("__rustedFetch", native)
        .map_err(|e| exception_message(ctx, e))?;
    ctx.eval::<(), _>(FETCH_PRELUDE)
        .map_err(|e| exception_message(ctx, e))
}

/// Backs `context.randomBytes` / `context.randomBase64Url` with the OS's
/// CSPRNG. Engine-provided rather than a host service on purpose: randomness
/// has no owner to scope to, so it exists everywhere — local runs included —
/// and QuickJS's `Math.random()` never has to be trusted with OAuth state,
/// PKCE verifiers, or nonces.
fn install_random(ctx: &Ctx<'_>) -> Result<(), String> {
    fn fill(ctx: &Ctx<'_>, len: f64) -> rquickjs::Result<Vec<u8>> {
        // Bounded because every byte is a syscall-backed allocation; 1024 is
        // far beyond any nonce, state, or key a handler legitimately mints.
        if len.fract() != 0.0 || !(1.0..=1024.0).contains(&len) {
            return Err(Exception::throw_message(
                ctx,
                "random length must be an integer from 1 to 1024",
            ));
        }
        let mut buf = vec![0u8; len as usize];
        getrandom::fill(&mut buf).expect("the OS random source is available");
        Ok(buf)
    }
    let bytes = Function::new(
        ctx.clone(),
        |ctx: Ctx<'_>, len: f64| -> rquickjs::Result<Vec<u8>> { fill(&ctx, len) },
    )
    .map_err(|e| exception_message(ctx, e))?;
    ctx.globals()
        .set("__rustedRandomBytes", bytes)
        .map_err(|e| exception_message(ctx, e))?;
    let base64url = Function::new(
        ctx.clone(),
        |ctx: Ctx<'_>, len: f64| -> rquickjs::Result<String> {
            use base64::Engine as _;
            fill(&ctx, len).map(|buf| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
        },
    )
    .map_err(|e| exception_message(ctx, e))?;
    ctx.globals()
        .set("__rustedRandomBase64Url", base64url)
        .map_err(|e| exception_message(ctx, e))
}

/// The bytes a codec native was handed: a UTF-8 string or a Uint8Array.
/// Anything else is the caller's bug, named rather than coerced.
fn value_bytes(ctx: &Ctx<'_>, value: &Value<'_>) -> rquickjs::Result<Vec<u8>> {
    if let Some(s) = value.as_string() {
        return Ok(s.to_string()?.into_bytes());
    }
    if let Some(object) = value.as_object() {
        if let Ok(array) = rquickjs::TypedArray::<u8>::from_object(object.clone()) {
            if let Some(bytes) = array.as_bytes() {
                return Ok(bytes.to_vec());
            }
        }
    }
    Err(Exception::throw_message(
        ctx,
        "expected a string or Uint8Array",
    ))
}

/// Backs `context.sha256`, the base64url/hex codecs, and `timingSafeEqual` —
/// the primitives every credential-handling function otherwise imports an npm
/// package (and pays interpreter time) for. Engine-provided, like randomness:
/// present everywhere, local runs included.
fn install_codec<'js>(ctx: &Ctx<'js>) -> Result<(), String> {
    use base64::Engine as _;
    let set = |name: &str, f: Function<'js>| -> Result<(), String> {
        ctx.globals()
            .set(name, f)
            .map_err(|e| exception_message(ctx, e))
    };
    let sha256 = Function::new(
        ctx.clone(),
        |ctx: Ctx<'_>, value: Value<'_>| -> rquickjs::Result<Vec<u8>> {
            Ok(sha2::Sha256::digest(value_bytes(&ctx, &value)?).to_vec())
        },
    )
    .map_err(|e| exception_message(ctx, e))?;
    set("__rustedSha256", sha256)?;

    let to_b64u = Function::new(
        ctx.clone(),
        |ctx: Ctx<'_>, value: Value<'_>| -> rquickjs::Result<String> {
            Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value_bytes(&ctx, &value)?))
        },
    )
    .map_err(|e| exception_message(ctx, e))?;
    set("__rustedToBase64Url", to_b64u)?;

    let from_b64u = Function::new(
        ctx.clone(),
        |ctx: Ctx<'_>, raw: String| -> rquickjs::Result<Vec<u8>> {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(raw.as_bytes())
                .map_err(|_| Exception::throw_message(&ctx, "invalid base64url"))
        },
    )
    .map_err(|e| exception_message(ctx, e))?;
    set("__rustedFromBase64Url", from_b64u)?;

    let to_hex = Function::new(
        ctx.clone(),
        |ctx: Ctx<'_>, value: Value<'_>| -> rquickjs::Result<String> {
            Ok(hex::encode(value_bytes(&ctx, &value)?))
        },
    )
    .map_err(|e| exception_message(ctx, e))?;
    set("__rustedToHex", to_hex)?;

    let from_hex = Function::new(
        ctx.clone(),
        |ctx: Ctx<'_>, raw: String| -> rquickjs::Result<Vec<u8>> {
            hex::decode(raw.as_bytes()).map_err(|_| Exception::throw_message(&ctx, "invalid hex"))
        },
    )
    .map_err(|e| exception_message(ctx, e))?;
    set("__rustedFromHex", from_hex)?;

    // Length-independent by construction: the digests are compared, not the
    // inputs, so neither content nor length leaks through timing — which an
    // interpreted-JS "constant time" loop cannot actually promise.
    let timing_safe = Function::new(
        ctx.clone(),
        |ctx: Ctx<'_>, a: Value<'_>, b: Value<'_>| -> rquickjs::Result<bool> {
            let (a, b) = (value_bytes(&ctx, &a)?, value_bytes(&ctx, &b)?);
            let (da, db) = (sha2::Sha256::digest(&a), sha2::Sha256::digest(&b));
            let mut diff = (a.len() ^ b.len()) as u8;
            for (x, y) in da.iter().zip(db.iter()) {
                diff |= x ^ y;
            }
            Ok(diff == 0)
        },
    )
    .map_err(|e| exception_message(ctx, e))?;
    set("__rustedTimingSafeEqual", timing_safe)
}

fn restricted_runtime(limits: &Limits) -> (Runtime, Arc<AtomicBool>) {
    let rt = Runtime::new().expect("quickjs runtime");
    rt.set_memory_limit(limits.memory_bytes);
    rt.set_max_stack_size(256 * 1024);
    let deadline = Instant::now() + Duration::from_millis(limits.wall_ms);
    let expired = Arc::new(AtomicBool::new(false));
    let flag = expired.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        if Instant::now() >= deadline {
            flag.store(true, Ordering::Relaxed);
            return true;
        }
        false
    })));
    (rt, expired)
}

/// Evaluates the module and returns its default export, which must be a function.
/// The console prelude runs first so top-level `console.log` works.
fn install_fetch<'js>(ctx: &Ctx<'js>, budget: Arc<outbound::OutboundBudget>) -> Result<(), String> {
    let native = Function::new(ctx.clone(), move |payload: String| -> String {
        let request: outbound::FetchRequest = match serde_json::from_str(&payload) {
            Ok(request) => request,
            Err(e) => {
                return serde_json::json!({ "error": format!("bad fetch arguments: {e}") })
                    .to_string()
            }
        };
        serde_json::to_string(&budget.perform(request)).unwrap_or_else(|e| {
            serde_json::json!({ "error": format!("fetch failed: {e}") }).to_string()
        })
    })
    .map_err(|e| exception_message(ctx, e))?;
    ctx.globals()
        .set("__rustedFetch", native)
        .map_err(|e| exception_message(ctx, e))?;
    ctx.eval::<(), _>(FETCH_PRELUDE)
        .map_err(|e| exception_message(ctx, e))
}

/// Evaluates the module without demanding any particular exports. The surface
/// checks (default handler for http, tools for mcp) are the caller's business.
fn load_module_raw<'js>(
    ctx: &Ctx<'js>,
    source: &str,
    bytecode: Option<&[u8]>,
) -> Result<Module<'js, rquickjs::module::Evaluated>, String> {
    ctx.eval::<(), _>(CONSOLE_PRELUDE)
        .map_err(|e| exception_message(ctx, e))?;
    ctx.eval::<(), _>(CAPS_PRELUDE)
        .map_err(|e| exception_message(ctx, e))?;
    // Before evaluation, so top-level code can already draw randomness. This
    // is the choke point every path — execute, tools, verify, inspect — goes
    // through, which is what keeps the capability universally present.
    install_random(ctx)?;
    install_codec(ctx)?;
    let declared = match bytecode {
        // SAFETY: the bytes came from `compile` in this same process, so the
        // QuickJS build that reads them is the one that wrote them.
        Some(bytes) => {
            unsafe { Module::load(ctx.clone(), bytes) }.map_err(|e| exception_message(ctx, e))?
        }
        None => Module::declare(ctx.clone(), "handler", source)
            .map_err(|e| exception_message(ctx, e))?,
    };
    let (module, progress) = declared.eval().map_err(|e| exception_message(ctx, e))?;
    progress
        .finish::<()>()
        .map_err(|e| exception_message(ctx, e))?;
    Ok(module)
}

/// The default export, which must exist and be a function.
fn default_handler<'js>(
    module: &Module<'js, rquickjs::module::Evaluated>,
) -> Result<Value<'js>, String> {
    let handler: Value = module
        .get("default")
        .map_err(|_| "module has no default export".to_string())?;
    if !handler.is_function() {
        return Err("default export is not a function".to_string());
    }
    Ok(handler)
}

fn load_module<'js>(
    ctx: &Ctx<'js>,
    source: &str,
    bytecode: Option<&[u8]>,
) -> Result<(Module<'js, rquickjs::module::Evaluated>, Value<'js>), String> {
    let module = load_module_raw(ctx, source, bytecode)?;
    let handler = default_handler(&module)?;
    Ok((module, handler))
}

fn load_handler<'js>(
    ctx: &Ctx<'js>,
    source: &str,
    bytecode: Option<&[u8]>,
) -> Result<Value<'js>, String> {
    load_module(ctx, source, bytecode).map(|(_, handler)| handler)
}

impl QuickJsExecutor {
    /// Runs a handler without holding the worker thread while it waits.
    ///
    /// Same limits, same guards, same envelope handling as [`Executor::execute`]
    /// — the difference is only that `fetch` suspends instead of blocking. That
    /// matters because a blocking fetch runs no bytecode, so QuickJS's interrupt
    /// cannot fire and the slot is held for the whole round trip: a handler
    /// awaiting a 1-second API capped a 2-core server at 15 requests a second
    /// with 85% of the CPU idle.
    pub async fn execute_async(
        &self,
        source: &str,
        request: &HttpRequest,
        limits: &Limits,
    ) -> InvocationResult {
        self.execute_with_services(source, request, limits, None, None, &Capabilities::none())
            .await
    }

    /// As [`Self::execute_async`], with whatever the host chooses to lend the
    /// handler. `None` services means only `fetch`, which is what tests and
    /// local development get. `env` is the decrypted secrets the module asked
    /// for; `None` leaves `context.env` undefined. `caps` names the declared
    /// capabilities being supplied — `context.state` / `context.objects`
    /// appear for exactly those.
    pub async fn execute_with_services(
        &self,
        source: &str,
        request: &HttpRequest,
        limits: &Limits,
        services: Option<Arc<dyn HostServices>>,
        env: Option<&BTreeMap<String, String>>,
        caps: &Capabilities,
    ) -> InvocationResult {
        let env_json = env
            .map(|env| serde_json::to_string(env).expect("serialize env"))
            .unwrap_or_default();
        let caps_json = caps.to_glue_json();
        let wall0 = Instant::now();
        let (rt, expired) = restricted_async_runtime(limits).await;
        let ctx = AsyncContext::full(&rt).await.expect("quickjs context");
        let request_json = serde_json::to_string(request).expect("serialize request");
        let bytecode = self.bytecode_for(source).ok();
        // Fetches share the invocation's budget, so exec_ms bounds total wall
        // time and not merely the JavaScript.
        let deadline = wall0 + Duration::from_millis(limits.wall_ms);
        let budget = Arc::new(outbound::OutboundBudget::with_deadline(
            limits.outbound.clone(),
            deadline,
        ));

        let body = ctx.async_with(async |c| {
            let zero = Duration::ZERO;
            if let Err(msg) = install_fetch_async(&c, budget.clone()) {
                return (
                    Outcome::Error(msg),
                    Response::default(),
                    Vec::new(),
                    None,
                    zero,
                );
            }
            if let Some(services) = services.clone() {
                if let Err(msg) = install_services(&c, services) {
                    return (
                        Outcome::Error(msg),
                        Response::default(),
                        Vec::new(),
                        None,
                        zero,
                    );
                }
            }
            let handler = match load_handler(&c, source, bytecode.as_deref().map(|b| b.as_slice()))
            {
                Ok(handler) => handler,
                Err(msg) => return (classify(msg), Response::default(), Vec::new(), None, zero),
            };
            let glue: Function = match c.eval(GLUE) {
                Ok(glue) => glue,
                Err(e) => {
                    return (
                        classify(exception_message(&c, e)),
                        Response::default(),
                        Vec::new(),
                        None,
                        zero,
                    )
                }
            };
            let exec0 = Instant::now();
            let promise: Promise = match glue.call((
                handler,
                request_json.as_str(),
                env_json.as_str(),
                caps_json.as_str(),
            )) {
                Ok(promise) => promise,
                Err(e) => {
                    return (
                        classify(exception_message(&c, e)),
                        Response::default(),
                        Vec::new(),
                        None,
                        exec0.elapsed(),
                    )
                }
            };
            let (outcome, response, logs, stack) =
                settle_with_deadline(&c, promise, deadline, &expired, limits).await;
            (outcome, response, logs, stack, exec0.elapsed())
        });

        let (parts, cpu) = cpu_metered(body).await;
        assemble_result(parts, cpu, wall0, limits, &budget)
    }

    /// Runs one mcp tool as a one-shot invocation: same runtime, limits, and
    /// envelope machinery as [`Self::execute_with_services`], but the module is
    /// loaded without demanding a default export and [`TOOL_GLUE`] resolves the
    /// named tool's handler in the live namespace instead.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_tool_with_services(
        &self,
        source: &str,
        tool: &str,
        args: &serde_json::Value,
        limits: &Limits,
        services: Option<Arc<dyn HostServices>>,
        env: Option<&BTreeMap<String, String>>,
        caps: &Capabilities,
    ) -> InvocationResult {
        let env_json = env
            .map(|env| serde_json::to_string(env).expect("serialize env"))
            .unwrap_or_default();
        let caps_json = caps.to_glue_json();
        let wall0 = Instant::now();
        let (rt, expired) = restricted_async_runtime(limits).await;
        let ctx = AsyncContext::full(&rt).await.expect("quickjs context");
        let args_json = serde_json::to_string(args).expect("serialize args");
        let bytecode = self.bytecode_for(source).ok();
        // Fetches share the invocation's budget, so exec_ms bounds total wall
        // time and not merely the JavaScript.
        let deadline = wall0 + Duration::from_millis(limits.wall_ms);
        let budget = Arc::new(outbound::OutboundBudget::with_deadline(
            limits.outbound.clone(),
            deadline,
        ));

        let body = ctx.async_with(async |c| {
            let zero = Duration::ZERO;
            if let Err(msg) = install_fetch_async(&c, budget.clone()) {
                return (
                    Outcome::Error(msg),
                    Response::default(),
                    Vec::new(),
                    None,
                    zero,
                );
            }
            if let Some(services) = services.clone() {
                if let Err(msg) = install_services(&c, services) {
                    return (
                        Outcome::Error(msg),
                        Response::default(),
                        Vec::new(),
                        None,
                        zero,
                    );
                }
            }
            let module =
                match load_module_raw(&c, source, bytecode.as_deref().map(|b| b.as_slice())) {
                    Ok(module) => module,
                    Err(msg) => {
                        return (classify(msg), Response::default(), Vec::new(), None, zero)
                    }
                };
            let ns = match module.namespace() {
                Ok(ns) => ns,
                Err(e) => {
                    return (
                        classify(exception_message(&c, e)),
                        Response::default(),
                        Vec::new(),
                        None,
                        zero,
                    )
                }
            };
            let glue: Function = match c.eval(TOOL_GLUE) {
                Ok(glue) => glue,
                Err(e) => {
                    return (
                        classify(exception_message(&c, e)),
                        Response::default(),
                        Vec::new(),
                        None,
                        zero,
                    )
                }
            };
            let exec0 = Instant::now();
            let promise: Promise = match glue.call((
                ns,
                tool,
                args_json.as_str(),
                env_json.as_str(),
                caps_json.as_str(),
            )) {
                Ok(promise) => promise,
                Err(e) => {
                    return (
                        classify(exception_message(&c, e)),
                        Response::default(),
                        Vec::new(),
                        None,
                        exec0.elapsed(),
                    )
                }
            };
            let (outcome, response, logs, stack) =
                settle_with_deadline(&c, promise, deadline, &expired, limits).await;
            (outcome, response, logs, stack, exec0.elapsed())
        });

        let (parts, cpu) = cpu_metered(body).await;
        assemble_result(parts, cpu, wall0, limits, &budget)
    }
}

/// Waits for the glue's promise with the host holding the wall deadline.
///
/// The QuickJS interrupt only fires while bytecode runs, so a promise nothing
/// will ever settle — `new Promise(() => {})` — leaves the job queue idle and
/// `into_future` pending forever. The host enforces the same deadline from the
/// outside, with a small grace so a genuine in-engine termination (which
/// carries the more specific message) wins the race, and reports the expiry
/// exactly as the blocking executor reports an unsettled promise.
async fn settle_with_deadline<'js>(
    c: &Ctx<'js>,
    promise: Promise<'js>,
    deadline: Instant,
    expired: &AtomicBool,
    limits: &Limits,
) -> (Outcome, Response, Vec<LogEntry>, Option<String>) {
    const GRACE: Duration = Duration::from_millis(50);
    let settled =
        tokio::time::timeout_at((deadline + GRACE).into(), promise.into_future::<String>()).await;
    let expired = expired.load(Ordering::Relaxed);
    match settled {
        Ok(finished) => outcome_from_envelope(
            finished.map_err(|e| exception_message(c, e)),
            expired,
            limits,
        ),
        // Feeding `classify_with` the dead-lock shape keeps the wording
        // identical to what the blocking executor says for this situation.
        Err(_) => (
            classify_with(
                "dead lock: the promise is still pending".to_string(),
                expired,
            ),
            Response::default(),
            Vec::new(),
            None,
        ),
    }
}

/// The shared tail of the async execute paths: reinterpret a valueless failure
/// as the memory limit, then assemble the [`InvocationResult`].
fn assemble_result(
    (outcome, response, logs, stack, exec_wall): (
        Outcome,
        Response,
        Vec<LogEntry>,
        Option<String>,
        Duration,
    ),
    cpu: Duration,
    wall0: Instant,
    limits: &Limits,
    budget: &outbound::OutboundBudget,
) -> InvocationResult {
    // A failure with neither message nor stack is what running out of heap
    // looks like from here.
    let outcome = match &outcome {
        Outcome::Error(msg) if is_valueless_failure(msg, stack.as_ref()) => {
            Outcome::Terminated(format!(
                "memory limit: the handler failed with no error value, which is what \
                 exceeding the {} MB heap looks like (throwing a non-Error value is \
                 indistinguishable from here)",
                limits.memory_bytes / (1024 * 1024)
            ))
        }
        _ => outcome,
    };

    InvocationResult {
        outcome,
        content_type: response.content_type,
        status: response.status,
        headers: response.headers,
        logs,
        stack,
        wall: wall0.elapsed(),
        cpu,
        exec_wall,
        outbound_used: budget.used(),
    }
}

impl Executor for QuickJsExecutor {
    fn execute(&self, source: &str, request: &HttpRequest, limits: &Limits) -> InvocationResult {
        let wall0 = Instant::now();
        let cpu0 = cpu_time::ThreadTime::now();

        let (rt, expired) = restricted_runtime(limits);
        let ctx = Context::full(&rt).expect("quickjs context");
        let request_json = serde_json::to_string(request).expect("serialize request");

        // A compile failure is the script's problem, not the cache's: fall
        // through to source so the error surfaces from the same code path.
        let bytecode = self.bytecode_for(source).ok();
        // Fetches share the invocation's budget. Without this, exec_ms bounds
        // only JavaScript: a blocking fetch runs no bytecode, so the interrupt
        // handler never fires and the timeouts add up on top of it.
        let deadline = wall0 + Duration::from_millis(limits.wall_ms);
        let budget = Arc::new(outbound::OutboundBudget::with_deadline(
            limits.outbound.clone(),
            deadline,
        ));
        let (outcome, response, logs, stack, exec_wall) = ctx.with(|c| {
            let zero = Duration::ZERO;
            if let Err(msg) = install_fetch(&c, budget.clone()) {
                return (
                    Outcome::Error(msg),
                    Response::default(),
                    Vec::new(),
                    None,
                    zero,
                );
            }
            let handler = match load_handler(&c, source, bytecode.as_deref().map(|b| b.as_slice()))
            {
                Ok(h) => h,
                Err(msg) => return (classify(msg), Response::default(), Vec::new(), None, zero),
            };
            let glue: Function = match c.eval(GLUE) {
                Ok(g) => g,
                Err(e) => {
                    return (
                        classify(exception_message(&c, e)),
                        Response::default(),
                        Vec::new(),
                        None,
                        zero,
                    )
                }
            };
            let exec0 = Instant::now();
            let promise: Promise = match glue.call((handler, request_json.as_str())) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        classify(exception_message(&c, e)),
                        Response::default(),
                        Vec::new(),
                        None,
                        exec0.elapsed(),
                    )
                }
            };
            let finished = promise.finish::<String>();
            let exec_wall = exec0.elapsed();
            let expired = expired.load(Ordering::Relaxed);
            let finished = finished.map_err(|e| exception_message(&c, e));
            let (outcome, response, logs, stack) = outcome_from_envelope(finished, expired, limits);
            (outcome, response, logs, stack, exec_wall)
        });

        // A failure with neither message nor stack is what running out of heap
        // looks like from here.
        let outcome = match &outcome {
            Outcome::Error(msg) if is_valueless_failure(msg, stack.as_ref()) => {
                Outcome::Terminated(format!(
                    "memory limit: the handler failed with no error value, which is what \
                     exceeding the {} MB heap looks like (throwing a non-Error value is \
                     indistinguishable from here)",
                    limits.memory_bytes / (1024 * 1024)
                ))
            }
            _ => outcome,
        };

        InvocationResult {
            outcome,
            content_type: response.content_type,
            status: response.status,
            headers: response.headers,
            logs,
            stack,
            wall: wall0.elapsed(),
            cpu: cpu0.elapsed(),
            exec_wall,
            outbound_used: budget.used(),
        }
    }

    fn verify(&self, source: &str) -> Result<(), String> {
        // Module evaluation runs top-level code, so verify gets the same
        // wall/heap restrictions as an invocation.
        let limits = Limits::default();
        let (rt, _expired) = restricted_runtime(&limits);
        let ctx = Context::full(&rt).expect("quickjs context");
        ctx.with(|c| {
            install_fetch(
                &c,
                Arc::new(outbound::OutboundBudget::new(limits.outbound.clone())),
            )?;
            load_handler(&c, source, None).map(|_| ())
        })
    }

    fn inspect(&self, source: &str) -> Result<Inspection, String> {
        let limits = Limits::default();
        let (rt, _expired) = restricted_runtime(&limits);
        let ctx = Context::full(&rt).expect("quickjs context");
        ctx.with(|c| {
            install_fetch(
                &c,
                Arc::new(outbound::OutboundBudget::new(limits.outbound.clone())),
            )?;
            let module = load_module_raw(&c, source, None)?;
            let export = |name: &str| -> Option<Value> {
                module
                    .get::<_, Value>(name)
                    .ok()
                    .filter(|v| !v.is_undefined() && !v.is_null())
            };
            let mcp = export("mcp");
            let http = export("http");
            let has_default = export("default").is_some();

            // Surface-independent: an http handler and an mcp tool ask for
            // secrets the same way.
            let runtime_config = match export("config") {
                None => RuntimeConfig::default(),
                Some(value) => {
                    let json = stringify(&c, value, "config")?;
                    let parsed = serde_json::from_str::<RuntimeConfig>(&json)
                        .map_err(|e| format!("invalid config export: {e}"))?;
                    vet_runtime_config(&parsed)?;
                    parsed
                }
            };
            let inspected = |surface: Surface| Inspection {
                surface,
                config: runtime_config.clone(),
            };

            let Some(mcp) = mcp else {
                // http (or nothing declared): the default handler is the surface.
                default_handler(&module)?;
                let Some(config) = http else {
                    return Ok(inspected(Surface::Http(HttpConfig::default())));
                };
                let json = stringify(&c, config, "http")?;
                return serde_json::from_str::<HttpConfig>(&json)
                    .map(Surface::Http)
                    .map(inspected)
                    .map_err(|e| format!("invalid http export: {e}"));
            };

            if has_default {
                return Err(
                    "an mcp module must not have a default export; tools are the interface"
                        .to_string(),
                );
            }
            if http.is_some() {
                return Err(
                    "declare one surface per module: found both http and mcp exports".to_string(),
                );
            }

            // Handlers must exist here — JSON.stringify drops them, so serde
            // can't check. One eval'd pass snapshots the export (every property
            // read exactly once), checks handler presence on that snapshot, and
            // serializes the same snapshot: a getter that answers differently
            // per read cannot smuggle an unchecked tool into the stored config.
            let probe: Function = c
                .eval(
                    r#"(m) => {
                        m = m || {};
                        const config = {};
                        for (const k of Object.keys(m)) config[k] = m[k];
                        const tools = config.tools || {};
                        const snapshot = {};
                        const missing = [];
                        for (const k of Object.keys(tools)) {
                            const t = tools[k];
                            if (typeof (t && t.handler) !== "function") missing.push(k);
                            snapshot[k] = t;
                        }
                        if ("tools" in config) config.tools = snapshot;
                        return JSON.stringify({ missing, config });
                    }"#,
                )
                .map_err(|e| exception_message(&c, e))?;
            let raw: String = probe
                .call((mcp.clone(),))
                .map_err(|e| exception_message(&c, e))?;

            #[derive(Deserialize)]
            struct Probed {
                #[serde(default)]
                missing: Vec<String>,
                #[serde(default)]
                config: serde_json::Value,
            }
            let probed: Probed =
                serde_json::from_str(&raw).map_err(|e| format!("invalid mcp export: {e}"))?;
            if let Some(name) = probed.missing.first() {
                return Err(format!("tool {name} has no handler function"));
            }
            let config = serde_json::from_value::<McpConfig>(probed.config)
                .map_err(|e| format!("invalid mcp export: {e}"))?;
            if config.tools.is_empty() {
                return Err("an mcp module must declare at least one tool".to_string());
            }
            if config.tools.len() > MAX_TOOLS {
                return Err(format!(
                    "too many tools: {} (max {MAX_TOOLS})",
                    config.tools.len()
                ));
            }
            for (name, tool) in &config.tools {
                if !valid_tool_name(name) {
                    return Err(format!(
                        "invalid tool name {name:?}: 1-64 chars of a-z, 0-9, '-', '_'"
                    ));
                }
                // The MCP spec requires an object schema; booleans are valid
                // JSON Schema but choke clients, so refuse them here.
                if !tool.input_schema.is_object() {
                    return Err(format!("tool {name}: inputSchema must be a JSON object"));
                }
                jsonschema::validator_for(&tool.input_schema)
                    .map_err(|e| format!("tool {name}: invalid inputSchema: {e}"))?;
            }
            Ok(inspected(Surface::Mcp(config)))
        })
    }
}

/// JSON.stringify an export inside the module's own context; functions are
/// dropped, symbols/cycles are an error.
fn stringify<'js>(c: &Ctx<'js>, value: Value<'js>, export: &str) -> Result<String, String> {
    c.json_stringify(value)
        .map_err(|e| exception_message(c, e))?
        .ok_or_else(|| format!("{export} export is not JSON-serializable"))?
        .to_string()
        .map_err(|e| exception_message(c, e))
}

/// Same charset the server accepts for function names.
fn valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod type_declarations {
    /// The declarations shipped by `rusted types`.
    const DECLARATIONS: &str = include_str!("../rusted.d.ts");

    /// Members of an object literal in a glue script, e.g. `const request = { … }`.
    fn members_of(glue: &str, binding: &str) -> Vec<String> {
        let start = glue
            .find(&format!("const {binding} = {{"))
            .unwrap_or_else(|| panic!("the glue no longer defines `{binding}`"));
        let body = &glue[start..];
        let end = body.find("\n  };").expect("unterminated object literal");
        body[..end]
            .lines()
            .skip(1)
            .filter_map(|line| {
                let line = line.trim();
                let name = line.split(':').next()?.trim();
                (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                    .then(|| name.to_string())
            })
            .collect()
    }

    /// The harmful direction is declarations promising something the runtime
    /// does not have: that typechecks and then fails in production.
    #[test]
    fn declarations_cover_everything_the_runtime_exposes() {
        for (glue, binding, interface) in [
            (super::GLUE, "request", "Request"),
            (super::GLUE, "context", "Context"),
            (super::TOOL_GLUE, "context", "ToolContext"),
        ] {
            let members = members_of(glue, binding);
            assert!(
                !members.is_empty(),
                "parsed no members for `{binding}` — the glue shape changed"
            );
            for member in members {
                // Declared as a method, a property, or an optional property —
                // all three count.
                let declared = DECLARATIONS.contains(&format!("{member}("))
                    || DECLARATIONS.contains(&format!("{member}:"))
                    || DECLARATIONS.contains(&format!("{member}?:"));
                assert!(
                    declared,
                    "runtime exposes `{binding}.{member}` but Rusted.{interface} \
                     in rusted.d.ts does not declare it"
                );
            }
        }
    }
}
