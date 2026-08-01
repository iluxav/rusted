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

/// A file with `import` statements can't run as-is — the runtime resolves no
/// modules — so it gets bundled first.
fn needs_bundling(source: &str) -> bool {
    source.lines().any(|line| {
        let line = line.trim_start();
        (line.starts_with("import ") || line.starts_with("import{") || line.starts_with("import("))
            && !line.starts_with("import.meta")
    }) || source.contains("} from \"")
        || source.contains("} from '")
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
    if !needs_bundling(&source) {
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

struct Loaded {
    source: String,
    name: String,
    trigger: HttpTrigger,
    /// Maps bundled positions back to your files, so a stack frame names
    /// `index.js:12` rather than `handler:2919`.
    sourcemap: Option<Arc<sourcemap::SourceMap>>,
    /// Where the map's relative sources resolve from (the bundle's directory).
    sourcemap_base: PathBuf,
}

struct LocalState {
    loaded: RwLock<Loaded>,
    /// Shared so the bytecode cache stays warm across requests.
    executor: Arc<QuickJsExecutor>,
    limits: Limits,
}

/// Reads the entry file and its `export const config`, falling back to the
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
    let config = executor.inspect(&source)?;
    let stem = entry
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "function".to_string());
    let name = config.name.unwrap_or(stem);
    let sourcemap = sourcemap_json
        .and_then(|json| sourcemap::SourceMap::from_reader(json.as_bytes()).ok())
        .map(Arc::new);
    Ok(Loaded {
        source,
        name,
        sourcemap,
        // The map's sources are relative to the entry's own directory.
        sourcemap_base: match entry.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        },
        trigger: HttpTrigger {
            methods: config
                .methods
                .map(|m| m.iter().map(|s| s.to_uppercase()).collect())
                .unwrap_or_else(|| vec!["POST".to_string()]),
            path: config.path,
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
    let route = format!(
        "/f/{}{}",
        loaded.name,
        loaded.trigger.path.as_deref().unwrap_or("")
    );
    let methods = loaded.trigger.methods.join(",");
    let state = Arc::new(LocalState {
        loaded: RwLock::new(loaded),
        executor: executor.clone(),
        limits: config.limits.clone(),
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
    println!("  \x1b[38;5;209m{methods}\x1b[0m http://{addr}{route}");
    println!(
        "  limits: {}ms wall · {} outbound · watching {}",
        config.limits.wall_ms,
        config.limits.outbound.max_requests,
        pipeline
            .watch
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  ctrl-c to stop\n");

    tokio::spawn(watch_loop(state, config.entry.clone(), pipeline));

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("server stopped: {e}"))
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
                println!(
                    "\x1b[38;5;150m✓ reloaded {} ({bytes} bytes)\x1b[0m\n",
                    next.name
                );
                *state.loaded.write().unwrap() = next;
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
    let (source, expected_name, trigger, sourcemap, sourcemap_base) = {
        let loaded = state.loaded.read().unwrap();
        (
            loaded.source.clone(),
            loaded.name.clone(),
            loaded.trigger.clone(),
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
    let request = HttpRequest {
        method: method.as_str().to_string(),
        headers: headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect(),
        query: query.into_iter().collect(),
        params,
        body: String::from_utf8_lossy(&body).into_owned(),
    };

    let executor = state.executor.clone();
    let limits = state.limits.clone();
    let result = tokio::task::spawn_blocking(move || executor.execute(&source, &request, &limits))
        .await
        .expect("executor thread never panics");

    let stack = result
        .stack
        .as_deref()
        .map(|stack| remap_stack(stack, sourcemap.as_deref(), &sourcemap_base));
    report(&method, rest.as_deref(), &result, stack.as_deref());

    // Unlike the deployed data plane, local dev returns the real error: the
    // only caller is the developer who wrote it.
    match result.outcome {
        Outcome::Success(body) => {
            let content_type = result.content_type.unwrap_or_else(|| {
                if serde_json::from_str::<serde_json::Value>(&body).is_ok() {
                    "application/json"
                } else {
                    "text/plain; charset=utf-8"
                }
                .to_string()
            });
            (StatusCode::OK, [(CONTENT_TYPE, content_type)], body).into_response()
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
fn report(
    method: &Method,
    rest: Option<&str>,
    result: &rusted_engine::InvocationResult,
    stack: Option<&str>,
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
    println!();
}
