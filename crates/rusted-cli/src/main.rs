//! The `rusted` binary: `serve` embeds the server; every other subcommand is a
//! thin client of its admin API.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

mod credentials;
use reqwest::blocking::Client;
use reqwest::Method;
use serde_json::{json, Value};

/// Where the CLI looks when nobody said otherwise. Installing the CLI is
/// overwhelmingly a step toward using the hosted service; running your own
/// server is the deliberate case, and `RUSTED_ADMIN` is how you say so.
const HOSTED_ADMIN: &str = "https://rusted.sh";

/// The admin port `rusted serve` binds, quoted in errors so self-hosters who
/// hit the hosted default by accident are told the exact way back.
const LOCAL_ADMIN: &str = "http://127.0.0.1:7412";

/// A connection failure is nearly always "pointed at the wrong server," so say
/// what the two servers usually are rather than only reporting the socket error.
fn unreachable(admin: &str, e: impl std::fmt::Display) -> String {
    let other = if admin.starts_with(HOSTED_ADMIN) {
        format!("running your own server? {LOCAL_ADMIN}")
    } else {
        format!("meant the hosted service? {HOSTED_ADMIN}")
    };
    format!(
        "cannot reach rusted at {admin}: {e}\n\n  {other}\n  \
         point somewhere else with --admin <url>, or set RUSTED_ADMIN to make it stick"
    )
}

#[derive(Parser)]
#[command(name = "rusted", version, about = "Tiny JavaScript microfunctions")]
struct Cli {
    /// Admin API to talk to — the hosted service unless you say otherwise
    #[arg(
        long,
        global = true,
        default_value = HOSTED_ADMIN,
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
        /// Interface to listen on; behind a reverse proxy leave this alone
        #[arg(long, env = "RUSTED_HOST", default_value = "127.0.0.1")]
        host: String,
        /// The origin callers reach this server on, e.g. https://rusted.sh
        #[arg(long, env = "PUBLIC_URL")]
        public_url: Option<String>,
    },
    /// Deploy a persistent function
    Push {
        file: PathBuf,
        /// Function name; optional when the file declares one
        /// (`export const http = { name }` or `export const mcp = { name }`)
        #[arg(long)]
        name: Option<String>,
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
    /// Invoke a deployed function without HTTP.
    ///
    /// Exit codes, for scripts and agent harnesses: 0 the function succeeded,
    /// 1 it threw, 2 usage or connection problems, 3 a resource limit
    /// terminated it, 4 this function cannot be invoked this way (mcp kind,
    /// non-POST methods, or a route path — call its URL instead).
    Invoke {
        name: String,
        /// JSON input, checked before sending; '-' reads it from stdin
        #[arg(long, conflicts_with = "body")]
        input: Option<String>,
        /// Raw request body, sent exactly as given
        #[arg(long, default_value = "")]
        body: String,
        /// Environment to run in — scopes secrets and state; default prod
        #[arg(long)]
        env: Option<String>,
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
    Logs {
        name: String,
        /// Only failures: errors, terminations, refusals, and 4xx/5xx answers
        #[arg(long)]
        errors: bool,
    },
    /// Throwaway URLs that anyone can POST to, for receiving callbacks
    #[command(subcommand)]
    Inbox(InboxCmd),
    /// Durable per-function state (`context.state`)
    #[command(subcommand)]
    State(StateCmd),
    /// Write TypeScript declarations for `request`, `context`, and the globals
    Types {
        /// Where to write them (default: rusted.d.ts)
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Scaffold a new function: handler, types, and tsconfig, ready to run
    New {
        /// Directory to create; also the function's default name
        name: String,
        /// Plain JavaScript instead of TypeScript
        #[arg(long)]
        js: bool,
        /// An MCP server (`export const mcp` with tools) instead of an HTTP handler
        #[arg(long)]
        mcp: bool,
    },
}

#[derive(Subcommand)]
enum StateCmd {
    /// Permanently delete EVERY state key a function holds.
    ///
    /// State survives redeploys and even deleting the function — this is the
    /// one explicit way it goes away.
    Purge { name: String },
}

#[derive(Subcommand)]
enum InboxCmd {
    /// Create one and print the URL to hand out
    New {
        name: String,
        /// How long it lives, from creation — never extended by activity.
        /// Plain seconds, or a suffix: 90s, 2m, 1h.
        #[arg(long, default_value = "5m")]
        ttl: String,
        /// "append" keeps every message; "upsert" keeps only the latest
        #[arg(long, default_value = "append")]
        store: String,
        /// Delete on the first read that finds something, like a queue
        #[arg(long)]
        drain: bool,
    },
    /// Read what has arrived
    Get { name: String },
    /// Show live inboxes
    List,
    /// Remove one before it expires
    Rm { name: String },
}

impl Cli {
    /// A flag beats the environment, which beats what `rusted login` stored.
    fn key(&self) -> Option<String> {
        self.api_key
            .clone()
            .or_else(|| credentials::get(&self.admin))
    }
}

/// Accepts `300`, `90s`, `2m` or `1h`.
///
/// A TTL is the one number here a human types by hand, and thinking in seconds
/// for anything over a minute is a small, avoidable tax.
fn parse_duration(raw: &str) -> Result<i64, String> {
    let raw = raw.trim();
    let (digits, multiplier) = match raw.chars().last() {
        Some('s') => (&raw[..raw.len() - 1], 1),
        Some('m') => (&raw[..raw.len() - 1], 60),
        Some('h') => (&raw[..raw.len() - 1], 3600),
        _ => (raw, 1),
    };
    digits
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .map(|n| n * multiplier)
        .ok_or_else(|| format!("cannot read '{raw}' as a duration — try 90s, 2m or 1h"))
}

/// Declarations for `request`, `context`, and the globals. Shipped inside the
/// binary so they always describe this runtime rather than whatever a registry
/// last published.
const DECLARATIONS: &str = include_str!("../../rusted-engine/rusted.d.ts");

/// Everything needed to run one function, and nothing else. No install step:
/// `rusted run` bundles in-process, so this is ready the moment it is written.
fn scaffold(name: &str, js: bool, mcp: bool) -> Result<(), String> {
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return Err(format!(
            "'{name}' is not a usable directory name — letters, digits and dashes work best"
        ));
    }
    let root = PathBuf::from(name);
    if root.exists() {
        return Err(format!(
            "{name} already exists — pick another name, or delete it first"
        ));
    }

    let ext = if js { "js" } else { "ts" };
    let entry = format!("index.{ext}");
    let handler = if mcp {
        // Tool handlers, not a request handler — the file *is* an MCP server.
        let (reference, annotation) = if js {
            ("", "")
        } else {
            (
                "/// <reference path=\"./rusted.d.ts\" />\n\n",
                ": Rusted.Mcp",
            )
        };
        format!(
            "{reference}export const mcp{annotation} = {{\n\
             \x20 name: \"{name}\",\n\
             \x20 tools: {{\n\
             \x20   hello: {{\n\
             \x20     description: \"Say hello\",\n\
             \x20     inputSchema: {{\n\
             \x20       type: \"object\",\n\
             \x20       properties: {{ name: {{ type: \"string\" }} }},\n\
             \x20       required: [\"name\"],\n\
             \x20     }},\n\
             \x20     async handler({{ name }}) {{\n\
             \x20       return `Hello, ${{name}}!`;\n\
             \x20     }},\n\
             \x20   }},\n\
             \x20 }},\n\
             }};\n"
        )
    } else if js {
        format!(
            "export const http = {{ name: \"{name}\", methods: [\"POST\"] }};\n\
             \n\
             export default async function handler(request, context) {{\n\
             \x20 const {{ name }} = await request.json();\n\
             \x20 return context.json({{ message: `Hello, ${{name ?? \"world\"}}` }});\n\
             }}\n"
        )
    } else {
        format!(
            "/// <reference path=\"./rusted.d.ts\" />\n\
             \n\
             interface Input {{\n\
             \x20 name?: string;\n\
             }}\n\
             \n\
             export const http: Rusted.Http = {{ name: \"{name}\", methods: [\"POST\"] }};\n\
             \n\
             const handler: Rusted.Handler = async (request, context) => {{\n\
             \x20 const {{ name }} = await request.json<Input>();\n\
             \x20 return context.json({{ message: `Hello, ${{name ?? \"world\"}}` }});\n\
             }};\n\
             \n\
             export default handler;\n"
        )
    };

    // "lib": ["ES2020"] with no "DOM" on purpose: this runtime's fetch and
    // console are smaller than a browser's, and DOM would promise methods the
    // engine does not have — code would typecheck and then fail at runtime.
    let tsconfig = "{\n\
        \x20 \"compilerOptions\": {\n\
        \x20   \"strict\": true,\n\
        \x20   \"lib\": [\"ES2020\"],\n\
        \x20   \"types\": [],\n\
        \x20   \"target\": \"ES2020\",\n\
        \x20   \"module\": \"ESNext\",\n\
        \x20   \"moduleResolution\": \"bundler\",\n\
        \x20   \"noEmit\": true\n\
        \x20 },\n\
        \x20 \"include\": [\"*.ts\", \"rusted.d.ts\"]\n\
        }\n";

    // Only needed once you add dependencies — `rusted run` needs no install.
    // Imports from node_modules are bundled in, so `npm i zod` just works.
    let package = format!(
        "{{\n\
         \x20 \"name\": \"{name}\",\n\
         \x20 \"private\": true,\n\
         \x20 \"type\": \"module\",\n\
         \x20 \"scripts\": {{\n\
         \x20   \"dev\": \"rusted run {entry}\",\n\
         \x20   \"deploy\": \"rusted push {entry}\"\n\
         \x20 }}\n\
         }}\n"
    );

    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {name}: {e}"))?;
    let mut written = vec![entry.clone()];
    write_file(&root, &entry, &handler)?;
    write_file(&root, "package.json", &package)?;
    write_file(&root, ".gitignore", "node_modules/\ndist/\n")?;
    written.push("package.json".into());
    written.push(".gitignore".into());
    if !js {
        write_file(&root, "rusted.d.ts", DECLARATIONS)?;
        write_file(&root, "tsconfig.json", tsconfig)?;
        written.push("rusted.d.ts".into());
        written.push("tsconfig.json".into());
    }

    println!("created {name}/");
    for f in &written {
        println!("  {f}");
    }
    println!("\n  cd {name}\n  rusted run {entry}\n");
    println!("no install needed — imports are bundled in, so `npm i <pkg>` also just works");
    Ok(())
}

fn write_file(root: &Path, name: &str, body: &str) -> Result<(), String> {
    let path = root.join(name);
    std::fs::write(&path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))
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
        .map_err(|e| unreachable(&admin, e))?
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
            .map_err(|e| unreachable(&admin, e))?;
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
            ref host,
            ref public_url,
        } => serve(
            port,
            admin_port,
            debug,
            database_url.clone(),
            require_auth,
            host.clone(),
            public_url.clone(),
        ),
        Cmd::Push {
            ref file,
            ref name,
            ref methods,
            ref path,
        } => {
            let source = deployable_source(file)?;
            let mut payload = json!({ "source": source });
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
            if v["kind"] == json!("mcp") {
                return emit(&cli, &v, mcp_push_summary);
            }
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
        Cmd::Invoke {
            ref name,
            ref input,
            ref body,
            ref env,
        } => {
            // Usage mistakes exit 2 before anything is sent, so a harness can
            // tell "my input was wrong" from "the function failed".
            let payload_body = match input {
                Some(raw) => {
                    let text = if raw == "-" {
                        let mut text = String::new();
                        if let Err(e) =
                            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                        {
                            eprintln!("reading stdin: {e}");
                            std::process::exit(EXIT_USAGE);
                        }
                        text
                    } else {
                        raw.clone()
                    };
                    if let Err(e) = serde_json::from_str::<Value>(&text) {
                        eprintln!("--input is not valid JSON: {e}");
                        std::process::exit(EXIT_USAGE);
                    }
                    text
                }
                None => body.clone(),
            };
            let mut payload = json!({ "name": name, "body": payload_body });
            if let Some(env) = env {
                payload["env"] = json!(env);
            }
            let outcome = api(&cli, Method::POST, "/api/invoke", Some(payload));
            let report = invoke_report(name, &outcome, cli.json);
            for line in &report.stderr {
                eprintln!("{line}");
            }
            if let Some(out) = &report.stdout {
                println!("{out}");
            }
            // Exit here rather than through main: the code carries the
            // outcome, not just pass/fail.
            if report.code == 0 {
                Ok(())
            } else {
                std::process::exit(report.code)
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
            emit(&cli, &v, |v| match v["kind"].as_str() {
                Some("mcp") => {
                    let tools = v["config"]["tools"]
                        .as_object()
                        .map(|m| m.keys().cloned().collect::<Vec<_>>().join(", "))
                        .unwrap_or_default();
                    format!("ok — mcp function, tools: {tools}")
                }
                Some(kind) => format!("ok — {kind} function"),
                None => "ok".to_string(),
            })
        }
        Cmd::List => {
            let v = api(&cli, Method::GET, "/api/functions", None)?;
            emit(&cli, &v, |v| {
                let mut lines = Vec::new();
                for f in v["functions"].as_array().into_iter().flatten() {
                    // An mcp entry names its kind and tools; http stays terse.
                    let kind = match f["kind"].as_str() {
                        Some("mcp") => {
                            let tools = f["tools"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|t| t.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            format!("mcp[{tools}]  ")
                        }
                        _ => String::new(),
                    };
                    lines.push(format!(
                        "{}  rev {}  {}{}",
                        f["name"].as_str().unwrap_or(""),
                        f["revision"],
                        kind,
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
        Cmd::State(StateCmd::Purge { ref name }) => {
            let v = api(
                &cli,
                Method::DELETE,
                &format!("/api/functions/{name}/state"),
                None,
            )?;
            emit(&cli, &v, |v| {
                format!(
                    "purged {} state key(s) of {name}",
                    v["purged_keys"].as_u64().unwrap_or(0)
                )
            })
        }
        Cmd::New { ref name, js, mcp } => scaffold(name, js, mcp),
        Cmd::Types { ref out } => {
            let path = out.clone().unwrap_or_else(|| PathBuf::from("rusted.d.ts"));
            std::fs::write(&path, DECLARATIONS)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            println!("wrote {} ({} bytes)", path.display(), DECLARATIONS.len());
            println!(
                "\nreference it from your handler:\n  \
                 /// <reference path=\"./{}\" />\n\n\
                 or list it in tsconfig \"include\". Set \"lib\": [\"ES2020\"] and\n\
                 leave out \"DOM\" — this runtime's fetch and console are smaller\n\
                 than a browser's, and DOM would promise methods it lacks.",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            Ok(())
        }
        Cmd::Inbox(ref sub) => match sub {
            InboxCmd::New {
                name,
                ttl,
                store,
                drain,
            } => {
                let seconds = parse_duration(ttl)?;
                let v = api(
                    &cli,
                    Method::POST,
                    "/api/inboxes",
                    Some(json!({
                        "name": name,
                        "ttl_seconds": seconds,
                        "store": store,
                        "drain": drain,
                    })),
                )?;
                emit(&cli, &v, |v| {
                    format!(
                        "{}\n  anyone with this URL can POST to it; reading needs your key\n  expires in {}s",
                        v["url"].as_str().unwrap_or(""),
                        v["expires_in_seconds"]
                    )
                })
            }
            InboxCmd::Get { name } => {
                let v = api(&cli, Method::GET, &format!("/api/inboxes/{name}"), None)?;
                emit(&cli, &v, |v| {
                    let messages = v["messages"].as_array().cloned().unwrap_or_default();
                    if messages.is_empty() {
                        return "nothing yet — the inbox is alive, try again".to_string();
                    }
                    let body = messages
                        .iter()
                        .map(|m| serde_json::to_string_pretty(m).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if v["drained"] == json!(true) {
                        format!("{body}\n\n  drained — this inbox is now gone")
                    } else {
                        body
                    }
                })
            }
            InboxCmd::List => {
                let v = api(&cli, Method::GET, "/api/inboxes", None)?;
                emit(&cli, &v, |v| {
                    let inboxes = v["inboxes"].as_array().cloned().unwrap_or_default();
                    if inboxes.is_empty() {
                        return "no live inboxes".to_string();
                    }
                    inboxes
                        .iter()
                        .map(|i| {
                            format!(
                                "{:<20} {:<8} held {:<4} expires in {}s\n  {}",
                                i["name"].as_str().unwrap_or(""),
                                i["store"].as_str().unwrap_or(""),
                                i["held"],
                                i["expires_in_seconds"],
                                i["url"].as_str().unwrap_or(""),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            }
            InboxCmd::Rm { name } => {
                let v = api(&cli, Method::DELETE, &format!("/api/inboxes/{name}"), None)?;
                emit(&cli, &v, |_| format!("removed {name}"))
            }
        },
        Cmd::Logs { ref name, errors } => {
            let v = api(&cli, Method::GET, &format!("/api/functions/{name}"), None)?;
            if cli.json {
                println!("{}", v["recent"]);
                return Ok(());
            }
            let mut recent = v["recent"].as_array().cloned().unwrap_or_default();
            if errors {
                // A "success" that answered 4xx/5xx is a failure to the caller.
                recent.retain(|inv| {
                    inv["outcome"].as_str() != Some("success")
                        || inv["status"].as_u64().unwrap_or(200) >= 400
                });
            }
            if recent.is_empty() {
                println!(
                    "no {}invocations recorded for {name}",
                    if errors { "failing " } else { "" }
                );
                return Ok(());
            }
            // Newest-first from the server; print oldest-first so it reads like a log.
            for inv in recent.iter().rev() {
                let status = inv["status"]
                    .as_u64()
                    .map(|s| format!(" {s}"))
                    .unwrap_or_default();
                println!(
                    "at {}  {}{status}  wall {:.2}ms  cpu {:.2}ms",
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
    let response = request.send().map_err(|e| unreachable(&cli.admin, e))?;
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

/// `rusted invoke`'s exit contract, stable for scripts and agent harnesses.
const EXIT_FUNCTION_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_TERMINATED: i32 = 3;
const EXIT_REFUSED: i32 = 4;

/// Everything one invocation should print, and how the process should exit.
struct InvokeReport {
    stdout: Option<String>,
    stderr: Vec<String>,
    code: i32,
}

/// Maps the server's reply to output and an exit code. Pure, so the contract
/// is testable: 0 success, 1 the function threw, 2 usage/transport trouble,
/// 3 a limit terminated it, 4 refused as not invocable this way.
///
/// In `--json` mode stdout is one stable document. `body` is the exact
/// response text — deliberately not parsed into a duplicate `json` field, so
/// large replies aren't paid for twice; `outcome` survives alongside `ok`
/// because an error (fix the code) and a termination (raise the limit or
/// retry) warrant different reactions.
fn invoke_report(name: &str, outcome: &Result<Value, String>, json_mode: bool) -> InvokeReport {
    match outcome {
        Ok(v) => {
            let timing = json!({ "wall_ms": v["wall_ms"], "cpu_ms": v["cpu_ms"] });
            let human_logs = || {
                v["logs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|log| {
                        format!(
                            "[{}] {}",
                            log["level"].as_str().unwrap_or("log"),
                            log["message"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let (code, doc, human) = match v["outcome"].as_str() {
                Some("success") => (
                    0,
                    json!({
                        "ok": true,
                        "outcome": "success",
                        "status": v["status"],
                        "content_type": v["content_type"],
                        "headers": v["headers"],
                        "body": v["response"],
                        "logs": v["logs"],
                        "timing": timing,
                    }),
                    Ok(v["response"].as_str().unwrap_or("").to_string()),
                ),
                Some("terminated") => (
                    EXIT_TERMINATED,
                    json!({
                        "ok": false,
                        "outcome": "terminated",
                        "reason": v["reason"],
                        "logs": v["logs"],
                        "timing": timing,
                    }),
                    Err(format!("terminated: {}", v["reason"])),
                ),
                _ => (
                    EXIT_FUNCTION_ERROR,
                    json!({
                        "ok": false,
                        "outcome": "error",
                        "message": v["message"],
                        "logs": v["logs"],
                        "timing": timing,
                    }),
                    Err(format!("function error: {}", v["message"])),
                ),
            };
            if json_mode {
                InvokeReport {
                    stdout: Some(doc.to_string()),
                    stderr: vec![],
                    code,
                }
            } else {
                let mut stderr = human_logs();
                let stdout = match human {
                    Ok(body) => Some(body),
                    Err(message) => {
                        stderr.push(message);
                        None
                    }
                };
                InvokeReport {
                    stdout,
                    stderr,
                    code,
                }
            }
        }
        Err(raw) => {
            // `api` surfaces server error envelopes as their JSON text;
            // anything unparseable is transport-level trouble.
            let e: serde_json::Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            let error_code = e["error"]["code"].as_str().unwrap_or("").to_string();
            let refused = matches!(
                error_code.as_str(),
                "kind_mismatch" | "method_mismatch" | "path_mismatch"
            );
            let mut message = friendly_kind_mismatch(name, raw).unwrap_or_else(|| {
                e["error"]["message"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| raw.clone())
            });
            if refused && error_code != "kind_mismatch" {
                if let Some(url) = e["error"]["url"].as_str() {
                    message.push_str(&format!("\ncall it at {url}"));
                }
            }
            let code = if refused { EXIT_REFUSED } else { EXIT_USAGE };
            if json_mode {
                let mut doc = json!({
                    "ok": false,
                    "outcome": if refused { "refused" } else { "failed" },
                    "message": message,
                });
                if !error_code.is_empty() {
                    doc["code"] = json!(error_code);
                }
                if let Some(url) = e["error"]["url"].as_str() {
                    doc["url"] = json!(url);
                }
                InvokeReport {
                    stdout: Some(doc.to_string()),
                    stderr: vec![],
                    code,
                }
            } else {
                InvokeReport {
                    stdout: None,
                    stderr: vec![message],
                    code,
                }
            }
        }
    }
}

/// Invoking an mcp function as http is refused by the server with
/// `kind_mismatch`; turn that envelope into advice. `None` means the error was
/// something else and should pass through untouched.
fn friendly_kind_mismatch(name: &str, raw: &str) -> Option<String> {
    let e: Value = serde_json::from_str(raw).ok()?;
    if e["error"]["code"] != json!("kind_mismatch") {
        return None;
    }
    let mut message = format!("{name} is an mcp function — `rusted invoke` drives http functions.");
    if let Some(url) = e["error"]["url"].as_str() {
        message.push_str(&format!("\nconnect an MCP client to {url} instead"));
    }
    Some(message)
}

/// The deliverable of an mcp push: a config block ready to paste into an MCP
/// client. A private function needs the owner's key on every request, so the
/// block carries the header hint; a public one must not suggest a key it
/// would ignore.
fn mcp_push_summary(v: &Value) -> String {
    let name = v["name"].as_str().unwrap_or("");
    let url = v["url"].as_str().unwrap_or("");
    let headers = if v["public"] == json!(true) {
        ""
    } else {
        ",\n      \"headers\": { \"Authorization\": \"Bearer <your rusted api key>\" }"
    };
    format!(
        "deployed mcp function {name} (rev {rev})\n\
         \n\
         add to your MCP client config:\n\
         {{\n\
         \x20 \"mcpServers\": {{\n\
         \x20   \"{name}\": {{\n\
         \x20     \"url\": \"{url}\"{headers}\n\
         \x20   }}\n\
         \x20 }}\n\
         }}",
        rev = v["revision"]
    )
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
    let inspection =
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
    match inspection.surface {
        rusted_engine::Surface::Http(config) => println!(
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
        ),
        rusted_engine::Surface::Mcp(config) => println!(
            "  mcp tools: {}",
            config.tools.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    }
    if !inspection.config.secrets.is_empty() {
        println!("  secrets: {}", inspection.config.secrets.join(", "));
    }
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

#[allow(clippy::too_many_arguments)]
fn serve(
    port: u16,
    admin_port: u16,
    debug: bool,
    database_url: String,
    require_auth: bool,
    host: String,
    public_url: Option<String>,
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
            host,
            public_url,
        })
        .await
        .map_err(|e| format!("failed to start server: {e}"))?;
        println!("rusted: functions on http://{}", handle.data_addr);
        println!("rusted: admin API on http://{}", handle.admin_addr);
        std::future::pending::<()>().await;
        unreachable!()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success_envelope() -> Value {
        json!({
            "outcome": "success",
            "status": 201,
            "content_type": "application/json",
            "headers": { "x-made-by": "test" },
            "response": "{\"made\":true}",
            "logs": [{ "level": "log", "message": "hi" }],
            "wall_ms": 1.5,
            "cpu_ms": 0.4,
        })
    }

    #[test]
    fn success_json_is_one_stable_document_with_exit_zero() {
        let report = invoke_report("fn", &Ok(success_envelope()), true);
        assert_eq!(report.code, 0);
        assert!(report.stderr.is_empty());
        let doc: Value = serde_json::from_str(&report.stdout.unwrap()).unwrap();
        assert_eq!(doc["ok"], true);
        assert_eq!(doc["outcome"], "success");
        assert_eq!(doc["status"], 201);
        assert_eq!(doc["content_type"], "application/json");
        assert_eq!(doc["body"], "{\"made\":true}");
        assert_eq!(doc["headers"]["x-made-by"], "test");
        assert_eq!(doc["timing"]["wall_ms"], 1.5);
        // The body stays a string: no duplicated parsed copy.
        assert!(doc["body"].is_string());
        assert!(doc.get("json").is_none());
    }

    #[test]
    fn success_human_prints_body_and_logs_apart() {
        let report = invoke_report("fn", &Ok(success_envelope()), false);
        assert_eq!(report.code, 0);
        assert_eq!(report.stdout.as_deref(), Some("{\"made\":true}"));
        assert_eq!(report.stderr, vec!["[log] hi"]);
    }

    #[test]
    fn function_error_exits_one_even_in_json_mode() {
        let envelope =
            json!({ "outcome": "error", "message": "boom", "logs": [], "wall_ms": 1, "cpu_ms": 1 });
        let report = invoke_report("fn", &Ok(envelope), true);
        assert_eq!(report.code, EXIT_FUNCTION_ERROR);
        let doc: Value = serde_json::from_str(&report.stdout.unwrap()).unwrap();
        assert_eq!(doc["ok"], false);
        assert_eq!(doc["outcome"], "error");
        assert_eq!(doc["message"], "boom");
    }

    #[test]
    fn termination_exits_three_and_keeps_its_reason() {
        let envelope = json!({ "outcome": "terminated", "reason": "wall_ms", "logs": [], "wall_ms": 1, "cpu_ms": 1 });
        let json_report = invoke_report("fn", &Ok(envelope.clone()), true);
        assert_eq!(json_report.code, EXIT_TERMINATED);
        let doc: Value = serde_json::from_str(&json_report.stdout.unwrap()).unwrap();
        assert_eq!(doc["outcome"], "terminated");
        assert_eq!(doc["reason"], "wall_ms");
        let human = invoke_report("fn", &Ok(envelope), false);
        assert_eq!(human.code, EXIT_TERMINATED);
        assert!(human.stderr.last().unwrap().contains("terminated"));
    }

    #[test]
    fn mismatch_refusals_exit_four_with_the_working_url() {
        for code in ["kind_mismatch", "method_mismatch", "path_mismatch"] {
            let raw = json!({ "error": {
                "code": code,
                "message": "not this way",
                "url": "https://rusted.sh/f/thing",
            }})
            .to_string();
            let report = invoke_report("thing", &Err(raw), true);
            assert_eq!(report.code, EXIT_REFUSED, "{code} should refuse");
            let doc: Value = serde_json::from_str(&report.stdout.unwrap()).unwrap();
            assert_eq!(doc["outcome"], "refused");
            assert_eq!(doc["code"], code);
            assert_eq!(doc["url"], "https://rusted.sh/f/thing");
        }
    }

    #[test]
    fn refusal_human_message_points_at_the_url() {
        let raw = json!({ "error": {
            "code": "method_mismatch",
            "message": "invoke sends POST, but this function answers GET — call its URL instead",
            "url": "https://rusted.sh/f/getter",
        }})
        .to_string();
        let report = invoke_report("getter", &Err(raw), false);
        assert_eq!(report.code, EXIT_REFUSED);
        assert!(report.stderr[0].contains("https://rusted.sh/f/getter"));
    }

    #[test]
    fn other_api_errors_exit_two() {
        let raw =
            json!({ "error": { "code": "no_such_env", "message": "no such environment: staje" }})
                .to_string();
        let report = invoke_report("fn", &Err(raw), true);
        assert_eq!(report.code, EXIT_USAGE);
        let doc: Value = serde_json::from_str(&report.stdout.unwrap()).unwrap();
        assert_eq!(doc["outcome"], "failed");
        assert_eq!(doc["message"], "no such environment: staje");
    }

    #[test]
    fn transport_gibberish_exits_two_and_passes_through() {
        let report = invoke_report("fn", &Err("connection refused".to_string()), false);
        assert_eq!(report.code, EXIT_USAGE);
        assert_eq!(report.stderr, vec!["connection refused"]);
    }
}
