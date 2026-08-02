use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{any, get, post};
use axum::Router;
use rusted_engine::{Executor, HttpRequest, InvocationResult, Outcome};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

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

/// Runs `source` on a worker thread with no per-key state — used for ad-hoc
/// invocations so they can't leak locks or records. The wait for a worker slot
/// is bounded by the same queue-wait budget as the per-function lock.
async fn execute_raw(
    state: &Arc<AppState>,
    source: String,
    request: HttpRequest,
    limits: rusted_engine::Limits,
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
    // Handed to the execution runtime rather than run here: JavaScript between
    // await points blocks whichever thread drives it, and that must not be a
    // thread serving HTTP.
    Ok(state
        .exec_runtime
        .spawn(async move { executor.execute_async(&source, &request, &limits).await })
        .await
        .expect("executor task never panics"))
}

/// Runs `source` with concurrency 1 per `key` and records the invocation.
async fn execute_serialized(
    state: &Arc<AppState>,
    key: &str,
    source: String,
    request: HttpRequest,
    limits: rusted_engine::Limits,
    owner: Option<uuid::Uuid>,
    concurrency: usize,
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
    let result = execute_raw(state, source, request, limits).await?;
    debug_print(state, key, &result);

    let record = InvocationRecord {
        at: now_epoch(),
        outcome: match &result.outcome {
            Outcome::Success(_) => "success".into(),
            Outcome::Terminated(_) => "terminated".into(),
            Outcome::Error(_) => "error".into(),
        },
        detail: match &result.outcome {
            Outcome::Success(_) => None,
            Outcome::Terminated(reason) => Some(reason.clone()),
            Outcome::Error(message) => Some(message.clone()),
        },
        wall_ms: result.wall.as_secs_f64() * 1000.0,
        cpu_ms: result.cpu.as_secs_f64() * 1000.0,
        logs: result.logs.clone(),
    };
    // Queued, never awaited: analytics can shed load but never delay a call.
    state.analytics.record(crate::analytics::Invocation {
        function_name: key.to_string(),
        user_id: owner,
        outcome: record.outcome.clone(),
        detail: record.detail.clone(),
        wall_ms: record.wall_ms,
        cpu_ms: record.cpu_ms,
        exec_ms: result.exec_wall.as_secs_f64() * 1000.0,
    });

    let mut records = state.records.lock().unwrap();
    let ring = records.entry(key.to_string()).or_default();
    ring.push_front(record);
    ring.truncate(RECORD_CAP);

    Ok(result)
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
async fn plan_for_owner(
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

/// Maps an outcome to a data-plane HTTP response. Endpoint callers are third
/// parties: JS error messages and console logs never appear here — the owner
/// inspects them through the admin API (`recent`) or `rusted logs`.
fn outcome_to_http(result: InvocationResult) -> Response {
    match result.outcome {
        Outcome::Success(body) => {
            // context.json/context.text set an explicit type; bare returns are sniffed.
            let content_type = result.content_type.unwrap_or_else(|| {
                if serde_json::from_str::<serde_json::Value>(&body).is_ok() {
                    "application/json"
                } else {
                    "text/plain; charset=utf-8"
                }
                .to_string()
            });
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

async fn serve_function(
    state: Arc<AppState>,
    name: String,
    rest: Option<String>,
    method: Method,
    query: HashMap<String, String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Served through the store's read cache; NOTIFY events keep it fresh.
    let (source, trigger, owner) = match state.store.fetch(&name).await {
        Ok(Some(hit)) => (hit.1.clone(), hit.0.trigger.clone(), hit.0.user_id),
        Ok(None) => return err(StatusCode::NOT_FOUND, "not_found", "no such function"),
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store_error",
                e.to_string(),
            )
        }
    };
    if !trigger.methods.iter().any(|m| m == method.as_str()) {
        return err(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("allowed: {}", trigger.methods.join(", ")),
        );
    }
    let params = match (&trigger.path, rest.as_deref()) {
        (None, None) => BTreeMap::new(),
        (None, Some(_)) => {
            return err(
                StatusCode::NOT_FOUND,
                "not_found",
                "function has no sub-path",
            )
        }
        (Some(pattern), rest) => match match_path(pattern, rest.unwrap_or("")) {
            Some(params) => params,
            None => {
                return err(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("this function serves /f/{name}{pattern}"),
                )
            }
        },
    };
    let (plan, limits) = plan_for_owner(&state, owner).await;
    if let Err(retry_after) = state.rate_limiter.check(&name, plan.limits.rate_per_min) {
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
        Err(response) => return *response,
    };
    match execute_serialized(
        &state,
        &name,
        source,
        request,
        limits,
        owner,
        plan.limits.concurrency,
    )
    .await
    {
        Ok(result) => outcome_to_http(result),
        Err(response) => response,
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
        request,
        limits,
        None,
        plan.limits.concurrency,
    )
    .await
    {
        Ok(result) => outcome_to_http(result),
        Err(response) => response,
    }
}

/// With `--require-auth`, every data-plane call needs a valid API key. The
/// verdict comes from the in-memory cache (see [`crate::auth`]) — steady-state
/// traffic never touches Postgres.
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

pub fn data_router(state: Arc<AppState>) -> Router {
    Router::new()
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
    #[serde(default, rename = "type")]
    trigger_type: Option<String>,
    #[serde(default)]
    methods: Option<Vec<String>>,
    #[serde(default)]
    path: Option<String>,
}

/// Compile-checks the source and reads its `export const config` declaration.
async fn inspect_source(
    state: &Arc<AppState>,
    source: String,
) -> Result<rusted_engine::FileConfig, String> {
    let executor = state.executor.clone();
    tokio::task::spawn_blocking(move || executor.inspect(&source))
        .await
        .expect("inspect thread never panics")
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
    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, Some(user_id)).await;
    if body.source.len() as i64 > plan.limits.max_script_bytes {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "plan_limit",
            format!(
                "script is {} bytes; the {} plan allows {} — upgrade at /console/billing",
                body.source.len(),
                plan.name,
                plan.limits.max_script_bytes
            ),
        );
    }
    if let Some(t) = &body.trigger_type {
        if t != "http" {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_trigger",
                format!("trigger type {t} is not supported yet; only http"),
            );
        }
    }
    // Compile check + read `export const config` from the file in one pass.
    let file_config = match inspect_source(&state, body.source.clone()).await {
        Ok(cfg) => cfg,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, "compile_error", e),
    };
    // Explicit request fields express fresher intent than the file config.
    let Some(name) = body.name.clone().or(file_config.name) else {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "missing_name",
            "provide --name or declare it in `export const config = { name: ... }`",
        );
    };
    if !valid_name(&name) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_name",
            "names are 1-64 chars of a-z, 0-9, '-', '_'",
        );
    }
    // Replacing an existing function doesn't consume another slot.
    let replacing = matches!(state.store.owner(&name).await, Ok(Some(owner)) if owner == user_id);
    if !replacing {
        let owned = state.store.count_for_user(user_id).await.unwrap_or(0);
        if owned >= plan.limits.max_functions {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "plan_limit",
                format!(
                    "you have {owned} functions; the {} plan allows {} — upgrade at /console/billing",
                    plan.name, plan.limits.max_functions
                ),
            );
        }
        if let Ok(Some(other)) = state.store.owner(&name).await {
            let _ = other;
            return err(
                StatusCode::CONFLICT,
                "name_taken",
                "another account already deployed a function with that name",
            );
        }
    }
    let methods = body.methods.clone().or(file_config.methods);
    let path = body.path.clone().or(file_config.path);
    // A push that names no trigger fields (anywhere) keeps the function's
    // existing route; naming any of them replaces the whole trigger config.
    let new_trigger = if methods.is_some() || path.is_some() {
        let methods: Vec<String> = methods
            .unwrap_or_else(|| vec!["POST".to_string()])
            .iter()
            .map(|m| m.to_uppercase())
            .collect();
        if let Err(e) = validate_trigger(&methods, &path) {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "invalid_trigger", e);
        }
        Some(crate::store::HttpTrigger { methods, path })
    } else {
        None
    };
    let pushed = match new_trigger {
        Some(trigger) => {
            state
                .store
                .push_with_trigger(&name, &body.source, trigger, Some(user_id))
                .await
        }
        None => state.store.push(&name, &body.source, Some(user_id)).await,
    };
    let pushed = match pushed {
        Ok(revision) => match state.store.get(&name).await {
            Ok(Some(record)) => Ok((revision, record.trigger)),
            Ok(None) => Err(sqlx::Error::RowNotFound),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    match pushed {
        Ok((revision, trigger)) => {
            let route = format!("/f/{}{}", name, trigger.path.as_deref().unwrap_or(""));
            Json(json!({
                "name": name,
                "revision": revision.rev,
                "hash": revision.hash,
                "size_bytes": body.source.len(),
                "methods": trigger.methods,
                "path": trigger.path,
                "limits": limits_json(&state, &plan),
                "url": state.data_url(&route),
            }))
            .into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            e.to_string(),
        ),
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
            functions.push(json!({
                "name": name,
                "revision": current.rev,
                "hash": current.hash,
                "updated_at": current.created_at,
                "url": state.data_url(&format!("/f/{name}")),
            }));
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
    let recent: Vec<InvocationRecord> = state
        .records
        .lock()
        .unwrap()
        .get(&name)
        .map(|ring| ring.iter().cloned().collect())
        .unwrap_or_default();
    let mut body = json!({
        "name": name,
        "revision": record.current().rev,
        "hash": record.current().hash,
        "revisions": record.revisions,
        "url": state.data_url(&format!("/f/{name}")),
        "recent": recent,
    });
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
    let (key, source) = match (&body.name, body.source) {
        (Some(name), _) => match state.store.source(name).await {
            Ok(Some(s)) => (Some(name.clone()), s),
            Ok(None) => return err(StatusCode::NOT_FOUND, "not_found", "no such function"),
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "store_error",
                    e.to_string(),
                )
            }
        },
        // Ad-hoc source: nothing shared to serialize on, nothing to record —
        // deliberately keyless so repeated invokes can't grow server state.
        (None, Some(source)) => (None, source),
        (None, None) => {
            return err(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "provide either name or source",
            )
        }
    };
    let request = HttpRequest::post_json(body.body);
    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, Some(user_id)).await;
    let limits = limits_for_plan(&state, &plan.limits);
    let executed = match key {
        Some(key) => {
            execute_serialized(
                &state,
                &key,
                source,
                request,
                limits,
                Some(user_id),
                plan.limits.concurrency,
            )
            .await
        }
        None => match execute_raw(&state, source, request, limits).await {
            Ok(result) => {
                debug_print(&state, "ad-hoc", &result);
                Ok(result)
            }
            Err(response) => Err(response),
        },
    };
    let result = match executed {
        Ok(r) => r,
        Err(response) => return response,
    };
    let mut response = json!({
        "logs": result.logs,
        "wall_ms": result.wall.as_secs_f64() * 1000.0,
        "cpu_ms": result.cpu.as_secs_f64() * 1000.0,
    });
    match result.outcome {
        Outcome::Success(s) => {
            response["outcome"] = json!("success");
            response["response"] = json!(s);
        }
        Outcome::Terminated(reason) => {
            response["outcome"] = json!("terminated");
            response["reason"] = json!(reason);
        }
        Outcome::Error(message) => {
            response["outcome"] = json!("error");
            response["message"] = json!(message);
        }
    }
    Json(response).into_response()
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
        Ok(config) => Json(json!({ "valid": true, "config": config })).into_response(),
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

const MCP_PROTOCOL: &str = "2025-06-18";

fn mcp_tools() -> Value {
    json!([{
        "name": "execute",
        "description":
            "Run JavaScript on a remote sandbox and get back its result and console output. \
             \n\nUse this to work with data without pulling it into context: fetch, filter, \
             aggregate or reshape at the source and return only the answer. A response too \
             large to read is fine here — return the few fields you actually need.\
             \n\nWrite one ES module:\n\
             export default async function handler(request, context) {\n\
             \u{0020}const input = await request.json();   // whatever you passed as `input`\n\
             \u{0020}const r = await fetch('https://api.example.com/thing');\n\
             \u{0020}const data = await r.json();\n\
             \u{0020}return context.json({ answer: data.items.length });\n\
             }\
             \n\nAvailable: fetch (http/https, text and JSON only), console.log/warn/error, \
             and standard ES2020 — JSON, RegExp, Date, Math, Intl.\
             \n\nNot available: import (send self-contained code; nothing is resolved at \
             runtime), node built-ins, the filesystem, processes, and addresses on private \
             networks. Execution is time- and memory-capped, so an endless loop is stopped \
             rather than hanging. Failures come back as the error message and stack, which \
             you can read and correct.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "The ES module to run." },
                "input": { "description": "Any JSON value; the handler reads it via request.json()." }
            },
            "required": ["code"]
        }
    }])
}

/// Tool failures come back as results, not JSON-RPC errors: that is how the
/// model sees what broke and fixes its own code.
fn mcp_tool_result(text: String, is_error: bool) -> Value {
    let mut result = json!({ "content": [{ "type": "text", "text": text }] });
    if is_error {
        result["isError"] = json!(true);
    }
    result
}

async fn mcp_execute(state: &Arc<AppState>, user_id: Uuid, args: &Value) -> Value {
    let Some(code) = args.get("code").and_then(|c| c.as_str()) else {
        return mcp_tool_result("`code` is required and must be a string".into(), true);
    };
    let input = match args.get("input") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "{}".to_string(),
    };

    let plan = crate::plans::effective_plan(&state.pool, &state.plan_cache, Some(user_id)).await;
    if code.len() as i64 > plan.limits.max_script_bytes {
        return mcp_tool_result(
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
        HttpRequest::post_json(input),
        limits,
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            return mcp_tool_result("the server is at capacity; retry in a moment".into(), true)
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
            mcp_tool_result(out.to_string(), false)
        }
        rusted_engine::Outcome::Terminated(reason) => mcp_tool_result(
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
            mcp_tool_result(out.to_string(), true)
        }
    }
}

/// One JSON-RPC message. `None` means it was a notification and wants no reply.
async fn mcp_dispatch(state: &Arc<AppState>, user_id: Uuid, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let ok = |result: Value| Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }));

    match method {
        "initialize" => ok(json!({
            "protocolVersion": params.get("protocolVersion")
                .and_then(|v| v.as_str()).unwrap_or(MCP_PROTOCOL),
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
                other => ok(mcp_tool_result(format!("unknown tool: {other}"), true)),
            }
        }
        other => Some(json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("method not found: {other}") }
        })),
    }
}

/// MCP over Streamable HTTP in JSON response mode — the spec allows answering a
/// POST with one `application/json` body instead of an SSE stream, so no
/// session or streaming machinery is needed.
async fn mcp_endpoint(State(state): Shared, headers: HeaderMap, body: Bytes) -> Response {
    let user_id = match caller(&state, &headers).await {
        Ok(user_id) => user_id,
        // Not the generic 401: the WWW-Authenticate header is how a client with
        // no credentials discovers where to get them, and the spec requires it.
        Err(_) => return crate::oauth::unauthorized_challenge(&state),
    };
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "jsonrpc": "2.0", "id": null,
                         "error": { "code": -32700, "message": "parse error" } })),
        )
            .into_response();
    };

    let replies: Vec<Value> = match &message {
        Value::Array(batch) => {
            let mut out = Vec::new();
            for m in batch {
                if let Some(reply) = mcp_dispatch(&state, user_id, m).await {
                    out.push(reply);
                }
            }
            out
        }
        single => mcp_dispatch(&state, user_id, single)
            .await
            .into_iter()
            .collect(),
    };

    // Nothing asked for an answer — say so the way the spec does.
    if replies.is_empty() {
        return (StatusCode::ACCEPTED, "").into_response();
    }

    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("stateless")
        .to_string();
    let payload = if message.is_array() {
        Value::Array(replies)
    } else {
        replies.into_iter().next().expect("one reply")
    };
    ([("mcp-session-id", session)], Json(payload)).into_response()
}

pub fn admin_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/functions", post(push_function).get(list_functions))
        .route(
            "/api/functions/{name}",
            get(function_detail).delete(delete_function),
        )
        .route("/api/runs", post(create_run))
        .route("/api/invoke", post(invoke))
        .route("/api/verify", post(verify))
        .route("/api/plan", get(current_plan))
        // Unauthenticated by necessity: a client starting the device flow has
        // no credential yet. Both are rate limited and every code expires.
        .route("/mcp", post(mcp_endpoint))
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
