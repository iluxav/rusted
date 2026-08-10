//! Single-function local development server: the same engine, glue, and route
//! matching the deployed data plane uses, with no database, no auth, and no
//! plan lookups. Reloads the file when it changes and reports failures to the
//! terminal instead of hiding them behind a generic 500.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::any;
use axum::Router;
use rusted_engine::{Executor, HttpRequest, Limits, Outcome, QuickJsExecutor};
use serde_json::json;

use crate::api::match_path;
use crate::state::REQUEST_BODY_LIMIT;
use crate::store::HttpTrigger;

const POLL_INTERVAL: Duration = Duration::from_millis(300);

pub struct LocalConfig {
    /// Admin API and key, when the developer has them. Used only to look up
    /// which plan they're on, in the background, after the server is up.
    pub admin: Option<(String, String)>,
    /// The file you point at — your source, whether or not it needs bundling.
    pub entry: PathBuf,
    /// Extra paths to watch; a change here triggers the build, then a reload.
    /// Defaults to the entry's directory when bundling, else the file itself.
    pub watch: Vec<PathBuf>,
    /// Overrides the built-in bundling with your own command.
    pub build: Option<String>,
    pub port: u16,
    pub limits: Limits,
}

/// How the served source is produced. Resolved once from [`LocalConfig`].
enum Source {
    /// Bundled in-process from the entry — no external tooling involved.
    Bundled,
    /// Produced by the developer's own command, then read from `serve_path`.
    Command(String),
    /// Read straight from disk.
    File,
}

struct Pipeline {
    serve_path: PathBuf,
    source: Source,
    watch: Vec<PathBuf>,
    note: Option<String>,
}

fn resolve_pipeline(config: &LocalConfig) -> Result<Pipeline, String> {
    // An explicit --build wins: your pipeline, your rules.
    if let Some(build) = &config.build {
        return Ok(Pipeline {
            serve_path: config.entry.clone(),
            source: Source::Command(build.clone()),
            watch: if config.watch.is_empty() {
                vec![config.entry.clone()]
            } else {
                config.watch.clone()
            },
            note: None,
        });
    }
    let source = std::fs::read_to_string(&config.entry)
        .map_err(|e| format!("cannot read {}: {e}", config.entry.display()))?;
    if !crate::bundler::should_bundle(&config.entry, &source) {
        return Ok(Pipeline {
            serve_path: config.entry.clone(),
            source: Source::File,
            watch: if config.watch.is_empty() {
                vec![config.entry.clone()]
            } else {
                config.watch.clone()
            },
            note: None,
        });
    }
    // A bare filename's parent is "", which watches nothing.
    let dir = match config.entry.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Ok(Pipeline {
        serve_path: config.entry.clone(),
        source: Source::Bundled,
        watch: if config.watch.is_empty() {
            vec![dir.to_path_buf()]
        } else {
            config.watch.clone()
        },
        note: Some(format!("bundling {}", config.entry.display())),
    })
}

/// Which surface the loaded module declares, resolved at inspect time.
#[derive(Clone)]
enum Served {
    Http(HttpTrigger),
    /// The same metadata shape the deployed server stores
    /// (`{"public": ..., "tools": {...}}`), so `mcp_host` reads it
    /// identically. `public` is carried but not enforced: local is trusted.
    Mcp(serde_json::Value),
}

struct Loaded {
    source: String,
    name: String,
    surface: Served,
    /// Maps bundled positions back to your files, so a stack frame names
    /// `index.js:12` rather than `handler:2919`.
    sourcemap: Option<Arc<sourcemap::SourceMap>>,
    /// Where the map's relative sources resolve from (the bundle's directory).
    sourcemap_base: PathBuf,
}

struct LocalState {
    loaded: RwLock<Loaded>,
    /// Reload counter, reported as `serverInfo.version` (`rev-N`) so an MCP
    /// client can tell whether it is talking to the file it just saved.
    rev: std::sync::atomic::AtomicU64,
    /// Shared so the bytecode cache stays warm across requests.
    executor: Arc<QuickJsExecutor>,
    limits: Limits,
    /// The caller's plan code, resolved in the background when an API key is
    /// available. None until (or unless) that lands — warnings fall back to
    /// naming the cheapest tier that would work.
    plan: RwLock<Option<String>>,
}

/// Reads the entry file and its `export const http`, falling back to the
/// file stem for the name and POST for the method.
/// Produces the source for the current pipeline: bundled in process, or read
/// from disk after the developer's own build.
async fn produce(pipeline: &Pipeline) -> Result<(String, Option<String>), String> {
    match &pipeline.source {
        Source::Bundled => {
            let bundled = crate::bundler::bundle(&pipeline.serve_path, true).await?;
            Ok((bundled.code, bundled.sourcemap))
        }
        Source::Command(command) => {
            run_build(command)?;
            Ok((read_file(&pipeline.serve_path)?, None))
        }
        Source::File => Ok((read_file(&pipeline.serve_path)?, None)),
    }
}

fn read_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "{} does not exist — check that your build writes there",
                path.display()
            )
        } else {
            format!("cannot read {}: {e}", path.display())
        }
    })
}

fn load(
    entry: &Path,
    source: String,
    sourcemap_json: Option<String>,
    executor: &QuickJsExecutor,
) -> Result<Loaded, String> {
    let stem = entry
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "function".to_string());
    let inspection = executor.inspect(&source)?;
    // Local mode has no database, so there is nothing to decrypt from —
    // context.env stays undefined, and the handler fails saying so if it reads
    // it. Saying it up front beats a puzzling undefined at request time.
    if !inspection.config.secrets.is_empty() {
        println!(
            "\x1b[38;5;180m▸ this module requests secrets ({}); local mode does not inject \
             them, so context.env is undefined\x1b[0m",
            inspection.config.secrets.join(", ")
        );
    }
    let (name, surface) = match inspection.surface {
        rusted_engine::Surface::Http(config) => (
            config.name.unwrap_or(stem),
            Served::Http(HttpTrigger {
                methods: config
                    .methods
                    .map(|m| m.iter().map(|s| s.to_uppercase()).collect())
                    .unwrap_or_else(|| vec!["POST".to_string()]),
                path: config.path,
            }),
        ),
        rusted_engine::Surface::Mcp(config) => (
            config.name.clone().unwrap_or(stem),
            Served::Mcp(json!({ "public": config.public, "tools": config.tools })),
        ),
    };
    let sourcemap = sourcemap_json
        .and_then(|json| sourcemap::SourceMap::from_reader(json.as_bytes()).ok())
        .map(Arc::new);
    Ok(Loaded {
        source,
        name,
        surface,
        sourcemap,
        // The map's sources are relative to the entry's own directory.
        sourcemap_base: match entry.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        },
    })
}

fn run_build(command: &str) -> Result<(), String> {
    println!("\x1b[38;5;180m▸ {command}\x1b[0m");
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .status()
        .map_err(|e| format!("cannot run build command: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("build failed ({status})"))
    }
}

/// Newest mtime across the watched paths, skipping the usual noise.
fn newest_mtime(paths: &[PathBuf]) -> SystemTime {
    fn walk(path: &Path, newest: &mut SystemTime) {
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        if meta.is_file() {
            if let Ok(modified) = meta.modified() {
                if modified > *newest {
                    *newest = modified;
                }
            }
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "node_modules" || name == ".git" || name.starts_with('.') {
                continue;
            }
            walk(&entry.path(), newest);
        }
    }
    let mut newest = SystemTime::UNIX_EPOCH;
    for path in paths {
        walk(path, &mut newest);
    }
    newest
}

pub async fn serve(config: LocalConfig) -> Result<(), String> {
    let executor = Arc::new(QuickJsExecutor::new());
    let pipeline = resolve_pipeline(&config)?;
    if let Some(note) = &pipeline.note {
        println!("\x1b[38;5;138m{note}\x1b[0m");
    }
    let (source, sourcemap) = produce(&pipeline).await?;
    let loaded = load(&config.entry, source, sourcemap, &executor)?;
    let name = loaded.name.clone();
    let surface = loaded.surface.clone();
    let state = Arc::new(LocalState {
        loaded: RwLock::new(loaded),
        rev: std::sync::atomic::AtomicU64::new(1),
        executor: executor.clone(),
        limits: config.limits.clone(),
        plan: RwLock::new(None),
    });

    let app = Router::new()
        .route("/f/{name}", any(handle_root))
        .route("/f/{name}/{*rest}", any(handle_sub))
        .layer(DefaultBodyLimit::max(REQUEST_BODY_LIMIT))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.port))
        .await
        .map_err(|e| format!("cannot bind port {}: {e}", config.port))?;
    let addr: SocketAddr = listener.local_addr().map_err(|e| e.to_string())?;

    println!("\x1b[1;38;5;209mrusted\x1b[0m dev server");
    match &surface {
        Served::Http(trigger) => {
            let route = format!("/f/{name}{}", trigger.path.as_deref().unwrap_or(""));
            let methods = trigger.methods.join(",");
            println!("  \x1b[38;5;209m{methods}\x1b[0m http://{addr}{route}");
        }
        Served::Mcp(meta) => {
            // The deliverable is the same as a deployed push: a config block
            // ready to paste into an MCP client — minus the auth header,
            // because local serving is trusted.
            let tools = meta["tools"]
                .as_object()
                .map(|t| t.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            println!("  \x1b[38;5;209mMCP\x1b[0m http://{addr}/f/{name}  (tools: {tools})");
            println!("  add to your MCP client config:");
            println!("  {{");
            println!("    \"mcpServers\": {{");
            println!("      \"{name}\": {{ \"url\": \"http://{addr}/f/{name}\" }}");
            println!("    }}");
            println!("  }}");
            let public = meta["public"].as_bool().unwrap_or(false);
            if !public {
                println!("  after `rusted push`: same block plus your API key in an Authorization header");
            }
        }
    }
    println!(
        "  limits: {} wall · {} outbound — the most any plan allows; yours may allow less",
        human_ms(config.limits.wall_ms),
        config.limits.outbound.max_requests,
    );
    println!(
        "  watching {}",
        pipeline
            .watch
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  ctrl-c to stop\n");

    if let Some((admin, key)) = config.admin.clone() {
        tokio::spawn(resolve_plan(state.clone(), admin, key));
    }
    tokio::spawn(watch_loop(state, config.entry.clone(), pipeline));

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("server stopped: {e}"))
}

/// Asks the server which plan the developer is on. Runs after the listener is
/// already accepting requests, so a slow or absent server costs nothing: until
/// this lands (or if it never does), warnings name the cheapest tier that fits
/// instead of the developer's own plan.
async fn resolve_plan(state: Arc<LocalState>, admin: String, key: String) {
    let url = format!("{}/api/plan", admin.trim_end_matches('/'));
    let Ok(response) = reqwest::Client::new()
        .get(&url)
        .bearer_auth(&key)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    else {
        return;
    };
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return;
    };
    if let Some(code) = body.get("code").and_then(|c| c.as_str()) {
        let name = body
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or(code)
            .to_string();
        *state.plan.write().unwrap() = Some(code.to_string());
        println!(
            "\x1b[38;5;138myou're on the {name} plan — warnings below are against it\x1b[0m\n"
        );
    }
}

/// Polls mtimes; on a change runs the build (if any) and swaps in the new
/// source. A failure leaves the previous version serving.
async fn watch_loop(state: Arc<LocalState>, entry: PathBuf, pipeline: Pipeline) {
    let mut last = newest_mtime(&pipeline.watch);
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let current = newest_mtime(&pipeline.watch);
        if current <= last {
            continue;
        }
        last = current;
        // Let the writer finish before reading a half-written file.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let produced = match produce(&pipeline).await {
            Ok(produced) => produced,
            Err(e) => {
                println!("\x1b[1;38;5;203m✗ {e}\x1b[0m — still serving the last good version\n");
                continue;
            }
        };
        last = newest_mtime(&pipeline.watch);
        match load(&entry, produced.0, produced.1, &state.executor) {
            Ok(next) => {
                let bytes = next.source.len();
                // Name the surface so an accidental flip (an mcp export
                // deleted, a default handler added) is visible at the moment
                // it happens, not when a client starts getting nonsense.
                let surface = match &next.surface {
                    Served::Http(_) => "http".to_string(),
                    Served::Mcp(meta) => {
                        let tools = meta["tools"]
                            .as_object()
                            .map(|t| t.keys().cloned().collect::<Vec<_>>().join(", "))
                            .unwrap_or_default();
                        format!("mcp, tools: {tools}")
                    }
                };
                println!(
                    "\x1b[38;5;150m✓ reloaded {} ({bytes} bytes, {surface})\x1b[0m\n",
                    next.name
                );
                *state.loaded.write().unwrap() = next;
                state.rev.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                println!("\x1b[1;38;5;203m✗ {e}\x1b[0m — still serving the last good version\n");
            }
        }
    }
}

async fn handle_root(
    State(state): State<Arc<LocalState>>,
    AxumPath(name): AxumPath<String>,
    method: Method,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(state, name, None, method, query, headers, body).await
}

async fn handle_sub(
    State(state): State<Arc<LocalState>>,
    AxumPath((name, rest)): AxumPath<(String, String)>,
    method: Method,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(state, name, Some(rest), method, query, headers, body).await
}

async fn dispatch(
    state: Arc<LocalState>,
    name: String,
    rest: Option<String>,
    method: Method,
    query: std::collections::HashMap<String, String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (source, expected_name, surface, sourcemap, sourcemap_base) = {
        let loaded = state.loaded.read().unwrap();
        (
            loaded.source.clone(),
            loaded.name.clone(),
            loaded.surface.clone(),
            loaded.sourcemap.clone(),
            loaded.sourcemap_base.clone(),
        )
    };
    if name != expected_name {
        return problem(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("this server serves /f/{expected_name}"),
        );
    }
    let trigger = match surface {
        Served::Http(trigger) => trigger,
        // Every POST to the root is protocol; the messages inside decide what
        // happens — the same split the deployed data plane makes.
        Served::Mcp(meta) => {
            if rest.is_some() {
                return problem(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("an mcp function serves only /f/{expected_name}"),
                );
            }
            if method != Method::POST {
                return problem(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "allowed: POST".to_string(),
                );
            }
            return dispatch_mcp(
                &state,
                &meta,
                &expected_name,
                &source,
                &headers,
                &body,
                sourcemap.as_deref(),
                &sourcemap_base,
            )
            .await;
        }
    };
    if !trigger.methods.iter().any(|m| m == method.as_str()) {
        return problem(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("allowed: {}", trigger.methods.join(", ")),
        );
    }
    let params = match (&trigger.path, rest.as_deref()) {
        (None, None) => Default::default(),
        (None, Some(_)) => {
            return problem(
                StatusCode::NOT_FOUND,
                "not_found",
                "this function has no sub-path".to_string(),
            )
        }
        (Some(pattern), rest) => match match_path(pattern, rest.unwrap_or("")) {
            Some(params) => params,
            None => {
                return problem(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("this function serves /f/{expected_name}{pattern}"),
                )
            }
        },
    };
    // Same refusal as the deployed server: a body that is not UTF-8 is rejected
    // rather than coerced, so `rusted run` cannot accept locally what the server
    // would turn away. See `api::to_engine_request` for why coercion is wrong.
    let body = match String::from_utf8(body.to_vec()) {
        Ok(body) => body,
        Err(e) => {
            let at = e.utf8_error().valid_up_to();
            return problem(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                format!(
                    "request body is not valid UTF-8 (first bad byte at offset {at} of {}). \
                     Functions receive the body as text, so binary payloads cannot be passed \
                     through unchanged — send them base64-encoded inside JSON instead.",
                    e.as_bytes().len()
                ),
            );
        }
    };
    let request = HttpRequest {
        method: method.as_str().to_string(),
        headers: headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|value| (k.as_str().to_string(), value.to_string()))
            })
            .collect(),
        query: query.into_iter().collect(),
        params,
        body,
    };

    let executor = state.executor.clone();
    let limits = state.limits.clone();
    let script_bytes = source.len();
    // The same async path the deployed server uses, so what you see locally is
    // what happens in production — including fetches overlapping rather than
    // holding the thread.
    let result = executor.execute_async(&source, &request, &limits).await;

    let stack = result
        .stack
        .as_deref()
        .map(|stack| remap_stack(stack, sourcemap.as_deref(), &sourcemap_base));
    let usage = crate::tiers::Usage {
        exec_ms: result.exec_wall.as_millis() as u64,
        outbound_reqs: result.outbound_used,
        script_bytes,
    };
    let plan = state.plan.read().unwrap().clone();
    let advisory = crate::tiers::warning(&usage, plan.as_deref());
    report(
        &method,
        rest.as_deref(),
        &result,
        stack.as_deref(),
        advisory.as_deref(),
    );

    // Unlike the deployed data plane, local dev returns the real error: the
    // only caller is the developer who wrote it.
    match &result.outcome {
        Outcome::Success(body) => {
            let body = body.clone();
            let content_type = result.content_type.clone().unwrap_or_else(|| {
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
        Outcome::Terminated(reason) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": { "code": "limit_exceeded", "message": reason } })),
        )
            .into_response(),
        Outcome::Error(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": { "code": "function_error", "message": message },
                "stack": stack,
                "logs": result.logs,
            })),
        )
            .into_response(),
    }
}

/// Answers one MCP POST locally: the same message dispatch and outcome
/// mapping as the deployed server, with no auth (local is trusted), no rate
/// limit, and the local limits. `serverInfo.version` is the reload counter,
/// so a client can tell it is talking to the file it just saved.
#[allow(clippy::too_many_arguments)]
async fn dispatch_mcp(
    state: &Arc<LocalState>,
    meta: &serde_json::Value,
    name: &str,
    source: &str,
    headers: &HeaderMap,
    body: &Bytes,
    sourcemap: Option<&sourcemap::SourceMap>,
    sourcemap_base: &Path,
) -> Response {
    let rev = state.rev.load(std::sync::atomic::Ordering::Relaxed);
    crate::mcp_wire::respond(body, headers, move |msg| async move {
        crate::mcp_host::handle_message(meta, name, rev, msg, move |tool, args| async move {
            let result = state
                .executor
                .execute_tool_with_services(source, &tool, &args, &state.limits, None, None)
                .await;
            let stack = result
                .stack
                .as_deref()
                .map(|stack| remap_stack(stack, sourcemap, sourcemap_base));
            let usage = crate::tiers::Usage {
                exec_ms: result.exec_wall.as_millis() as u64,
                outbound_reqs: result.outbound_used,
                script_bytes: source.len(),
            };
            let plan = state.plan.read().unwrap().clone();
            let advisory = crate::tiers::warning(&usage, plan.as_deref());
            report(
                &Method::POST,
                Some(&format!("tools/{tool}")),
                &result,
                stack.as_deref(),
                advisory.as_deref(),
            );
            crate::mcp_host::invocation_tool_result(result)
        })
        .await
    })
    .await
}

/// Rewrites `at handler (handler:2919:31)` into `at handler (index.js:12:31)`.
/// Frames the map can't place are left untouched rather than dropped.
fn remap_stack(stack: &str, map: Option<&sourcemap::SourceMap>, base: &Path) -> String {
    let Some(map) = map else {
        return stack.to_string();
    };
    stack
        .lines()
        .map(|line| match locate(line) {
            Some((start, end, generated_line, generated_col)) => {
                // QuickJS counts from 1; source maps count from 0.
                match map.lookup_token(generated_line.saturating_sub(1), generated_col) {
                    Some(token) => {
                        let file = readable_source(token.get_source().unwrap_or("source"), base);
                        format!(
                            "{}{file}:{}:{}{}",
                            &line[..start],
                            token.get_src_line() + 1,
                            token.get_src_col(),
                            &line[end..]
                        )
                    }
                    None => line.to_string(),
                }
            }
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A bundle in the temp directory maps back through a pile of `../`; show the
/// path relative to where the developer is standing instead.
fn readable_source(source: &str, base: &Path) -> String {
    let resolved = base.join(source);
    let absolute = resolved.canonicalize().unwrap_or(resolved);
    std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            absolute
                .strip_prefix(&cwd)
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| {
            absolute
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| source.to_string())
        })
}

/// Finds the `file:line:col` span in a stack frame.
fn locate(line: &str) -> Option<(usize, usize, u32, u32)> {
    let (start, end) = match (line.find('('), line.rfind(')')) {
        (Some(open), Some(close)) if close > open => (open + 1, close),
        _ => {
            let at = line.find("at ")? + 3;
            (at, line.len())
        }
    };
    let span = line.get(start..end)?;
    let (rest, col) = span.rsplit_once(':')?;
    let (_file, row) = rest.rsplit_once(':')?;
    Some((start, end, row.parse().ok()?, col.parse().ok()?))
}

fn problem(status: StatusCode, code: &str, message: String) -> Response {
    println!("\x1b[38;5;203m✗ {} {message}\x1b[0m", status.as_u16());
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// One block per request: outcome, timings, console output, and the JS stack
/// when something threw.
fn human_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

fn report(
    method: &Method,
    rest: Option<&str>,
    result: &rusted_engine::InvocationResult,
    stack: Option<&str>,
    advisory: Option<&str>,
) {
    let path = rest.map(|r| format!("/{r}")).unwrap_or_default();
    let (mark, label) = match &result.outcome {
        Outcome::Success(_) => ("\x1b[38;5;150m✓", "200"),
        Outcome::Terminated(_) => ("\x1b[38;5;214m▲", "429"),
        Outcome::Error(_) => ("\x1b[1;38;5;203m✗", "500"),
    };
    println!(
        "{mark} {label}\x1b[0m {} {}  \x1b[38;5;138mwall {:.2}ms · exec {:.2}ms\x1b[0m",
        method.as_str(),
        path,
        result.wall.as_secs_f64() * 1000.0,
        result.exec_wall.as_secs_f64() * 1000.0,
    );
    for log in &result.logs {
        println!("  \x1b[38;5;180m{}\x1b[0m {}", log.level, log.message);
    }
    match &result.outcome {
        Outcome::Terminated(reason) => println!("  \x1b[38;5;214m{reason}\x1b[0m"),
        Outcome::Error(message) => {
            println!("  \x1b[1;38;5;203m{message}\x1b[0m");
            if let Some(stack) = stack {
                for line in stack.lines().filter(|l| !l.trim().is_empty()) {
                    println!("  \x1b[38;5;138m{line}\x1b[0m");
                }
            }
        }
        Outcome::Success(_) => {}
    }
    // Local runs at the ceiling deliberately; this is the only hint that a
    // subscription might not take what just happened.
    if let Some(advisory) = advisory {
        println!("  \x1b[38;5;214m⚠ {advisory}\x1b[0m");
    }
    println!();
}
