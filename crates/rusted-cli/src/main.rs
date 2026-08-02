//! The `rusted` binary: `serve` embeds the server; every other subcommand is a
//! thin client of its admin API.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

mod credentials;
use reqwest::blocking::Client;
use reqwest::Method;
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "rusted", version, about = "Tiny JavaScript microfunctions")]
struct Cli {
    /// Admin API of a running `rusted serve`
    #[arg(
        long,
        global = true,
        default_value = "http://127.0.0.1:7412",
        env = "RUSTED_ADMIN"
    )]
    admin: String,

    /// Print raw JSON responses (stable output for scripts and agents)
    #[arg(long, global = true)]
    json: bool,

    /// API key identifying you; create one at /console/keys
    #[arg(long, global = true, env = "RUSTED_API_KEY")]
    api_key: Option<String>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the server/orchestrator
    Serve {
        /// Data API port serving functions
        #[arg(long, default_value_t = 7411)]
        port: u16,
        /// Admin API port for this CLI
        #[arg(long, default_value_t = 7412)]
        admin_port: u16,
        /// Print per-invocation details (outcome, errors, console output, timings)
        #[arg(long)]
        debug: bool,
        /// Postgres connection string (run `make db` for the local default)
        #[arg(long, env = "DATABASE_URL", default_value = rusted_server::DEFAULT_DATABASE_URL)]
        database_url: String,
        /// Require Authorization: Bearer <api key> on function endpoints
        #[arg(long)]
        require_auth: bool,
    },
    /// Deploy a persistent function
    Push {
        file: PathBuf,
        /// Function name; optional when the file declares `export const config = { name }`
        #[arg(long)]
        name: Option<String>,
        /// Trigger type (only "http" for now)
        #[arg(long = "type", default_value = "http")]
        trigger_type: String,
        /// Allowed HTTP methods, comma-separated (e.g. GET,POST); default POST
        #[arg(long = "method", value_delimiter = ',')]
        methods: Vec<String>,
        /// Route pattern nested under /f/<name>, e.g. /users/{id}
        #[arg(long)]
        path: Option<String>,
    },
    /// Run a function locally with hot reload — no server, database, or key
    Run {
        /// Your handler; files with imports are bundled automatically
        file: PathBuf,
        /// Port to listen on (0 picks a free one)
        #[arg(long, default_value_t = 7400)]
        port: u16,
        /// Replace the built-in bundling with your own command
        #[arg(long)]
        build: Option<String>,
        /// Paths whose changes trigger a rebuild (default: the file's directory)
        #[arg(long)]
        watch: Vec<PathBuf>,
        /// Execution budget in ms (defaults to the most any plan allows)
        #[arg(long)]
        exec_ms: Option<u64>,
        /// Outbound fetch() calls allowed per invocation
        #[arg(long)]
        outbound: Option<u32>,
    },
    /// Sign in — approve once in a browser, no key to copy
    Login,
    /// Forget the stored credential for this server
    Logout,
    /// Bundle a function into a single deployable file
    Build {
        /// Your handler; imports are bundled in
        file: PathBuf,
        /// Where to write it (default: dist/<filename>)
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Also write a .map alongside it
        #[arg(long)]
        sourcemap: bool,
    },
    /// Deploy a temporary function to the server; it expires automatically
    Preview {
        file: PathBuf,
        /// Lifetime in seconds (default 120)
        #[arg(long)]
        ttl: Option<u64>,
    },
    /// Invoke a deployed function without HTTP
    Invoke {
        name: String,
        /// Request body
        #[arg(long, default_value = "")]
        body: String,
    },
    /// Validate a script without deploying it
    Verify { file: PathBuf },
    /// List functions and live temporary runs
    List,
    /// Print (or save) the deployed source of a function
    Pull {
        name: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Delete a function
    Delete { name: String },
    /// Show recent invocations of a function with their console output
    Logs { name: String },
}

impl Cli {
    /// A flag beats the environment, which beats what `rusted login` stored.
    fn key(&self) -> Option<String> {
        self.api_key
            .clone()
            .or_else(|| credentials::get(&self.admin))
    }
}

/// The device flow: the browser work happens on the human's side, and the CLI
/// only ever sees the key it was granted.
fn login(cli: &Cli) -> Result<(), String> {
    let admin = cli.admin.trim_end_matches('/').to_string();
    let label = format!(
        "cli on {}",
        hostname().unwrap_or_else(|| "this machine".to_string())
    );
    let client = Client::new();
    let start: Value = client
        .post(format!("{admin}/api/device/code"))
        .json(&json!({ "label": label }))
        .send()
        .map_err(|e| format!("cannot reach rusted at {admin}: {e}"))?
        .json()
        .map_err(|e| format!("unexpected response: {e}"))?;

    let (Some(device_code), Some(user_code), Some(uri)) = (
        start["device_code"].as_str(),
        start["user_code"].as_str(),
        start["verification_uri"].as_str(),
    ) else {
        return Err(format!(
            "this server does not support `rusted login`: {start}"
        ));
    };
    let interval = start["interval"].as_u64().unwrap_or(2);

    println!("open {uri} and enter\n");
    println!("    {user_code}\n");
    println!("waiting…");

    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(start["expires_in"].as_u64().unwrap_or(600));
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_secs(interval));
        let response = client
            .post(format!("{admin}/api/device/token"))
            .json(&json!({ "device_code": device_code }))
            .send()
            .map_err(|e| format!("cannot reach rusted at {admin}: {e}"))?;
        let body: Value = response.json().unwrap_or_else(|_| json!({}));
        if let Some(key) = body["api_key"].as_str() {
            let path = credentials::save(&admin, key)?;
            println!("\nsigned in — key stored in {}", path.display());
            return Ok(());
        }
        match body["error"]["code"].as_str() {
            Some("authorization_pending") => continue,
            Some("access_denied") => return Err("request declined".to_string()),
            Some("expired_token") => {
                return Err("that code expired — run `rusted login` again".to_string())
            }
            _ => return Err(format!("sign-in failed: {body}")),
        }
    }
    Err("timed out waiting for approval".to_string())
}

fn hostname() -> Option<String> {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

fn main() {
    // .env in the working directory feeds DATABASE_URL / GITHUB_* into the
    // environment before clap resolves `env = …` arguments.
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    if let Err(message) = dispatch(cli) {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn dispatch(cli: Cli) -> Result<(), String> {
    match cli.command {
        Cmd::Serve {
            port,
            admin_port,
            debug,
            ref database_url,
            require_auth,
        } => serve(port, admin_port, debug, database_url.clone(), require_auth),
        Cmd::Push {
            ref file,
            ref name,
            ref trigger_type,
            ref methods,
            ref path,
        } => {
            let source = deployable_source(file)?;
            let mut payload = json!({ "source": source, "type": trigger_type });
            if let Some(name) = name {
                payload["name"] = json!(name);
            }
            if !methods.is_empty() {
                payload["methods"] = json!(methods);
            }
            if let Some(path) = path {
                payload["path"] = json!(path);
            }
            let v = api(&cli, Method::POST, "/api/functions", Some(payload))?;
            emit(&cli, &v, |v| {
                let l = &v["limits"];
                let methods = v["methods"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                format!(
                    "pushed {} revision {} ({} bytes)\nbudget: {}ms wall · {}MB memory · {}KB request/response · concurrency {}\n{} {}",
                    v["name"].as_str().unwrap_or(""),
                    v["revision"],
                    v["size_bytes"],
                    l["wall_ms"],
                    l["memory_bytes"].as_u64().unwrap_or(0) / (1024 * 1024),
                    l["response_body_bytes"].as_u64().unwrap_or(0) / 1024,
                    l["concurrency"],
                    methods,
                    v["url"].as_str().unwrap_or("")
                )
            })
        }
        Cmd::Run {
            ref file,
            port,
            ref build,
            ref watch,
            exec_ms,
            outbound,
        } => run_local(
            file.clone(),
            port,
            build.clone(),
            watch.clone(),
            exec_ms,
            outbound,
            cli.admin.clone(),
            cli.api_key.clone(),
        ),
        Cmd::Login => login(&cli),
        Cmd::Logout => {
            let admin = cli.admin.trim_end_matches('/');
            if credentials::forget(admin)? {
                println!("signed out of {admin}");
            } else {
                println!("no stored credential for {admin}");
            }
            Ok(())
        }
        Cmd::Build {
            ref file,
            ref out,
            sourcemap,
        } => build_bundle(file.clone(), out.clone(), sourcemap),
        Cmd::Preview { ref file, ttl } => {
            let source = deployable_source(file)?;
            let mut body = json!({ "source": source });
            if let Some(ttl) = ttl {
                body["ttl_seconds"] = json!(ttl);
            }
            let v = api(&cli, Method::POST, "/api/runs", Some(body))?;
            emit(&cli, &v, |v| {
                format!(
                    "temporary endpoint (expires at epoch {}):\n{}",
                    v["expires_at"],
                    v["url"].as_str().unwrap_or("")
                )
            })
        }
        Cmd::Invoke { ref name, ref body } => {
            let v = api(
                &cli,
                Method::POST,
                "/api/invoke",
                Some(json!({ "name": name, "body": body })),
            )?;
            if cli.json {
                println!("{v}");
                return Ok(());
            }
            for log in v["logs"].as_array().into_iter().flatten() {
                eprintln!(
                    "[{}] {}",
                    log["level"].as_str().unwrap_or("log"),
                    log["message"].as_str().unwrap_or("")
                );
            }
            match v["outcome"].as_str() {
                Some("success") => {
                    println!("{}", v["response"].as_str().unwrap_or(""));
                    Ok(())
                }
                Some("terminated") => Err(format!("terminated: {}", v["reason"])),
                _ => Err(format!("function error: {}", v["message"])),
            }
        }
        Cmd::Verify { ref file } => {
            // Check what would deploy, not the source before bundling.
            let source = deployable_source(file)?;
            let v = api(
                &cli,
                Method::POST,
                "/api/verify",
                Some(json!({ "source": source })),
            )?;
            emit(&cli, &v, |_| "ok".to_string())
        }
        Cmd::List => {
            let v = api(&cli, Method::GET, "/api/functions", None)?;
            emit(&cli, &v, |v| {
                let mut lines = Vec::new();
                for f in v["functions"].as_array().into_iter().flatten() {
                    lines.push(format!(
                        "{}  rev {}  {}",
                        f["name"].as_str().unwrap_or(""),
                        f["revision"],
                        f["url"].as_str().unwrap_or("")
                    ));
                }
                for r in v["runs"].as_array().into_iter().flatten() {
                    lines.push(format!(
                        "run {}  expires {}  {}",
                        r["id"].as_str().unwrap_or(""),
                        r["expires_at"],
                        r["url"].as_str().unwrap_or("")
                    ));
                }
                if lines.is_empty() {
                    "no functions deployed".to_string()
                } else {
                    lines.join("\n")
                }
            })
        }
        Cmd::Pull {
            ref name,
            ref output,
        } => {
            let v = api(
                &cli,
                Method::GET,
                &format!("/api/functions/{name}?source=true"),
                None,
            )?;
            let source = v["source"]
                .as_str()
                .ok_or_else(|| "server response had no source".to_string())?;
            match output {
                Some(path) => {
                    std::fs::write(path, source).map_err(|e| format!("write {path:?}: {e}"))?;
                    println!("wrote {}", path.display());
                }
                None => println!("{source}"),
            }
            Ok(())
        }
        Cmd::Delete { ref name } => {
            let v = api(
                &cli,
                Method::DELETE,
                &format!("/api/functions/{name}"),
                None,
            )?;
            emit(&cli, &v, |_| format!("deleted {name}"))
        }
        Cmd::Logs { ref name } => {
            let v = api(&cli, Method::GET, &format!("/api/functions/{name}"), None)?;
            if cli.json {
                println!("{}", v["recent"]);
                return Ok(());
            }
            let recent = v["recent"].as_array().cloned().unwrap_or_default();
            if recent.is_empty() {
                println!("no invocations recorded for {name}");
                return Ok(());
            }
            // Newest-first from the server; print oldest-first so it reads like a log.
            for inv in recent.iter().rev() {
                println!(
                    "at {}  {}  wall {:.2}ms  cpu {:.2}ms",
                    inv["at"],
                    inv["outcome"].as_str().unwrap_or("?"),
                    inv["wall_ms"].as_f64().unwrap_or(0.0),
                    inv["cpu_ms"].as_f64().unwrap_or(0.0),
                );
                if let Some(detail) = inv["detail"].as_str() {
                    println!("  !! {detail}");
                }
                for log in inv["logs"].as_array().into_iter().flatten() {
                    println!(
                        "  [{}] {}",
                        log["level"].as_str().unwrap_or("log"),
                        log["message"].as_str().unwrap_or("")
                    );
                }
            }
            Ok(())
        }
    }
}

/// What actually gets deployed: bundled when the file has imports, so `push`
/// takes the same source `run` does rather than demanding a built artifact.
fn deployable_source(path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Err(format!("cannot read {}: no such file", path.display()));
    }
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(rusted_server::bundler::source_for(path))
}

/// Calls the admin API; non-2xx responses become Err with the server's JSON
/// error body so exit codes and stderr stay script-friendly.
fn api(cli: &Cli, method: Method, path: &str, body: Option<Value>) -> Result<Value, String> {
    let url = format!("{}{path}", cli.admin.trim_end_matches('/'));
    let mut request = Client::new().request(method, &url);
    if let Some(key) = cli.key() {
        request = request.bearer_auth(key);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().map_err(|e| {
        format!(
            "cannot reach rusted server at {}: {e}\nis `rusted serve` running?",
            cli.admin
        )
    })?;
    let status = response.status();
    let text = response.text().map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    if status.is_success() {
        return Ok(value);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED && cli.key().is_none() {
        return Err("not signed in — run `rusted login`".to_string());
    }
    Err(value.to_string())
}

fn emit(cli: &Cli, v: &Value, human: impl Fn(&Value) -> String) -> Result<(), String> {
    if cli.json {
        println!("{v}");
    } else {
        println!("{}", human(v));
    }
    Ok(())
}

/// Bundles a handler into one file, then checks the result is deployable.
fn build_bundle(file: PathBuf, out: Option<PathBuf>, sourcemap: bool) -> Result<(), String> {
    if !file.exists() {
        return Err(format!("{} does not exist", file.display()));
    }
    let out = out.unwrap_or_else(|| {
        PathBuf::from("dist").join(file.file_name().unwrap_or_else(|| "bundle.js".as_ref()))
    });
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let bundled = rt.block_on(rusted_server::bundler::bundle(&file, sourcemap))?;

    // A bundle that won't load is not a build worth writing.
    let config =
        rusted_engine::Executor::inspect(&rusted_engine::QuickJsExecutor::new(), &bundled.code)
            .map_err(|e| format!("the bundle does not load: {e}"))?;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {parent:?}: {e}"))?;
    }
    std::fs::write(&out, &bundled.code).map_err(|e| format!("cannot write {out:?}: {e}"))?;
    if let Some(map) = bundled.sourcemap.filter(|_| sourcemap) {
        let map_path = out.with_extension("js.map");
        std::fs::write(&map_path, map).map_err(|e| format!("cannot write {map_path:?}: {e}"))?;
    }

    let size = bundled.code.len();
    let pretty = if size >= 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{size} bytes")
    };
    println!("built {} ({pretty})", out.display());
    println!(
        "  {} /f/{}{}",
        config
            .methods
            .map(|m| m.join(","))
            .unwrap_or_else(|| "POST".to_string()),
        config.name.unwrap_or_else(|| out
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()),
        config.path.unwrap_or_default()
    );
    Ok(())
}

/// Local development: one function, hot reload, no server or database.
#[allow(clippy::too_many_arguments)]
fn run_local(
    file: PathBuf,
    port: u16,
    build: Option<String>,
    watch: Vec<PathBuf>,
    exec_ms: Option<u64>,
    outbound: Option<u32>,
    admin: String,
    api_key: Option<String>,
) -> Result<(), String> {
    // With --build the entry may be the build's output, which a clean
    // checkout hasn't produced yet.
    if build.is_none() && !file.exists() {
        return Err(format!("{} does not exist", file.display()));
    }
    // Develop against the ceiling: nothing deployable exceeds the top plan, so
    // this never blocks work, and each run reports what it would cost.
    let ceiling = rusted_server::tiers::most_permissive();
    let limits = rusted_engine::Limits {
        wall_ms: exec_ms.unwrap_or(ceiling.exec_ms),
        outbound: rusted_engine::OutboundPolicy {
            max_requests: outbound.unwrap_or(ceiling.outbound_reqs),
            ..Default::default()
        },
        ..Default::default()
    };
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(rusted_server::local::serve(
        rusted_server::local::LocalConfig {
            // Only used to look up the plan, in the background, after startup.
            admin: api_key.map(|key| (admin, key)),
            entry: file,
            watch,
            build,
            port,
            limits,
        },
    ))
}

fn serve(
    port: u16,
    admin_port: u16,
    debug: bool,
    database_url: String,
    require_auth: bool,
) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let handle = rusted_server::start(rusted_server::ServerConfig {
            data_port: port,
            admin_port,
            queue_wait_ms: rusted_server::DEFAULT_QUEUE_WAIT_MS,
            debug,
            database_url,
            require_auth,
        })
        .await
        .map_err(|e| format!("failed to start server: {e}"))?;
        println!("rusted: functions on http://{}", handle.data_addr);
        println!("rusted: admin API on http://{}", handle.admin_addr);
        std::future::pending::<()>().await;
        unreachable!()
    })
}
