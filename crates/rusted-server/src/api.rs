use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{any, delete, get, post};
use axum::Router;
use rusted_engine::{Executor, HttpRequest, InvocationResult, Outcome};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::mcp_wire;
use crate::state::{
    now_epoch, AppState, InvocationRecord, TempRun, ADMIN_BODY_LIMIT, RECORD_CAP,
    REQUEST_BODY_LIMIT,
};

const DEFAULT_RUN_TTL_SECS: u64 = 120;
const MAX_RUN_TTL_SECS: u64 = 3600;

type Shared = State<Arc<AppState>>;

/// True when the function exists and belongs to `user_id`.
async fn owns(state: &Arc<AppState>, name: &str, user_id: Uuid) -> bool {
    matches!(state.store.owner(name).await, Ok(Some(owner)) if owner == user_id)
}

/// The user behind an admin request, resolved from `Authorization: Bearer
/// rk_live_…`. Returns the 401 response when absent or invalid.
async fn caller(state: &Arc<AppState>, headers: &HeaderMap) -> Result<Uuid, Response> {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) => match crate::auth::user_for_key(state, token).await {
            Some(user_id) => Ok(user_id),
            None => Err(err(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "that API key is not valid",
            )),
        },
        None => Err(err(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "set RUSTED_API_KEY in .env — create a key in the console at /console/keys",
        )),
    }
}

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message.into() } })),
    )
        .into_response()
}

/// The resource budget a deployed function runs under, echoed on push/run so
/// owners know what they were allocated. CPU is bounded via the wall deadline
/// for now; a separate CPU budget is a cloud-milestone item.
fn limits_json(state: &AppState, plan: &crate::plans::Plan) -> serde_json::Value {
    json!({
        "plan": plan.name,
        "plan_version": plan.version,
        "wall_ms": plan.limits.exec_ms,
        "memory_bytes": state.limits.memory_bytes,
        "request_body_bytes": REQUEST_BODY_LIMIT,
        "response_body_bytes": state.limits.max_output_bytes,
        "max_script_bytes": plan.limits.max_script_bytes,
        "max_functions": plan.limits.max_functions,
        "rate_per_min": plan.limits.rate_per_min,
        "outbound_reqs": plan.limits.outbound_reqs,
        "concurrency": plan.limits.concurrency,
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

const ALLOWED_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

fn validate_trigger(methods: &[String], path: &Option<String>) -> Result<(), String> {
    if methods.is_empty() {
        return Err("at least one HTTP method is required".into());
    }
    for m in methods {
        if !ALLOWED_METHODS.contains(&m.as_str()) {
            return Err(format!("unsupported method: {m}"));
        }
    }
    if let Some(path) = path {
        if !path.starts_with('/') {
            return Err("path must start with '/'".into());
        }
        if path.contains('?') || path.contains('#') {
            return Err(
                "path must not declare a query string or fragment; query parameters are dynamic and arrive via request.query"
                    .into(),
            );
        }
        for segment in path.trim_matches('/').split('/') {
            if segment.is_empty() {
                return Err("path has an empty segment".into());
            }
            if segment.starts_with('{') != segment.ends_with('}') {
                return Err(format!("malformed parameter segment: {segment}"));
            }
        }
    }
    Ok(())
}

/// Matches an actual sub-path against a declared pattern like `/users/{id}`,
/// returning the captured params.
pub fn match_path(pattern: &str, actual: &str) -> Option<BTreeMap<String, String>> {
    let pattern: Vec<&str> = pattern.trim_matches('/').split('/').collect();
    let actual: Vec<&str> = actual.trim_matches('/').split('/').collect();
    if pattern.len() != actual.len() {
        return None;
    }
    let mut params = BTreeMap::new();
    for (p, a) in pattern.iter().zip(actual.iter()) {
        if let Some(name) = p.strip_prefix('{').and_then(|x| x.strip_suffix('}')) {
            params.insert(name.to_string(), (*a).to_string());
        } else if p != a {
            return None;
        }
    }
    Some(params)
}

// ---------------------------------------------------------------- execution

/// One unit of work for the executor: an HTTP request against a function's
/// default handler, or an MCP tool call against one of its named tools. Both
/// share the admission path — memory guard, worker slots, per-function
/// concurrency, analytics — so limits mean the same thing on either surface.
pub(crate) enum Job {
    Http(HttpRequest),
    Tool { name: String, args: Value },
}

impl Job {
    /// The tool name, when this is a tool call — recorded in the invocation
    /// detail so `rusted logs` says which tool ran.
    fn tool(&self) -> Option<&str> {
        match self {
            Job::Http(_) => None,
            Job::Tool { name, .. } => Some(name),
        }
    }
}

/// Runs `source` on a worker thread with no per-key state — used for ad-hoc
/// invocations so they can't leak locks or records. The wait for a worker slot
/// is bounded by the same queue-wait budget as the per-function lock.
/// Everything the host grants one invocation beyond the request itself:
/// decrypted secrets, the declared capabilities being supplied, and the scope
/// the services need to enforce them. Built from the stored record and the
/// owner's plan — never from anything the caller sent. The default grants
/// nothing, which is what ad-hoc and temp-run invocations get.
pub(crate) struct HostGrant {
    pub env: Option<BTreeMap<String, String>>,
    pub caps: rusted_engine::Capabilities,
    /// The stable function name state and objects are scoped by.
    pub function_name: Option<String>,
    /// The environment the invocation resolved through.
    pub env_name: String,
    pub objects: BTreeMap<String, rusted_engine::ObjectBinding>,
    pub allowance: crate::fnstate::StateAllowance,
}

impl Default for HostGrant {
    fn default() -> Self {
        Self {
            env: None,
            caps: rusted_engine::Capabilities::none()
                .with_env(crate::secrets::PROD_ENV.to_string()),
            function_name: None,
            env_name: crate::secrets::PROD_ENV.to_string(),
            objects: BTreeMap::new(),
            allowance: crate::fnstate::StateAllowance::default(),
        }
    }
}

async fn execute_raw(
    state: &Arc<AppState>,
    source: String,
    job: Job,
    limits: rusted_engine::Limits,
    owner: Option<Uuid>,
    grant: HostGrant,
) -> Result<InvocationResult, Response> {
    // Before queueing for a slot: if the process is already using more memory
    // than it should, another invocation makes that worse rather than better.
    if !state.memory.has_headroom() {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "memory_pressure",
            "the server is using too much memory to start another invocation; retry shortly",
        ));
    }
    let slots = state.exec_slots.clone();
    let _slot = tokio::time::timeout(
        Duration::from_millis(state.queue_wait_ms),
        slots.acquire_owned(),
    )
    .await
    .map_err(|_| {
        err(
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
            "all worker slots are busy",
        )
    })?
    .expect("semaphore never closed");

    let executor = state.executor.clone();
    // Scoped to the function's owner, taken from the stored record rather than
    // anything the caller sent, so a handler cannot reach another account's
    // inboxes, state, or objects. Anonymous functions get no services at all.
    let services: Option<Arc<dyn rusted_engine::HostServices>> = owner.map(|user_id| {
        Arc::new(crate::services::OwnerScopedServices::new(
            state.clone(),
            user_id,
            grant.function_name.clone().unwrap_or_default(),
            grant.env_name.clone(),
            grant.objects.clone(),
            grant.allowance,
            grant.caps.db,
            std::time::Instant::now() + std::time::Duration::from_millis(limits.wall_ms),
        )) as Arc<dyn rusted_engine::HostServices>
    });
    // Handed to the execution runtime rather than run here: JavaScript between
    // await points blocks whichever thread drives it, and that must not be a
    // thread serving HTTP.
    Ok(state
        .exec_runtime
        .spawn(async move {
            let env = grant.env.as_ref();
            let caps = &grant.caps;
            match job {
                Job::Http(request) => {
                    executor
                        .execute_with_services(&source, &request, &limits, services, env, caps)
                        .await
                }
                Job::Tool { name, args } => {
                    executor
                        .execute_tool_with_services(
                            &source, &name, &args, &limits, services, env, caps,
                        )
                        .await
                }
            }
        })
        .await
        .expect("executor task never panics"))
}

/// Runs `source` with concurrency 1 per `key` and records the invocation.
/// The grant carries the function's decrypted secrets and supplied
/// capabilities, resolved by the caller from the stored record — never from
/// anything the request sent.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_serialized(
    state: &Arc<AppState>,
    key: &str,
    source: String,
    job: Job,
    limits: rusted_engine::Limits,
    owner: Option<uuid::Uuid>,
    concurrency: usize,
    grant: HostGrant,
) -> Result<InvocationResult, Response> {
    let allowed = concurrency.max(1);
    let gate = {
        let mut locks = state.fn_locks.lock().unwrap();
        let entry = locks
            .entry(key.to_string())
            .or_insert_with(|| (allowed, Arc::new(tokio::sync::Semaphore::new(allowed))));
        // A plan change means a different allowance; start a fresh semaphore
        // rather than trying to resize one that has permits outstanding.
        if entry.0 != allowed {
            *entry = (allowed, Arc::new(tokio::sync::Semaphore::new(allowed)));
        }
        entry.1.clone()
    };
    let _turn = tokio::time::timeout(
        Duration::from_millis(state.queue_wait_ms),
        gate.acquire_owned(),
    )
    .await
    .map_err(|_| {
        err(
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
            format!("function is busy; this plan allows {allowed} at once"),
        )
    })?
    .expect("semaphore never closed");
    let tool = job.tool().map(|t| t.to_string());
    let is_http = matches!(job, Job::Http(_));
    let result = execute_raw(state, source, job, limits, owner, grant).await?;
    debug_print(state, key, &result);

    let base_detail = match &result.outcome {
        Outcome::Success(_) => None,
        Outcome::Terminated(reason) => Some(reason.clone()),
        Outcome::Error(message) => Some(message.clone()),
    };
    // The status the caller saw — mirrors outcome_to_http, recorded so a
    // handler answering error envelopes with 4xx statuses reads as what it
    // was, not as an unblemished success. Tool calls have no HTTP status.
    let status = is_http.then(|| match &result.outcome {
        Outcome::Success(_) => result.status.unwrap_or(200),
        Outcome::Terminated(_) => 429,
        Outcome::Error(_) => 500,
    });
    let record = InvocationRecord {
        at: now_epoch(),
        outcome: match &result.outcome {
            Outcome::Success(_) => "success".into(),
            Outcome::Terminated(_) => "terminated".into(),
            Outcome::Error(_) => "error".into(),
        },
        detail: match (&tool, base_detail) {
            (Some(tool), Some(detail)) => Some(format!("tool {tool}: {detail}")),
            (Some(tool), None) => Some(format!("tool {tool}")),
            (None, detail) => detail,
        },
        wall_ms: result.wall.as_secs_f64() * 1000.0,
        cpu_ms: result.cpu.as_secs_f64() * 1000.0,
        status,
        logs: result.logs.clone(),
    };
    state.telemetry.record(
        key,
        &record.outcome,
        Some(result.exec_wall.as_secs_f64() * 1000.0),
    );
    // Queued, never awaited: analytics can shed load but never delay a call.
    state.analytics.record(crate::analytics::Invocation {
        function_name: key.to_string(),
        user_id: owner,
        outcome: record.outcome.clone(),
        detail: record.detail.clone(),
        wall_ms: record.wall_ms,
        cpu_ms: record.cpu_ms,
        exec_ms: result.exec_wall.as_secs_f64() * 1000.0,
        status: status.map(|s| s as i16),
    });

    let mut records = state.records.lock().unwrap();
    let ring = records.entry(key.to_string()).or_default();
    ring.push_front(record);
    ring.truncate(RECORD_CAP);

    Ok(result)
}

/// The decrypted environment a function's stored record asks for. `Ok(None)`
/// when the module requested no secrets — `context.env` then stays undefined.
/// The names come from the deploy-time record and the owner from the store,
/// never from anything the caller sent.
pub(crate) async fn env_for_function(
    state: &Arc<AppState>,
    fetched: &crate::store::Fetched,
    env: &str,
) -> Result<Option<BTreeMap<String, String>>, String> {
    if fetched.secrets.is_empty() {
        return Ok(None);
    }
    let Some(owner) = fetched.owner else {
        return Err("this function has no owner, so its secrets cannot be resolved".to_string());
    };
    state
        .secrets
        .env_for(owner, env, &fetched.secrets)
        .await
        .map(Some)
}

/// The full grant for a deployed function, or the refusal `(code, detail)`.
///
/// This is the "refuse before JavaScript runs" gate: declared secrets must
/// decrypt, declared object bindings must point at allowlisted endpoints with
/// resolvable credentials. The detail names what is wrong for the owner's
/// logs; the code picks the caller-facing error.
pub(crate) async fn grant_for_function(
    state: &Arc<AppState>,
    name: &str,
    fetched: &crate::store::Fetched,
    plan: &crate::plans::Plan,
    env_name: &str,
) -> Result<HostGrant, (&'static str, String)> {
    let env = env_for_function(state, fetched, env_name)
        .await
        .map_err(|detail| ("missing_secrets", detail))?;
    if !fetched.objects.is_empty() {
        let Some(owner) = fetched.owner else {
            return Err((
                "capability_unavailable",
                "this function has no owner, so its object bindings cannot be resolved".to_string(),
            ));
        };
        for (binding_name, binding) in &fetched.objects {
            // Re-checked per invocation: an endpoint struck off the allowlist
            // stops working without waiting for a redeploy.
            state.objects.allows(&binding.endpoint).map_err(|e| {
                (
                    "capability_unavailable",
                    format!("binding {binding_name}: {e}"),
                )
            })?;
            let names = [
                binding.access_key_id_secret.clone(),
                binding.secret_access_key_secret.clone(),
            ];
            state
                .secrets
                .env_for(owner, env_name, &names)
                .await
                .map_err(|e| {
                    (
                        "capability_unavailable",
                        format!("binding {binding_name}: {e}"),
                    )
                })?;
        }
    }
    Ok(HostGrant {
        env,
        caps: rusted_engine::Capabilities {
            state: fetched.state,
            db: fetched.db,
            objects: fetched.objects.keys().cloned().collect(),
            auth: None,
            env_name: Some(env_name.to_string()),
        },
        function_name: Some(name.to_string()),
        env_name: env_name.to_string(),
        objects: fetched.objects.clone(),
        allowance: crate::fnstate::StateAllowance {
            max_keys: plan.limits.max_state_keys,
            max_bytes: plan.limits.max_state_bytes,
        },
    })
}

/// A refusal before execution — missing secrets — still needs to reach the
/// owner's logs: record it like an errored invocation so `rusted logs` and the
/// console explain it, whatever generic answer the caller gets.
pub(crate) fn record_refusal(
    state: &Arc<AppState>,
    key: &str,
    owner: Option<uuid::Uuid>,
    outcome: &str,
    status: u16,
    detail: String,
) {
    state.telemetry.record(key, outcome, None);
    state.analytics.record(crate::analytics::Invocation {
        function_name: key.to_string(),
        user_id: owner,
        outcome: outcome.to_string(),
        detail: Some(detail.clone()),
        wall_ms: 0.0,
        cpu_ms: 0.0,
        exec_ms: 0.0,
        status: Some(status as i16),
    });
    let mut records = state.records.lock().unwrap();
    let ring = records.entry(key.to_string()).or_default();
    ring.push_front(InvocationRecord {
        at: now_epoch(),
        outcome: outcome.to_string(),
        detail: Some(detail),
        wall_ms: 0.0,
        cpu_ms: 0.0,
        status: Some(status),
        logs: Vec::new(),
    });
    ring.truncate(RECORD_CAP);
}

/// Engine limits for a plan: execution budget and outbound allowance.
fn limits_for_plan(state: &AppState, plan: &crate::plans::PlanLimits) -> rusted_engine::Limits {
    rusted_engine::Limits {
        wall_ms: plan.exec_ms,
        memory_bytes: state.limits.memory_bytes,
        max_output_bytes: state.limits.max_output_bytes,
        outbound: rusted_engine::OutboundPolicy {
            max_requests: plan.outbound_reqs,
            max_response_bytes: state.limits.max_output_bytes,
            timeout: Duration::from_millis(plan.exec_ms),
        },
    }
}

/// The plan governing a function, and its engine limits. The owner comes from
/// the cached function record, so a warm invocation never queries Postgres.
pub(crate) async fn plan_for_owner(
    state: &Arc<AppState>,
    owner: Option<uuid::Uuid>,
) -> (crate::plans::Plan, rusted_engine::Limits) {
    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, owner).await;
    let limits = limits_for_plan(state, &plan.limits);
    (plan, limits)
}

/// With `--debug`, prints one line per invocation (plus its console output) to
/// the server's stdout.
fn debug_print(state: &AppState, key: &str, result: &InvocationResult) {
    if !state.debug {
        return;
    }
    let (status, detail) = match &result.outcome {
        Outcome::Success(_) => ("success", None),
        Outcome::Terminated(reason) => ("terminated", Some(reason.as_str())),
        Outcome::Error(message) => ("error", Some(message.as_str())),
    };
    println!(
        "[rusted] {key} {status} wall={:.2}ms cpu={:.2}ms exec={:.2}ms{}",
        result.wall.as_secs_f64() * 1000.0,
        result.cpu.as_secs_f64() * 1000.0,
        result.exec_wall.as_secs_f64() * 1000.0,
        detail.map(|d| format!(" — {d}")).unwrap_or_default(),
    );
    for log in &result.logs {
        println!("[rusted] {key} console.{}: {}", log.level, log.message);
    }
}

/// Drops the per-key lock and record state for keys that no longer exist
/// (deleted functions, expired temp runs).
fn prune_keys(state: &Arc<AppState>, keys: impl IntoIterator<Item = String>) {
    let mut locks = state.fn_locks.lock().unwrap();
    let mut records = state.records.lock().unwrap();
    for key in keys {
        locks.remove(&key);
        records.remove(&key);
    }
}

/// context.json/context.text set an explicit type; bare returns are sniffed.
fn response_content_type(explicit: Option<String>, body: &str) -> String {
    explicit.unwrap_or_else(|| {
        if serde_json::from_str::<serde_json::Value>(body).is_ok() {
            "application/json"
        } else {
            "text/plain; charset=utf-8"
        }
        .to_string()
    })
}

/// Maps an outcome to a data-plane HTTP response. Endpoint callers are third
/// parties: JS error messages and console logs never appear here — the owner
/// inspects them through the admin API (`recent`) or `rusted logs`.
fn outcome_to_http(result: InvocationResult) -> Response {
    match result.outcome {
        Outcome::Success(body) => {
            let content_type = response_content_type(result.content_type, &body);
            let status =
                StatusCode::from_u16(result.status.unwrap_or(200)).unwrap_or(StatusCode::OK);
            let mut response = (status, [(CONTENT_TYPE, content_type)], body).into_response();
            // Applied after content-type so a handler can override it, and
            // already vetted by the engine — nothing here can reframe the reply.
            let out = response.headers_mut();
            for (name, value) in &result.headers {
                if let (Ok(name), Ok(value)) = (
                    axum::http::HeaderName::try_from(name.as_str()),
                    axum::http::HeaderValue::try_from(value.as_str()),
                ) {
                    out.insert(name, value);
                }
            }
            response
        }
        Outcome::Terminated(reason) => err(StatusCode::TOO_MANY_REQUESTS, "limit_exceeded", reason),
        Outcome::Error(_) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "function_error",
            "function execution failed",
        ),
    }
}

/// Wraps error responses that bypass our handlers (body-limit 413, router 404/405)
/// in the standard `{error:{code,message}}` envelope.
async fn envelope_errors(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let already_json = response
        .headers()
        .get(CONTENT_TYPE)
        .map(|v| v.as_bytes().starts_with(b"application/json"))
        .unwrap_or(false);
    if already_json {
        return response;
    }
    let code = match status {
        StatusCode::PAYLOAD_TOO_LARGE => "body_too_large",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        _ => "http_error",
    };
    err(status, code, status.canonical_reason().unwrap_or("error"))
}

/// Builds the engine's view of a request, or explains why it cannot.
///
/// The body is refused rather than coerced. `from_utf8_lossy` does not reject
/// invalid bytes — it replaces each one with U+FFFD, so a PNG posted here used
/// to arrive as mojibake with no error: 1024 random bytes became 967 characters,
/// 409 of them replacements. A handler cannot detect that, and neither can the
/// caller. Refusing is the only honest answer while the engine speaks strings.
///
/// Header values are dropped rather than refused. A non-ASCII header value is
/// non-conformant to begin with, and one odd header should not sink an
/// otherwise valid request — but it must not be silently mangled either, so an
/// unreadable value is absent rather than corrupted.
fn to_engine_request(
    method: &str,
    headers: &HeaderMap,
    query: HashMap<String, String>,
    params: BTreeMap<String, String>,
    body: Bytes,
) -> Result<HttpRequest, Box<Response>> {
    let body = String::from_utf8(body.to_vec()).map_err(|e| {
        let at = e.utf8_error().valid_up_to();
        Box::new(err(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            format!(
                "request body is not valid UTF-8 (first bad byte at offset {at} of {}). \
                 Functions receive the body as text, so binary payloads cannot be passed \
                 through unchanged — send them base64-encoded inside JSON instead.",
                e.as_bytes().len()
            ),
        ))
    })?;
    Ok(HttpRequest {
        method: method.to_string(),
        headers: headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|value| (k.as_str().to_string(), value.to_string()))
            })
            .collect(),
        query: query.into_iter().collect::<BTreeMap<_, _>>(),
        params,
        body,
    })
}

// ----------------------------------------------------------------- data API

async fn call_function_root(
    State(state): Shared,
    Path(name): Path<String>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve_function(state, name, None, method, query, headers, body).await
}

async fn call_function_sub(
    State(state): Shared,
    Path((name, rest)): Path<(String, String)>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve_function(state, name, Some(rest), method, query, headers, body).await
}

async fn mcp_protected_resource(State(state): Shared, Path(name): Path<String>) -> Response {
    crate::mcp_auth::protected_resource(state, crate::secrets::PROD_ENV.to_string(), name).await
}

/// The env variant: `/f/@stage/name`. The marker is required — two bare
/// segments describe nothing this server serves.
async fn mcp_protected_resource_env(
    State(state): Shared,
    Path((env, name)): Path<(String, String)>,
) -> Response {
    let Some(env) = env.strip_prefix('@') else {
        return err(StatusCode::NOT_FOUND, "not_found", "no such resource");
    };
    crate::mcp_auth::protected_resource(state, env.to_string(), name).await
}

async fn serve_function(
    state: Arc<AppState>,
    name: String,
    rest: Option<String>,
    method: Method,
    query: HashMap<String, String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Environment selection: `/f/@stage/name[/rest]`. `@` can never begin a
    // function name, so the parse is unambiguous and costs no lookup.
    let (env, name, rest) = match name.strip_prefix('@') {
        None => (crate::secrets::PROD_ENV.to_string(), name, rest),
        Some(env) => {
            let env = env.to_string();
            let Some(r) = rest else {
                return err(StatusCode::NOT_FOUND, "not_found", "no such function");
            };
            match r.split_once('/') {
                Some((function, sub)) if !function.is_empty() => {
                    (env, function.to_string(), Some(sub.to_string()))
                }
                None if !r.is_empty() => (env, r, None),
                _ => return err(StatusCode::NOT_FOUND, "not_found", "no such function"),
            }
        }
    };
    // Refusal details carry the env so stage noise never reads as prod's.
    let tag = if env == crate::secrets::PROD_ENV {
        String::new()
    } else {
        format!("[@{env}] ")
    };
    // Served through the store's read cache; NOTIFY events keep it fresh.
    let fetched = match state.store.fetch(&name).await {
        Ok(Some(hit)) => hit,
        Ok(None) => return err(StatusCode::NOT_FOUND, "not_found", "no such function"),
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store_error",
                e.to_string(),
            )
        }
    };
    // An environment the owner never created answers exactly like a missing
    // function — probing envs must reveal as little as probing names.
    if env != crate::secrets::PROD_ENV {
        let known = match fetched.owner {
            Some(owner) => crate::secrets::env_exists(&state.pool, owner, &env).await,
            None => false,
        };
        if !known {
            return err(StatusCode::NOT_FOUND, "not_found", "no such function");
        }
    }
    // Unpublished answers exactly like missing — the toggle must reveal
    // nothing to callers — while the owner's logs say what actually happened.
    if !fetched.published {
        record_refusal(
            &state,
            &name,
            fetched.owner,
            "refused",
            404,
            format!("{tag}refused: this function is unpublished"),
        );
        return err(StatusCode::NOT_FOUND, "not_found", "no such function");
    }
    // An mcp function has no route pattern: every POST to /f/{name} is
    // protocol, and the messages inside decide what happens.
    if fetched.kind == "mcp" {
        if rest.is_some() {
            return err(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("an mcp function serves only /f/{name}"),
            );
        }
        if method != Method::POST {
            return err(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "allowed: POST",
            );
        }
        return crate::mcp_host::serve(state, fetched, name, env, headers, body).await;
    }
    // An explicit `access: "private"` (stored as public = FALSE) opts this
    // URL out of the open data plane:
    // the caller must present one of the owner's keys — anyone's valid key is
    // not enough on a multi-tenant server. Undeclared functions never enter
    // this branch; they follow the server-wide gate.
    if fetched.public == Some(false) {
        let presented = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let authorized = match presented {
            Some(token) => {
                let caller = crate::auth::user_for_key(&state, token).await;
                caller.is_some() && caller == fetched.owner
            }
            None => false,
        };
        if !authorized {
            record_refusal(
                &state,
                &name,
                fetched.owner,
                "refused",
                401,
                format!("{tag}refused: this function requires the owner's API key"),
            );
            return (
                StatusCode::UNAUTHORIZED,
                [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
                Json(json!({ "error": {
                    "code": "unauthorized",
                    "message": "this function requires its owner's API key — Authorization: Bearer rk_live_…"
                }})),
            )
                .into_response();
        }
    }
    let (source, trigger, owner) = (
        fetched.source.clone(),
        fetched.trigger.clone(),
        fetched.owner,
    );
    // From here down the function exists, so what happens to the request is
    // the owner's business: gate refusals are recorded like invocations,
    // because "my logs show nothing" and "callers are being turned away" were
    // previously indistinguishable.
    if !trigger.methods.iter().any(|m| m == method.as_str()) {
        record_refusal(
            &state,
            &name,
            owner,
            "refused",
            405,
            format!(
                "{tag}refused: method {method} (this function allows {})",
                trigger.methods.join(", ")
            ),
        );
        return err(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("allowed: {}", trigger.methods.join(", ")),
        );
    }
    let params = match (&trigger.path, rest.as_deref()) {
        (None, None) => BTreeMap::new(),
        (None, Some(rest)) => {
            record_refusal(
                &state,
                &name,
                owner,
                "refused",
                404,
                format!("{tag}refused: /{rest} — this function has no sub-path"),
            );
            return err(
                StatusCode::NOT_FOUND,
                "not_found",
                "function has no sub-path",
            );
        }
        (Some(pattern), rest) => match match_path(pattern, rest.unwrap_or("")) {
            Some(params) => params,
            None => {
                record_refusal(
                    &state,
                    &name,
                    owner,
                    "refused",
                    404,
                    format!(
                        "{tag}refused: /{} does not match the declared route {pattern}",
                        rest.unwrap_or("")
                    ),
                );
                return err(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("this function serves /f/{name}{pattern}"),
                );
            }
        },
    };
    let (plan, limits) = plan_for_owner(&state, owner).await;
    if let Err(retry_after) = state.rate_limiter.check(&name, plan.limits.rate_per_min) {
        record_refusal(
            &state,
            &name,
            owner,
            "refused",
            429,
            format!(
                "{tag}refused: rate limit ({} allows {} requests per minute)",
                plan.name, plan.limits.rate_per_min
            ),
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, retry_after.to_string())],
            Json(json!({ "error": {
                "code": "rate_limited",
                "message": format!("{} allows {} requests per minute for this function", plan.name, plan.limits.rate_per_min)
            }})),
        )
            .into_response();
    }
    let request = match to_engine_request(method.as_str(), &headers, query, params, body) {
        Ok(request) => request,
        Err(response) => {
            record_refusal(
                &state,
                &name,
                owner,
                "refused",
                400,
                format!("{tag}refused: request body is not valid UTF-8"),
            );
            return *response;
        }
    };
    // Resolved before spending an execution slot: a function whose secrets or
    // capabilities cannot be supplied would only fail inside the handler,
    // less clearly.
    let grant = match grant_for_function(&state, &name, &fetched, &plan, &env).await {
        Ok(grant) => grant,
        Err((code, detail)) => {
            record_refusal(&state, &name, owner, "error", 500, format!("{tag}{detail}"));
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                code,
                "this function declares configuration its owner has not supplied; \
                 the details are in the owner's logs",
            );
        }
    };
    match execute_serialized(
        &state,
        &name,
        source,
        Job::Http(request),
        limits,
        owner,
        plan.limits.concurrency,
        grant,
    )
    .await
    {
        Ok(result) => outcome_to_http(result),
        Err(response) => {
            // Admission refusals: queue-wait busy (429) or memory pressure
            // (503). Owner-visible for the same reason the gates above are.
            let status = response.status().as_u16();
            if matches!(status, 429 | 503) {
                record_refusal(
                    &state,
                    &name,
                    owner,
                    "refused",
                    status,
                    match status {
                        503 => format!("{tag}refused: the server was under memory pressure"),
                        _ => format!(
                            "{tag}refused: busy ({} concurrent allowed on {})",
                            plan.limits.concurrency, plan.name
                        ),
                    },
                );
            }
            response
        }
    }
}

async fn call_run(
    State(state): Shared,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let source = {
        let mut runs = state.temp_runs.lock().unwrap();
        match runs.get(&id) {
            Some(run) if run.expires_at > now_epoch() => run.source.clone(),
            Some(_) => {
                runs.remove(&id);
                drop(runs);
                prune_keys(&state, [format!("run:{id}")]);
                return err(StatusCode::NOT_FOUND, "expired", "temporary run expired");
            }
            None => return err(StatusCode::NOT_FOUND, "not_found", "no such run"),
        }
    };
    let request = match to_engine_request("POST", &headers, query, BTreeMap::new(), body) {
        Ok(request) => request,
        Err(response) => return *response,
    };
    let key = format!("run:{id}");
    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, None).await;
    let limits = limits_for_plan(&state, &plan.limits);
    match execute_serialized(
        &state,
        &key,
        source,
        Job::Http(request),
        limits,
        None,
        plan.limits.concurrency,
        HostGrant::default(),
    )
    .await
    {
        Ok(result) => outcome_to_http(result),
        Err(response) => response,
    }
}

/// With `--require-auth`, every data-plane call needs a valid API key — except
/// calls to a function that declared itself public, which is how an OAuth
/// callback or webhook target works: the third party calling it cannot present
/// the owner's key. The verdict comes from the in-memory caches (see
/// [`crate::auth`] and the store's read cache) — steady-state traffic never
/// touches Postgres.
async fn bearer_gate(
    State(state): Shared,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !state.require_auth {
        return next.run(request).await;
    }
    let presented = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let ok = match presented {
        Some(token) => crate::auth::verify_key(&state, token).await,
        None => false,
    };
    if !ok {
        // Protected-resource metadata is how an unauthenticated OAuth client
        // learns where to authorize — it is public by definition (RFC 9728),
        // so the key gate cannot apply to it.
        if request
            .uri()
            .path()
            .starts_with("/.well-known/oauth-protected-resource/")
        {
            return next.run(request).await;
        }
        // Publicness comes from the stored record, never the request; only
        // /f/<name> routes can carry it, so temp runs stay gated. A missing
        // function answers 401 like everything else — an unauthenticated
        // probe learns nothing about what exists.
        if let Some(name) = public_function_candidate(request.uri().path()) {
            if let Ok(Some(hit)) = state.store.fetch(name).await {
                let externally_authenticated_mcp = hit.kind == "mcp"
                    && hit.mcp.as_ref().and_then(|meta| meta.get("auth")).is_some();
                if hit.public == Some(true) || externally_authenticated_mcp {
                    return next.run(request).await;
                }
            }
        }
        return (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({ "error": {
                "code": "unauthorized",
                "message": "provide Authorization: Bearer rk_live_… — create keys in the console"
            }})),
        )
            .into_response();
    }
    next.run(request).await
}

/// The function name a data-plane path targets, when it's a path publicness
/// can apply to.
fn public_function_candidate(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/f/")?;
    // `/f/@stage/name/...` — the env segment is routing, not the name.
    let rest = match rest.strip_prefix('@') {
        Some(after) => after.split_once('/').map(|(_, r)| r).unwrap_or(""),
        None => rest,
    };
    rest.split('/').next().filter(|name| !name.is_empty())
}

pub fn data_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource/f/{name}",
            get(mcp_protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/f/{env}/{name}",
            get(mcp_protected_resource_env),
        )
        .route("/f/{name}", any(call_function_root))
        .route("/f/{name}/{*rest}", any(call_function_sub))
        .route("/r/{id}", post(call_run))
        .layer(DefaultBodyLimit::max(REQUEST_BODY_LIMIT))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            bearer_gate,
        ))
        .layer(axum::middleware::from_fn(envelope_errors))
        .with_state(state)
}

// ---------------------------------------------------------------- admin API

async fn verify_source(state: &Arc<AppState>, source: String) -> Result<(), String> {
    let executor = state.executor.clone();
    tokio::task::spawn_blocking(move || executor.verify(&source))
        .await
        .expect("verify thread never panics")
}

#[derive(Deserialize)]
struct PushBody {
    #[serde(default)]
    name: Option<String>,
    source: String,
    #[serde(default)]
    methods: Option<Vec<String>>,
    #[serde(default)]
    path: Option<String>,
}

/// Compile-checks the source and reads which surface it declares
/// (`export const http` or `export const mcp`) plus the runtime config
/// (`export const config`).
pub(crate) async fn inspect_source(
    state: &Arc<AppState>,
    source: String,
) -> Result<rusted_engine::Inspection, String> {
    let executor = state.executor.clone();
    tokio::task::spawn_blocking(move || executor.inspect(&source))
        .await
        .expect("inspect thread never panics")
}

/// Tool names out of stored mcp metadata (`{"public": ..., "tools": {...}}`).
fn mcp_tool_names(meta: &Value) -> Vec<String> {
    meta["tools"]
        .as_object()
        .map(|tools| tools.keys().cloned().collect())
        .unwrap_or_default()
}

/// Why a deploy was refused, carrying the code an API caller expects.
pub struct DeployRefused {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl DeployRefused {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

/// The origin of a function's currently-serving revision, None for a new name.
pub(crate) async fn previous_via(state: &Arc<AppState>, name: &str) -> Option<String> {
    sqlx::query(
        "SELECT r.via FROM functions f
         JOIN revisions r ON r.function_name = f.name AND r.rev = f.current_rev
         WHERE f.name = $1",
    )
    .bind(name)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()
    .map(|row| sqlx::Row::get(&row, "via"))
}

/// Deploys a function and describes what now exists at what URL.
///
/// Shared by the HTTP route and the MCP tool. Everything here is a decision —
/// plan limits, name ownership, trigger validation — and two copies of it would
/// drift, so there is one.
pub async fn deploy_function(
    state: &Arc<AppState>,
    user_id: Uuid,
    source: String,
    name: Option<String>,
    methods: Option<Vec<String>>,
    path: Option<String>,
    via: &str,
) -> Result<Value, DeployRefused> {
    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, Some(user_id)).await;
    if source.len() as i64 > plan.limits.max_script_bytes {
        return Err(DeployRefused::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "plan_limit",
            format!(
                "script is {} bytes; the {} plan allows {} — upgrade at /console/billing",
                source.len(),
                plan.name,
                plan.limits.max_script_bytes
            ),
        ));
    }

    let inspection = inspect_source(state, source.clone())
        .await
        .map_err(|e| DeployRefused::new(StatusCode::UNPROCESSABLE_ENTITY, "compile_error", e))?;
    let config = inspection.config;
    let surface = inspection.surface;

    // Object bindings are refused at deploy unless every endpoint is on this
    // server's allowlist — a binding is a credentialed HTTP client, and where
    // it may point is the server admin's decision, not the module's.
    for (binding_name, binding) in &config.objects {
        state.objects.allows(&binding.endpoint).map_err(|e| {
            DeployRefused::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "endpoint_not_allowed",
                format!("object binding {binding_name}: {e}"),
            )
        })?;
    }

    // Everything up to the surface split is shared: the file's declared name
    // stands in for a missing request name, and the same plan/ownership rules
    // apply either way.
    let declared_name = match &surface {
        rusted_engine::Surface::Http(config) => config.name.clone(),
        rusted_engine::Surface::Mcp(config) => config.name.clone(),
    };
    let Some(name) = name.or(declared_name) else {
        return Err(DeployRefused::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "missing_name",
            "provide a name, or declare it in the module's `http`/`mcp` config export",
        ));
    };
    if !valid_name(&name) {
        return Err(DeployRefused::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_name",
            "names are 1-64 chars of a-z, 0-9, '-', '_'",
        ));
    }

    let replacing = matches!(state.store.owner(&name).await, Ok(Some(owner)) if owner == user_id);
    if !replacing {
        let owned = state.store.count_for_user(user_id).await.unwrap_or(0);
        if owned >= plan.limits.max_functions {
            return Err(DeployRefused::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "plan_limit",
                format!(
                    "the {} plan allows {} functions — delete one or upgrade at /console/billing",
                    plan.name, plan.limits.max_functions
                ),
            ));
        }
        if let Ok(Some(_other)) = state.store.owner(&name).await {
            return Err(DeployRefused::new(
                StatusCode::CONFLICT,
                "name_taken",
                "another account already deployed a function with that name",
            ));
        }
    }

    let http_config = match surface {
        rusted_engine::Surface::Http(config) => config,
        rusted_engine::Surface::Mcp(mcp_config) => {
            if methods.is_some() || path.is_some() {
                return Err(DeployRefused::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unsupported_trigger",
                    "an mcp function takes no methods or path",
                ));
            }
            let meta = json!({
                "public": mcp_config.public,
                "auth": mcp_config.auth,
                "tools": mcp_config.tools,
            });
            let declared = crate::store::Declared::from_config(&config, Some(mcp_config.public));
            let previous_via = previous_via(state, &name).await;
            let revision = state
                .store
                .push_full(
                    &name,
                    &source,
                    None,
                    "mcp",
                    Some(&meta),
                    Some(user_id),
                    &declared,
                    via,
                )
                .await
                .map_err(|e| {
                    DeployRefused::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "store_error",
                        e.to_string(),
                    )
                })?;
            let mut out = json!({
                "name": name,
                "revision": revision.rev,
                "hash": revision.hash,
                "size_bytes": source.len(),
                "kind": "mcp",
                "tools": mcp_config.tools.keys().collect::<Vec<_>>(),
                "public": mcp_config.public,
                "secrets": declared.secrets,
                "state": declared.state,
                "objects": declared.objects.keys().collect::<Vec<_>>(),
                "via": via,
                "previous_via": previous_via,
                "url": state.data_url(&format!("/f/{name}")),
            });
            if !mcp_config.public {
                out["note"] =
                    json!("connecting requires your API key (Authorization: Bearer <key>)");
            }
            return Ok(out);
        }
    };

    // The file's own declaration stands in for anything the caller left out.
    let methods = methods.or(http_config.methods);
    let path = path.or(http_config.path);

    // A push that names no trigger fields keeps the function's existing route;
    // naming any of them replaces the whole trigger config.
    let new_trigger = if methods.is_some() || path.is_some() {
        let methods: Vec<String> = methods
            .unwrap_or_else(|| vec!["POST".to_string()])
            .iter()
            .map(|m| m.to_uppercase())
            .collect();
        validate_trigger(&methods, &path).map_err(|e| {
            DeployRefused::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_trigger", e)
        })?;
        Some(crate::store::HttpTrigger { methods, path })
    } else {
        None
    };

    // The stored column stays tri-state: TRUE for public, FALSE for private,
    // NULL for a module that declared nothing and follows the server. The
    // engine has already validated the value and the legacy-alias conflict.
    let access = match (http_config.access.as_deref(), http_config.public) {
        (Some("private"), _) => Some(false),
        (Some("public"), _) | (None, true) => Some(true),
        _ => None,
    };
    let declared = crate::store::Declared::from_config(&config, access);
    let previous_via = previous_via(state, &name).await;
    let pushed = match new_trigger {
        Some(trigger) => {
            state
                .store
                .push_with_trigger(&name, &source, trigger, Some(user_id), &declared, via)
                .await
        }
        None => {
            state
                .store
                .push(&name, &source, Some(user_id), &declared, via)
                .await
        }
    };
    let (revision, trigger) = match pushed {
        Ok(revision) => match state.store.get(&name).await {
            Ok(Some(record)) => (revision, record.trigger),
            Ok(None) => {
                return Err(DeployRefused::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "store_error",
                    "the function vanished immediately after being written",
                ))
            }
            Err(e) => {
                return Err(DeployRefused::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "store_error",
                    e.to_string(),
                ))
            }
        },
        Err(e) => {
            return Err(DeployRefused::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store_error",
                e.to_string(),
            ))
        }
    };

    let route = format!("/f/{}{}", name, trigger.path.as_deref().unwrap_or(""));
    Ok(json!({
        "name": name,
        "revision": revision.rev,
        "hash": revision.hash,
        "size_bytes": source.len(),
        "kind": "http",
        "methods": trigger.methods,
        "path": trigger.path,
        "secrets": declared.secrets,
        "public": access,
        "via": via,
        "previous_via": previous_via,
        "state": declared.state,
        "objects": declared.objects.keys().collect::<Vec<_>>(),
        "limits": limits_json(state, &plan),
        "url": state.data_url(&route),
    }))
}

async fn push_function(
    State(state): Shared,
    headers: HeaderMap,
    Json(body): Json<PushBody>,
) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let methods = body.methods.clone().filter(|m| !m.is_empty());
    match deploy_function(
        &state,
        user_id,
        body.source,
        body.name,
        methods,
        body.path,
        "cli",
    )
    .await
    {
        Ok(value) => Json(value).into_response(),
        Err(refused) => err(refused.status, refused.code, refused.message),
    }
}

async fn list_functions(State(state): Shared, headers: HeaderMap) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let names = match state.store.names_for_user(user_id).await {
        Ok(names) => names,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store_error",
                e.to_string(),
            )
        }
    };
    let mut functions = Vec::with_capacity(names.len());
    for name in names {
        if let Ok(Some(record)) = state.store.get(&name).await {
            let current = record.current();
            let mut entry = json!({
                "name": name,
                "revision": current.rev,
                "hash": current.hash,
                "updated_at": current.created_at,
                "kind": record.kind,
                "state": record.state,
                "url": state.data_url(&format!("/f/{name}")),
            });
            if let Some(objects) = &record.objects {
                if let Some(bindings) = objects.as_object() {
                    entry["objects"] = json!(bindings.keys().collect::<Vec<_>>());
                }
            }
            if record.kind == "mcp" {
                if let Some(meta) = &record.mcp {
                    entry["tools"] = json!(mcp_tool_names(meta));
                    entry["public"] = meta["public"].clone();
                }
            }
            functions.push(entry);
        }
    }
    let now = now_epoch();
    let runs: Vec<_> = state
        .temp_runs
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, run)| run.expires_at > now)
        .map(|(id, run)| {
            json!({
                "id": id,
                "url": state.data_url(&format!("/r/{id}")),
                "expires_at": run.expires_at,
            })
        })
        .collect();
    Json(json!({ "functions": functions, "runs": runs })).into_response()
}

async fn function_detail(
    State(state): Shared,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    if !owns(&state, &name, user_id).await {
        return err(StatusCode::NOT_FOUND, "not_found", "no such function");
    }
    let Ok(record) = state.store.get(&name).await else {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            "lookup failed",
        );
    };
    let Some(record) = record else {
        return err(StatusCode::NOT_FOUND, "not_found", "no such function");
    };
    let source = if query.get("source").map(String::as_str) == Some("true") {
        match state.store.source(&name).await {
            Ok(s) => s,
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "store_error",
                    e.to_string(),
                )
            }
        }
    } else {
        None
    };
    let mut recent: Vec<InvocationRecord> = state
        .records
        .lock()
        .unwrap()
        .get(&name)
        .map(|ring| ring.iter().cloned().collect())
        .unwrap_or_default();
    // The ring is per-process and dies with every restart; right after a
    // deploy `rusted logs` would show nothing while Postgres holds the
    // history. Fall back to analytics — same shape, minus console output,
    // which only the ring keeps.
    if recent.is_empty() {
        recent = crate::analytics::recent(
            &state.pool,
            user_id,
            RECORD_CAP as i64,
            0,
            Some(&name),
            false,
        )
        .await
        .into_iter()
        .map(|row| InvocationRecord {
            at: row.at.max(0) as u64,
            outcome: row.outcome,
            detail: row.detail,
            wall_ms: row.wall_ms,
            cpu_ms: row.cpu_ms,
            status: row.status.map(|s| s as u16),
            logs: Vec::new(),
        })
        .collect();
    }
    let mut body = json!({
        "name": name,
        "revision": record.current().rev,
        "hash": record.current().hash,
        "revisions": record.revisions,
        "kind": record.kind,
        "secrets": record.secrets,
        "public": record.public,
        "state": record.state,
        "published": record.published,
        "objects": record.objects,
        "url": state.data_url(&format!("/f/{name}")),
        "recent": recent,
    });
    if record.kind == "mcp" {
        if let Some(meta) = &record.mcp {
            body["tools"] = json!(mcp_tool_names(meta));
            body["public"] = meta["public"].clone();
        }
    }
    if let Some(source) = source {
        body["source"] = json!(source);
    }
    Json(body).into_response()
}

async fn delete_function(
    State(state): Shared,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    if !owns(&state, &name, user_id).await {
        return err(StatusCode::NOT_FOUND, "not_found", "no such function");
    }
    let deleted = state.store.delete(&name).await;
    match deleted {
        Ok(true) => {
            prune_keys(&state, [name]);
            Json(json!({ "deleted": true })).into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "not_found", "no such function"),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct RunBody {
    source: String,
    ttl_seconds: Option<u64>,
}

async fn create_run(
    State(state): Shared,
    headers: HeaderMap,
    Json(body): Json<RunBody>,
) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, Some(user_id)).await;
    if let Err(e) = verify_source(&state, body.source.clone()).await {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "compile_error", e);
    }
    let ttl = body
        .ttl_seconds
        .unwrap_or(DEFAULT_RUN_TTL_SECS)
        .clamp(1, MAX_RUN_TTL_SECS);
    let seq = state.invoke_seq.fetch_add(1, Ordering::Relaxed);
    let digest = Sha256::digest(format!("{}:{seq}:{}", now_epoch(), body.source));
    let id = hex::encode(&digest[..6]);
    let expires_at = now_epoch() + ttl;
    let size_bytes = body.source.len();
    state.temp_runs.lock().unwrap().insert(
        id.clone(),
        TempRun {
            source: body.source,
            expires_at,
        },
    );
    Json(json!({
        "id": id,
        "url": state.data_url(&format!("/r/{id}")),
        "expires_at": expires_at,
        "size_bytes": size_bytes,
        "limits": limits_json(&state, &plan),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct InvokeBody {
    name: Option<String>,
    source: Option<String>,
    #[serde(default)]
    body: String,
    /// Environment to run in — scopes secrets and durable state, exactly as
    /// `/f/@env/name` does on the data plane. Deployed functions only.
    env: Option<String>,
}

async fn invoke(
    State(state): Shared,
    headers: HeaderMap,
    Json(body): Json<InvokeBody>,
) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    if let Some(name) = &body.name {
        if !owns(&state, name, user_id).await {
            return err(StatusCode::NOT_FOUND, "not_found", "no such function");
        }
    }
    let (key, source, fetched) = match (&body.name, body.source) {
        (Some(name), _) => {
            match state.store.fetch(name).await {
                // Invoke runs a module as http. An mcp module has no request
                // handler, so refuse the mismatch up front — pointing at the
                // endpoint an MCP client should connect to — rather than
                // letting it surface as a script error.
                Ok(Some(hit)) if hit.kind == "mcp" => {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "error": {
                            "code": "kind_mismatch",
                            "message": "an mcp function is invoked by its tools, not as an http function",
                            "url": state.data_url(&format!("/f/{name}")),
                        }})),
                    )
                        .into_response()
                }
                // Invoke is a bare POST to the function root. A function that
                // does not answer POST, or one that captures from a route
                // path, would silently exercise different behavior than its
                // deployed URL — refuse rather than misrepresent, pointing at
                // the address that is faithful.
                Ok(Some(hit)) if !hit.trigger.methods.iter().any(|m| m == "POST") => {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "error": {
                            "code": "method_mismatch",
                            "message": format!(
                                "invoke sends POST, but this function answers {} — call its URL instead",
                                hit.trigger.methods.join(", ")
                            ),
                            "url": state.data_url(&format!("/f/{name}")),
                        }})),
                    )
                        .into_response()
                }
                Ok(Some(hit)) if hit.trigger.path.is_some() => {
                    let pattern = hit.trigger.path.clone().unwrap_or_default();
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "error": {
                            "code": "path_mismatch",
                            "message": format!(
                                "this function serves the route {pattern}, which invoke cannot represent — call its URL instead"
                            ),
                            "url": state.data_url(&format!("/f/{name}{pattern}")),
                        }})),
                    )
                        .into_response()
                }
                Ok(Some(hit)) => (Some(name.clone()), hit.source.clone(), Some(hit)),
                Ok(None) => return err(StatusCode::NOT_FOUND, "not_found", "no such function"),
                Err(e) => {
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "store_error",
                        e.to_string(),
                    )
                }
            }
        }
        // Ad-hoc source: nothing shared to serialize on, nothing to record —
        // deliberately keyless so repeated invokes can't grow server state.
        (None, Some(source)) => (None, source, None),
        (None, None) => {
            return err(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "provide either name or source",
            )
        }
    };
    // The caller is the authenticated owner, so unlike the data plane's
    // 404-masking, a wrong environment gets told what is actually wrong.
    let env = match &body.env {
        None => crate::secrets::PROD_ENV.to_string(),
        Some(env) => {
            if key.is_none() {
                return err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "bad_request",
                    "env applies to deployed functions, not ad-hoc source",
                );
            }
            if env != crate::secrets::PROD_ENV
                && !crate::secrets::env_exists(&state.pool, user_id, env).await
            {
                return err(
                    StatusCode::NOT_FOUND,
                    "no_such_env",
                    format!("no such environment: {env} — create it in the console first"),
                );
            }
            env.clone()
        }
    };
    let request = HttpRequest::post_json(body.body);
    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, Some(user_id)).await;
    let limits = limits_for_plan(&state, &plan.limits);
    let executed = match (key, fetched) {
        (Some(key), Some(hit)) => {
            // The caller here is the owner, so the refusal carries the detail.
            let grant = match grant_for_function(&state, &key, &hit, &plan, &env).await {
                Ok(grant) => grant,
                Err((code, detail)) => {
                    record_refusal(&state, &key, hit.owner, "error", 500, detail.clone());
                    return err(StatusCode::INTERNAL_SERVER_ERROR, code, detail);
                }
            };
            execute_serialized(
                &state,
                &key,
                source,
                Job::Http(request),
                limits,
                Some(user_id),
                plan.limits.concurrency,
                grant,
            )
            .await
        }
        _ => {
            match execute_raw(
                &state,
                source,
                Job::Http(request),
                limits,
                Some(user_id),
                HostGrant::default(),
            )
            .await
            {
                Ok(result) => {
                    debug_print(&state, "ad-hoc", &result);
                    Ok(result)
                }
                Err(response) => Err(response),
            }
        }
    };
    let result = match executed {
        Ok(r) => r,
        Err(response) => return response,
    };
    Json(invocation_envelope(result)).into_response()
}

/// The owner-facing invocation envelope `/api/invoke` and the console editor
/// share: outcome, the reply as the deployed URL would answer it, logs,
/// timings — and for errors the JS stack, which only owner-authenticated
/// surfaces ever see.
pub(crate) fn invocation_envelope(result: InvocationResult) -> Value {
    let mut response = json!({
        "logs": result.logs,
        "wall_ms": result.wall.as_secs_f64() * 1000.0,
        "cpu_ms": result.cpu.as_secs_f64() * 1000.0,
    });
    match result.outcome {
        Outcome::Success(s) => {
            response["outcome"] = json!("success");
            // The same status, content type, and vetted headers the deployed
            // URL would have answered with — the full reply, not just its body.
            response["status"] = json!(result.status.unwrap_or(200));
            response["content_type"] = json!(response_content_type(result.content_type, &s));
            response["headers"] = json!(result.headers);
            response["response"] = json!(s);
        }
        Outcome::Terminated(reason) => {
            response["outcome"] = json!("terminated");
            response["reason"] = json!(reason);
        }
        Outcome::Error(message) => {
            response["outcome"] = json!("error");
            response["message"] = json!(message);
            if let Some(stack) = result.stack {
                response["stack"] = json!(stack);
            }
        }
    }
    response
}

/// Runs ad-hoc source as a bare POST under the caller's plan limits, with no
/// host grant — the console editor's Run button. Same execution and envelope
/// as `/api/invoke`'s ad-hoc branch.
pub(crate) async fn run_adhoc(
    state: &Arc<AppState>,
    user_id: Uuid,
    source: String,
    body: String,
) -> Response {
    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, Some(user_id)).await;
    let limits = limits_for_plan(state, &plan.limits);
    let request = HttpRequest::post_json(body);
    match execute_raw(
        state,
        source,
        Job::Http(request),
        limits,
        Some(user_id),
        HostGrant::default(),
    )
    .await
    {
        Ok(result) => {
            debug_print(state, "editor", &result);
            Json(invocation_envelope(result)).into_response()
        }
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct VerifyBody {
    source: String,
}

#[derive(Deserialize)]
struct DeviceStart {
    /// Names the key this becomes, so it's recognisable in the console.
    #[serde(default)]
    label: Option<String>,
}

async fn device_start(State(state): Shared, Json(body): Json<DeviceStart>) -> Response {
    // A shared bucket: this endpoint mints rows for anyone who asks.
    if state.rate_limiter.check("device-code", 60).is_err() {
        return err(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many sign-in attempts; try again shortly",
        );
    }
    let label = body
        .label
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| "cli".to_string());
    let label: String = label.chars().filter(|c| !c.is_control()).take(48).collect();
    match crate::device::start(&state.pool, &label).await {
        Ok(pending) => Json(json!({
            "device_code": pending.device_code,
            "user_code": pending.user_code,
            "verification_uri": state.console_url("/device"),
            "expires_in": crate::device::EXPIRY.as_secs(),
            "interval": crate::device::POLL_INTERVAL,
        }))
        .into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct DevicePoll {
    device_code: String,
}

async fn device_poll(State(state): Shared, Json(body): Json<DevicePoll>) -> Response {
    match crate::device::poll(&state.pool, &body.device_code).await {
        // RFC 8628 names these; the CLI keeps polling on the first.
        Ok(crate::device::Poll::Pending) => err(
            StatusCode::BAD_REQUEST,
            "authorization_pending",
            "waiting for approval",
        ),
        Ok(crate::device::Poll::Denied) => {
            err(StatusCode::BAD_REQUEST, "access_denied", "request declined")
        }
        Ok(crate::device::Poll::Expired) => err(
            StatusCode::BAD_REQUEST,
            "expired_token",
            "that code has expired — run the command again",
        ),
        Ok(crate::device::Poll::Approved(key)) => Json(json!({ "api_key": key })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            e.to_string(),
        ),
    }
}

/// The caller's plan. Local development resolves this in the background so it
/// can say "over your Pro plan" instead of guessing.
async fn current_plan(State(state): Shared, headers: HeaderMap) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, Some(user_id)).await;
    Json(json!({
        "code": plan.code,
        "name": plan.name,
        "version": plan.version,
        "limits": limits_json(&state, &plan),
    }))
    .into_response()
}

async fn verify(
    State(state): Shared,
    headers: HeaderMap,
    Json(body): Json<VerifyBody>,
) -> Response {
    if let Err(response) = caller(&state, &headers).await {
        return response;
    }
    match inspect_source(&state, body.source).await {
        Ok(inspection) => {
            let (kind, config) = match &inspection.surface {
                rusted_engine::Surface::Http(c) => ("http", json!(c)),
                rusted_engine::Surface::Mcp(c) => ("mcp", json!(c)),
            };
            Json(json!({
                "valid": true,
                "kind": kind,
                "config": config,
                "secrets": inspection.config.secrets,
                "state": inspection.config.wants_state(),
                "objects": inspection.config.objects.keys().collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::UNPROCESSABLE_ENTITY, "compile_error", e),
    }
}

// ------------------------------------------------------------------------ mcp
//
// One tool, deliberately. The prevailing MCP pattern hands a model dozens of
// tools whose schemas cost context before the conversation starts — an
// abstraction from when models could not write code. A model that writes
// JavaScript does not need a schema per capability; it needs `fetch` and
// somewhere safe to run. `execute` is that, and the sandbox is what makes
// handing it to a model defensible.
//
// Named `execute`, not `run`, because `rusted run` is the CLI's local dev
// server — the opposite of this, which runs on the server.

fn mcp_tools() -> Value {
    json!([
    {
        "name": "execute",
        "description":
            "Run JavaScript once on a remote sandbox and get back its result and console \
             output. Nothing is saved; use `deploy` for code that should stay reachable.\
             \n\nUse this to work with data without pulling it into context: fetch, filter, \
             aggregate or reshape at the source and return only the answer. A response too \
             large to read is fine here — return the few fields you actually need.\
             \n\nWrite one ES module:\n\
             export default async function handler(request, context) {\n\
             \u{0020} const input = await request.json();   // whatever you passed as `input`\n\
             \u{0020} const r = await fetch('https://api.example.com/thing');\n\
             \u{0020} const data = await r.json();\n\
             \u{0020} return context.json({ answer: data.items.length });\n\
             }\
             \n\nAvailable: fetch (http/https, text and JSON only), console.log/warn/error, \
             URL and URLSearchParams, TextEncoder/TextDecoder (utf-8), \
             and the ECMAScript standard library — JSON, RegExp, Date, Math, Map, Set, \
             Promise, BigInt, Proxy, Reflect, btoa/atob.\
             \n\nNot available, so do it by hand: setTimeout and timers of any kind, crypto, \
             Intl, Buffer, Blob, FormData, AbortController, and \
             the Headers/Request/Response classes — `fetch` takes and returns plain \
             objects here.\
             \n\nAlso not available: import — nothing is resolved at runtime, so send \
             self-contained code — node built-ins, the filesystem, processes, and addresses \
             on private networks. Execution is time- and memory-capped, so an endless loop \
             is stopped rather than hanging. Failures come back as the error message and \
             stack, which you can read and correct.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "The ES module to run." },
                "input": { "description": "Any JSON value; the handler reads it via request.json()." }
            },
            "required": ["code"]
        }
    },
    {
        "name": "deploy",
        "description":
            "Publish a JavaScript module at a stable HTTPS URL and return that URL. The \
             function stays live until deleted. What the module exports decides what it \
             becomes:\
             \n\nA module with `export default async function handler(request, context)` \
             (optionally `export const http = { name, methods, path, access }`) deploys \
             as an HTTP endpoint anyone can call — no key required — unless it declares \
             `access: \"private\"`, which demands the owner's API key on every call. Reach for this when \
             something needs an address rather than an answer: a webhook endpoint, an API \
             for someone else to call, or a callback URL for a service that will POST \
             back to you. The handler is written exactly as for `execute`, with the same \
             sandbox and the same limits. It runs per request, so `request.json()` is \
             whatever the caller sent.\
             \n\nA module with `export const mcp = { name, public, tools }` and no \
             default handler deploys as an MCP server, served at its /f/{name} URL. \
             `tools` maps each tool name to { description, inputSchema, handler }; \
             `inputSchema` is JSON Schema and arguments are validated against it before \
             the handler runs. Unless `public: true`, connecting requires the owner's \
             API key (Authorization: Bearer <key>). A module cannot export both surfaces.\
             \n\nDeploying the same name again replaces it and keeps the URL — including \
             switching a function between http and mcp. The reply includes the URL and \
             revision number, plus which methods it answers (http) or which tools it \
             serves (mcp).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "The ES module to publish." },
                "name": {
                    "type": "string",
                    "description": "1-64 chars of a-z, 0-9, '-', '_'. Becomes part of the URL. Optional if the code declares it in its `http` or `mcp` export."
                },
                "methods": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "HTTP methods it answers, e.g. [\"GET\",\"POST\"]. Defaults to POST. Http modules only — rejected for mcp modules."
                },
                "path": {
                    "type": "string",
                    "description": "Optional route nested under the function, e.g. '/users/{id}'. Captures arrive in request.params. Http modules only — rejected for mcp modules."
                }
            },
            "required": ["code"]
        }
    },
    {
        "name": "inbox_create",
        "description":
            "Create a throwaway URL that anyone can POST to, and get that URL back. \
             Use it to receive something you cannot fetch: an OAuth callback, a webhook \
             from Stripe or GitHub, a form submission, a reply from another agent, or a \
             'job finished' notification.\
             \n\nYou have no inbound address otherwise — this is how something on the \
             internet reaches you. Hand the URL out, then poll `inbox_read` by name until \
             it arrives. Holding the URL only allows writing; reading needs your key, so \
             giving it to a third party never gives them what they sent.\
             \n\nThe inbox expires and the URL stops working. Pick a ttl that covers the \
             wait and no more.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "How you will ask for it back. 1-64 chars of a-z, 0-9, '-', '_'. Reusing a name replaces the old inbox."
                },
                "ttl_seconds": {
                    "type": "integer",
                    "description": "How long it lives, from creation. Default 300, max 86400. Never extended by activity."
                },
                "store": {
                    "type": "string",
                    "enum": ["append", "upsert"],
                    "description": "'append' keeps every message (default). 'upsert' keeps only the most recent — use it when you want the latest value, like a single OAuth code."
                },
                "drain": {
                    "type": "boolean",
                    "description": "Delete the inbox on the first read that finds something, like taking a message off a queue. Default false. Note the message is then unrecoverable if the read fails in transit."
                }
            },
            "required": ["name"]
        }
    },
    {
        "name": "inbox_read",
        "description":
            "Read what has arrived at an inbox. Poll this after handing out the URL from \
             `inbox_create`.\
             \n\nAn empty `messages` array means the inbox is alive and nothing has \
             arrived yet — keep polling. An error saying the inbox is gone means it \
             expired or was drained, and waiting longer will not help.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The inbox name you chose." }
            },
            "required": ["name"]
        }
    },
    {
        "name": "list",
        "description":
            "List what exists on this account: deployed functions with their URLs and \
             methods, and any live inboxes with how long they have left. Use it to find \
             something from earlier, or to check a name before reusing it.",
        "inputSchema": { "type": "object", "properties": {} }
    },
    {
        "name": "delete",
        "description":
            "Remove a deployed function so its URL stops answering. Plans cap how many \
             functions an account may keep, so delete what is no longer needed.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The function to remove." }
            },
            "required": ["name"]
        }
    }
    ])
}

async fn mcp_execute(state: &Arc<AppState>, user_id: Uuid, args: &Value) -> Value {
    let Some(code) = args.get("code").and_then(|c| c.as_str()) else {
        return mcp_wire::tool_result("`code` is required and must be a string", true);
    };
    let input = match args.get("input") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "{}".to_string(),
    };

    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, Some(user_id)).await;
    if code.len() as i64 > plan.limits.max_script_bytes {
        return mcp_wire::tool_result(
            format!(
                "script is {} bytes; the {} plan allows {}",
                code.len(),
                plan.name,
                plan.limits.max_script_bytes
            ),
            true,
        );
    }
    let limits = limits_for_plan(state, &plan.limits);

    // Ad-hoc, so keyless: nothing is stored and nothing accumulates per call.
    let result = match execute_raw(
        state,
        code.to_string(),
        Job::Http(HttpRequest::post_json(input)),
        limits,
        Some(user_id),
        HostGrant::default(),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            return mcp_wire::tool_result("the server is at capacity; retry in a moment", true)
        }
    };

    let logs: Vec<String> = result
        .logs
        .iter()
        .map(|l| format!("[{}] {}", l.level, l.message))
        .collect();
    let ms = (result.wall.as_secs_f64() * 1000.0 * 100.0).round() / 100.0;

    match &result.outcome {
        rusted_engine::Outcome::Success(body) => {
            // Handlers almost always reply with context.json, so the body is
            // already JSON. Embed it as a value rather than a string: a model
            // should not have to unescape JSON nested inside JSON.
            let result = serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!(body));
            let mut out = json!({ "result": result, "ms": ms });
            if !logs.is_empty() {
                out["logs"] = json!(logs);
            }
            mcp_wire::tool_result(out.to_string(), false)
        }
        rusted_engine::Outcome::Terminated(reason) => mcp_wire::tool_result(
            json!({ "error": reason, "kind": "limit", "logs": logs, "ms": ms }).to_string(),
            true,
        ),
        rusted_engine::Outcome::Error(message) => {
            let mut out = json!({ "error": message, "kind": "script", "ms": ms });
            if !logs.is_empty() {
                out["logs"] = json!(logs);
            }
            if let Some(stack) = &result.stack {
                out["stack"] = json!(stack);
            }
            mcp_wire::tool_result(out.to_string(), true)
        }
    }
}

async fn mcp_deploy(state: &Arc<AppState>, user_id: Uuid, args: &Value) -> Value {
    let Some(code) = args.get("code").and_then(|c| c.as_str()) else {
        return mcp_wire::tool_result("`code` is required and must be a string", true);
    };
    let name = args
        .get("name")
        .and_then(|n| n.as_str())
        .map(|n| n.to_string());
    let methods = args
        .get("methods")
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .filter(|m| !m.is_empty());
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .map(|p| p.to_string());

    match deploy_function(
        state,
        user_id,
        code.to_string(),
        name,
        methods,
        path,
        "agent",
    )
    .await
    {
        Ok(value) => {
            // The URL is the reason to deploy, so lead with it and say plainly
            // that it is callable now.
            let url = value["url"].as_str().unwrap_or("").to_string();
            let mut out = json!({
                "url": url,
                "name": value["name"],
                "revision": value["revision"],
            });
            if value["kind"] == json!("mcp") {
                out["kind"] = json!("mcp");
                out["tools"] = value["tools"].clone();
                out["public"] = value["public"].clone();
                out["note"] = if value["public"] == json!(true) {
                    json!("live now as an MCP server at that URL, public, no key needed")
                } else {
                    json!("live now as an MCP server at that URL; connecting requires the owner's API key (Authorization: Bearer <key>)")
                };
            } else {
                out["methods"] = value["methods"].clone();
                out["note"] = json!("live now, callable by anyone, no key needed");
            }
            mcp_wire::tool_result(out.to_string(), false)
        }
        Err(refused) => mcp_wire::tool_result(
            json!({ "error": refused.message, "kind": refused.code }).to_string(),
            true,
        ),
    }
}

async fn mcp_list(state: &Arc<AppState>, user_id: Uuid) -> Value {
    let names = match state.store.names_for_user(user_id).await {
        Ok(names) => names,
        Err(e) => return mcp_wire::tool_result(format!("could not list functions: {e}"), true),
    };
    let mut functions = Vec::new();
    for name in names {
        if let Ok(Some(record)) = state.store.get(&name).await {
            let route = format!(
                "/f/{}{}",
                name,
                record.trigger.path.as_deref().unwrap_or("")
            );
            let mut entry = json!({
                "name": name,
                "url": state.data_url(&route),
                "kind": record.kind,
            });
            // An mcp function answers protocol, not HTTP methods: listing the
            // methods it would 405 misleads the model reading this.
            if record.kind == "mcp" {
                if let Some(meta) = &record.mcp {
                    entry["tools"] = json!(mcp_tool_names(meta));
                    entry["public"] = meta["public"].clone();
                }
            } else {
                entry["methods"] = json!(record.trigger.methods);
            }
            functions.push(entry);
        }
    }
    let inboxes = crate::inbox::list(state, user_id).await.unwrap_or_default();
    mcp_wire::tool_result(
        json!({ "functions": functions, "inboxes": inboxes }).to_string(),
        false,
    )
}

async fn mcp_delete(state: &Arc<AppState>, user_id: Uuid, args: &Value) -> Value {
    let Some(name) = args.get("name").and_then(|n| n.as_str()) else {
        return mcp_wire::tool_result("`name` is required and must be a string", true);
    };
    // Ownership, not existence: a function someone else deployed is not this
    // caller's to remove, and saying so would leak that it exists.
    if !owns(state, name, user_id).await {
        return mcp_wire::tool_result(format!("you have no function named '{name}'"), true);
    }
    match state.store.delete(name).await {
        Ok(_) => mcp_wire::tool_result(json!({ "deleted": name }).to_string(), false),
        Err(e) => mcp_wire::tool_result(format!("could not delete '{name}': {e}"), true),
    }
}

async fn mcp_inbox_create(state: &Arc<AppState>, user_id: Uuid, args: &Value) -> Value {
    match crate::inbox::create_endpoint(state, user_id, args.clone()).await {
        Ok(value) => mcp_wire::tool_result(value.to_string(), false),
        // The body of the refusal carries the reason; a model reads it and
        // corrects, so it must not be flattened to a status.
        Err(_) => mcp_wire::tool_result(
            json!({ "error": "could not create that inbox — check the name, ttl and store mode" })
                .to_string(),
            true,
        ),
    }
}

async fn mcp_inbox_read(state: &Arc<AppState>, user_id: Uuid, args: &Value) -> Value {
    let Some(name) = args.get("name").and_then(|n| n.as_str()) else {
        return mcp_wire::tool_result("`name` is required and must be a string", true);
    };
    match crate::inbox::read(state, user_id, name).await {
        Ok(crate::inbox::Reading::Alive { messages, drained }) => {
            let waiting = messages.is_empty();
            let mut out = json!({ "name": name, "messages": messages, "drained": drained });
            if waiting {
                // Said explicitly, because "empty" and "expired" lead to
                // opposite decisions and a model should not have to guess.
                out["note"] = json!("alive, nothing has arrived yet — poll again");
            }
            mcp_wire::tool_result(out.to_string(), false)
        }
        Ok(crate::inbox::Reading::Gone) => mcp_wire::tool_result(
            json!({ "error": format!("inbox '{name}' has expired, been drained, or never existed"),
                    "kind": "gone" })
            .to_string(),
            true,
        ),
        Err(e) => mcp_wire::tool_result(json!({ "error": e }).to_string(), true),
    }
}

/// One JSON-RPC message. `None` means it was a notification and wants no reply.
async fn mcp_dispatch(state: &Arc<AppState>, user_id: Uuid, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let ok = |result: Value| Some(mcp_wire::ok(id.clone(), result));

    match method {
        "initialize" => ok(json!({
            "protocolVersion": params.get("protocolVersion")
                .and_then(|v| v.as_str()).unwrap_or(mcp_wire::MCP_PROTOCOL),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "rusted", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({ "tools": mcp_tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match name {
                "execute" => ok(mcp_execute(state, user_id, &args).await),
                "deploy" => ok(mcp_deploy(state, user_id, &args).await),
                "list" => ok(mcp_list(state, user_id).await),
                "delete" => ok(mcp_delete(state, user_id, &args).await),
                "inbox_create" => ok(mcp_inbox_create(state, user_id, &args).await),
                "inbox_read" => ok(mcp_inbox_read(state, user_id, &args).await),
                other => ok(mcp_wire::tool_result(
                    format!("unknown tool: {other}"),
                    true,
                )),
            }
        }
        other => Some(mcp_wire::err(
            id.clone(),
            -32601,
            &format!("method not found: {other}"),
        )),
    }
}

/// The platform `/mcp` endpoint: authenticate the caller, then hand the wire
/// work (framing, batches, notifications, session echo) to [`mcp_wire`].
async fn mcp_endpoint(State(state): Shared, headers: HeaderMap, body: Bytes) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        // Not the generic 401: the WWW-Authenticate header is how a client with
        // no credentials discovers where to get them, and the spec requires it.
        Err(_) => return crate::oauth::unauthorized_challenge(&state),
    };
    let state = &state;
    mcp_wire::respond(&body, &headers, move |msg| async move {
        mcp_dispatch(state, user_id, &msg).await
    })
    .await
}

// ------------------------------------------------------------------ inboxes

async fn inbox_create(
    State(state): Shared,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    match crate::inbox::create_endpoint(&state, user_id, body).await {
        Ok(value) => Json(value).into_response(),
        Err(response) => response,
    }
}

async fn inbox_list(State(state): Shared, headers: HeaderMap) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    match crate::inbox::list(&state, user_id).await {
        Ok(inboxes) => Json(json!({ "inboxes": inboxes })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "store_error", e),
    }
}

async fn inbox_read(
    State(state): Shared,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    match crate::inbox::read(&state, user_id, &name).await {
        // Alive but empty is a 200, not a 410: an agent polling a fresh inbox
        // has to be able to tell "nothing yet" from "too late".
        Ok(crate::inbox::Reading::Alive { messages, drained }) => Json(json!({
            "name": name,
            "messages": messages,
            "drained": drained,
        }))
        .into_response(),
        Ok(crate::inbox::Reading::Gone) => err(
            StatusCode::GONE,
            "gone",
            "this inbox has expired, been drained, or never existed",
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "store_error", e),
    }
}

async fn inbox_delete(
    State(state): Shared,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    match crate::inbox::delete(&state, user_id, &name).await {
        Ok(true) => Json(json!({ "deleted": name })).into_response(),
        Ok(false) => err(StatusCode::GONE, "gone", "no such inbox"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "store_error", e),
    }
}

/// The explicit, separate deletion path for durable state. Scoped to the
/// caller's own account by construction, and deliberately independent of
/// whether the function still exists — state outlives delete/redeploy, so its
/// removal cannot depend on the function record being there.
async fn purge_function_state(
    State(state): Shared,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    match state.fnstate.purge(user_id, &name).await {
        Ok(removed) => Json(json!({ "purged_keys": removed })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "state_error", e),
    }
}

/// Per-function stats from the in-process OpenTelemetry pipeline, scoped to
/// the caller's own functions. Totals are cumulative across restarts via the
/// persisted baseline.
async fn stats(State(state): Shared, headers: HeaderMap) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let names = state
        .store
        .names_for_user(user_id)
        .await
        .unwrap_or_default();
    let functions = state.telemetry.snapshot(Some(&names));
    Json(json!({
        "source": "opentelemetry",
        "functions": functions,
    }))
    .into_response()
}

pub fn admin_router(state: Arc<AppState>) -> Router {
    Router::new()
        // Also served by the data router. Public OAuth discovery metadata
        // (RFC 9728) rides both listeners on purpose: which side of a
        // reverse proxy's path split a /.well-known/ URL lands on is
        // deployment-specific, and a 404 here cost a production integration
        // an afternoon.
        .route(
            "/.well-known/oauth-protected-resource/f/{name}",
            get(mcp_protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/f/{env}/{name}",
            get(mcp_protected_resource_env),
        )
        .route("/api/functions", post(push_function).get(list_functions))
        .route(
            "/api/functions/{name}",
            get(function_detail).delete(delete_function),
        )
        .route("/api/functions/{name}/state", delete(purge_function_state))
        .route("/api/runs", post(create_run))
        .route("/api/invoke", post(invoke))
        .route("/api/verify", post(verify))
        .route("/api/plan", get(current_plan))
        .route("/api/stats", get(stats))
        // Unauthenticated by necessity: a client starting the device flow has
        // no credential yet. Both are rate limited and every code expires.
        .route("/mcp", post(mcp_endpoint))
        .route("/api/inboxes", post(inbox_create).get(inbox_list))
        .route("/api/inboxes/{name}", get(inbox_read).delete(inbox_delete))
        .route("/api/device/code", post(device_start))
        .route("/api/device/token", post(device_poll))
        .layer(DefaultBodyLimit::max(ADMIN_BODY_LIMIT))
        .layer(axum::middleware::from_fn(envelope_errors))
        .with_state(state)
}

/// One sweep pass: drops expired temp runs (they're also checked lazily on
/// call) along with their per-key lock and record state.
pub fn sweep_once(state: &Arc<AppState>) {
    let now = now_epoch();
    let expired: Vec<String> = {
        let mut runs = state.temp_runs.lock().unwrap();
        let dead: Vec<String> = runs
            .iter()
            .filter(|(_, run)| run.expires_at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            runs.remove(id);
        }
        dead
    };
    if !expired.is_empty() {
        prune_keys(state, expired.into_iter().map(|id| format!("run:{id}")));
    }
}

pub async fn sweep_temp_runs(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        sweep_once(&state);
    }
}
