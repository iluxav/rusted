//! The rusted execution engine: QuickJS (via rquickjs) behind an [`Executor`]
//! trait. Fresh Runtime+Context per invocation; in-engine limits (uncatchable
//! wall interrupt, heap cap, stack cap) plus host-side output cap; structured
//! console logs. QuickJS was chosen over Boa by measurement — see the README.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::{Context, Ctx, Function, Module, Promise, Runtime, Value};
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

/// Deployment intent a script declares about itself via `export const config`.
/// Unknown keys are rejected so typos fail at verify time instead of silently
/// deploying with defaults.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub methods: Option<Vec<String>>,
    #[serde(default)]
    pub path: Option<String>,
}

pub trait Executor: Send + Sync {
    fn execute(&self, source: &str, request: &HttpRequest, limits: &Limits) -> InvocationResult;

    /// Parse + compile + check the default export, without invoking. Err is a
    /// human-readable compile/shape error.
    fn verify(&self, source: &str) -> Result<(), String>;

    /// Like [`Executor::verify`], but also reads the optional `export const
    /// config` declaration from the module.
    fn inspect(&self, source: &str) -> Result<FileConfig, String>;
}

/// Installed before module evaluation so top-level `console.log` works and is
/// captured. Log entries are capped at 100 × 1KB.
const FETCH_PRELUDE: &str = r#"(() => {
  globalThis.fetch = async (url, init) => {
    const raw = globalThis.__rustedFetch(JSON.stringify({
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

/// Adapts `handler(request, context)` to `(handler, requestJson) -> Promise<envelopeJson>`.
/// The envelope carries the response (or error) plus the logs collected by the
/// console prelude, so the host only ever marshals strings.
const GLUE: &str = r#"(handler, requestJson) => {
  const req = JSON.parse(requestJson);
  const logs = globalThis.__rustedLogs || [];
  const request = {
    method: req.method,
    headers: req.headers,
    query: req.query,
    params: req.params || {},
    body: req.body,
    json: async () => JSON.parse(req.body),
  };
  const respond = (body, contentType, init) => ({
    __rustedResponse: true,
    body,
    contentType,
    status: init && init.status,
    headers: (init && init.headers) || {},
  });
  const context = {
    json: (o, init) => respond(JSON.stringify(o), "application/json", init),
    text: (s, init) => respond(String(s), "text/plain; charset=utf-8", init),
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

fn load_module<'js>(
    ctx: &Ctx<'js>,
    source: &str,
    bytecode: Option<&[u8]>,
) -> Result<(Module<'js, rquickjs::module::Evaluated>, Value<'js>), String> {
    ctx.eval::<(), _>(CONSOLE_PRELUDE)
        .map_err(|e| exception_message(ctx, e))?;
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
    let handler: Value = module
        .get("default")
        .map_err(|_| "module has no default export".to_string())?;
    if !handler.is_function() {
        return Err("default export is not a function".to_string());
    }
    Ok((module, handler))
}

fn load_handler<'js>(
    ctx: &Ctx<'js>,
    source: &str,
    bytecode: Option<&[u8]>,
) -> Result<Value<'js>, String> {
    load_module(ctx, source, bytecode).map(|(_, handler)| handler)
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
        let budget = Arc::new(outbound::OutboundBudget::new(limits.outbound.clone()));
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
            match finished {
                Ok(envelope_json) => match serde_json::from_str::<Envelope>(&envelope_json) {
                    Ok(env) if env.ok => {
                        if env.response.len() > limits.max_output_bytes {
                            (
                                Outcome::Terminated(format!(
                                    "output limit: {} > {} bytes",
                                    env.response.len(),
                                    limits.max_output_bytes
                                )),
                                Response::default(),
                                env.logs,
                                None,
                                exec_wall,
                            )
                        } else {
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
                                    exec_wall,
                                ),
                                Err(message) => (
                                    Outcome::Error(message),
                                    Response::default(),
                                    env.logs,
                                    None,
                                    exec_wall,
                                ),
                            }
                        }
                    }
                    // Rejections also classify: an uncatchable stack overflow
                    // surfaces here as a rejection, and it is a limit, not a bug.
                    Ok(env) => (
                        classify_with(env.error, expired),
                        Response::default(),
                        env.logs,
                        env.stack,
                        exec_wall,
                    ),
                    Err(e) => (
                        Outcome::Error(format!("malformed envelope: {e}")),
                        Response::default(),
                        Vec::new(),
                        None,
                        exec_wall,
                    ),
                },
                Err(e) => (
                    classify_with(exception_message(&c, e), expired),
                    Response::default(),
                    Vec::new(),
                    None,
                    exec_wall,
                ),
            }
        });

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

    fn inspect(&self, source: &str) -> Result<FileConfig, String> {
        let limits = Limits::default();
        let (rt, _expired) = restricted_runtime(&limits);
        let ctx = Context::full(&rt).expect("quickjs context");
        ctx.with(|c| {
            install_fetch(
                &c,
                Arc::new(outbound::OutboundBudget::new(limits.outbound.clone())),
            )?;
            let (module, _handler) = load_module(&c, source, None)?;
            let config: Value = match module.get("config") {
                Ok(v) => v,
                Err(_) => return Ok(FileConfig::default()),
            };
            if config.is_undefined() || config.is_null() {
                return Ok(FileConfig::default());
            }
            let json = c
                .json_stringify(config)
                .map_err(|e| exception_message(&c, e))?
                .ok_or_else(|| "config export is not JSON-serializable".to_string())?
                .to_string()
                .map_err(|e| exception_message(&c, e))?;
            serde_json::from_str::<FileConfig>(&json)
                .map_err(|e| format!("invalid config export: {e}"))
        })
    }
}
