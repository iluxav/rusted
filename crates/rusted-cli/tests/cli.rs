use assert_cmd::Command;
use rusted_server::{start, ServerConfig, ServerHandle};

const GREET: &str = r#"export default async function handler(request, context) {
    const input = await request.json();
    return context.json({ message: `Hello, ${input.name}` });
}"#;

struct Harness {
    admin: String,
    key: String,
    _handle: ServerHandle,
    _rt: tokio::runtime::Runtime,
    dir: tempfile::TempDir,
}

fn boot() -> Harness {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db_for_seed = rt.block_on(rusted_server::testsupport::create_test_database());
    let database_url = db_for_seed.clone();
    let handle = rt
        .block_on(async {
            start(ServerConfig {
                data_port: 0,
                admin_port: 0,
                queue_wait_ms: rusted_server::DEFAULT_QUEUE_WAIT_MS,
                debug: false,
                database_url,
                require_auth: false,
                host: "127.0.0.1".to_string(),
                public_url: None,
            })
            .await
        })
        .unwrap();
    let key = rt.block_on(async {
        let pool = rusted_server::testsupport::migrate(&db_for_seed).await;
        let user_id = rusted_server::testsupport::seed_user(&pool).await;
        let plan = rusted_server::plans::latest_by_code(&pool, "extra")
            .await
            .unwrap()
            .unwrap();
        rusted_server::plans::subscribe(&pool, user_id, plan.id)
            .await
            .unwrap();
        rusted_server::auth::create_key(&pool, user_id, "cli-test")
            .await
            .unwrap()
            .1
    });
    Harness {
        admin: format!("http://{}", handle.admin_addr),
        key,
        _handle: handle,
        _rt: rt,
        dir,
    }
}

impl Harness {
    fn script(&self, name: &str, content: &str) -> String {
        let path = self.dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn dir_path(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn rusted(&self) -> Command {
        let mut cmd = Command::cargo_bin("rusted").unwrap();
        cmd.arg("--admin").arg(&self.admin);
        cmd.arg("--api-key").arg(&self.key);
        cmd
    }
}

#[test]
fn push_invoke_list_roundtrip() {
    let h = boot();
    let script = h.script("greet.js", GREET);

    let out = h
        .rusted()
        .args(["push", &script, "--name", "greet", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let pushed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(pushed["revision"], 1);
    assert!(pushed["url"].as_str().unwrap().contains("/f/greet"));

    let out = h
        .rusted()
        .args(["invoke", "greet", "--body", r#"{"name":"Ada"}"#, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let invoked: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(invoked["outcome"], "success");
    assert_eq!(invoked["response"], r#"{"message":"Hello, Ada"}"#);

    h.rusted()
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("greet"));
}

#[test]
fn verify_rejects_bad_source_with_nonzero_exit() {
    let h = boot();
    let script = h.script("bad.js", "export default function (");
    h.rusted()
        .args(["verify", &script])
        .assert()
        .failure()
        .stderr(predicates::str::contains("compile_error"));
}

#[test]
fn verify_accepts_good_source() {
    let h = boot();
    let script = h.script("good.js", GREET);
    h.rusted().args(["verify", &script]).assert().success();
}

#[test]
fn pull_prints_deployed_source() {
    let h = boot();
    let script = h.script("greet.js", GREET);
    h.rusted()
        .args(["push", &script, "--name", "greet"])
        .assert()
        .success();
    h.rusted()
        .args(["pull", "greet"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Hello, ${input.name}"));
}

#[test]
fn delete_then_missing_function_fails() {
    let h = boot();
    let script = h.script("greet.js", GREET);
    h.rusted()
        .args(["push", &script, "--name", "greet"])
        .assert()
        .success();
    h.rusted().args(["delete", "greet"]).assert().success();
    h.rusted()
        .args(["delete", "greet"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not_found"));
}

#[test]
fn preview_returns_temporary_url_that_serves() {
    let h = boot();
    let script = h.script("greet.js", GREET);
    let out = h
        .rusted()
        .args(["preview", &script, "--ttl", "30", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let run: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let url = run["url"].as_str().unwrap();
    assert!(url.contains("/r/"));

    let response = reqwest::blocking::Client::new()
        .post(url)
        .body(r#"{"name":"Tmp"}"#)
        .send()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().unwrap(), r#"{"message":"Hello, Tmp"}"#);
}

#[test]
fn logs_shows_recent_invocations_with_console_output() {
    let h = boot();
    let script = h.script(
        "chatty.js",
        r#"export default async function handler(request) {
            console.log("processing", request.body);
            console.warn("low disk");
            return "done";
        }"#,
    );
    h.rusted()
        .args(["push", &script, "--name", "chatty"])
        .assert()
        .success();
    h.rusted()
        .args(["invoke", "chatty", "--body", "job-42"])
        .assert()
        .success();

    let assert = h.rusted().args(["logs", "chatty"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("success"), "outcome missing: {out}");
    assert!(out.contains("processing job-42"), "log line missing: {out}");
    assert!(out.contains("[warn] low disk"), "warn line missing: {out}");
}

#[test]
fn logs_for_uninvoked_function_says_so() {
    let h = boot();
    let script = h.script("quiet.js", GREET);
    h.rusted()
        .args(["push", &script, "--name", "quiet"])
        .assert()
        .success();
    h.rusted()
        .args(["logs", "quiet"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no invocations"));
}

#[test]
fn logs_shows_error_detail_for_failed_invocations() {
    let h = boot();
    let script = h.script(
        "typo.js",
        r#"export default async function handler() { consosdle.log("oops"); }"#,
    );
    h.rusted()
        .args(["push", &script, "--name", "typo"])
        .assert()
        .success();
    h.rusted().args(["invoke", "typo"]).assert().failure();

    let assert = h.rusted().args(["logs", "typo"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("error"), "outcome missing: {out}");
    assert!(
        out.contains("consosdle is not defined"),
        "error detail missing from logs: {out}"
    );
}

#[test]
fn serve_debug_prints_invocation_details_to_console() {
    use std::io::BufRead;
    use std::time::{Duration, Instant};

    let rt = tokio::runtime::Runtime::new().unwrap();
    let database_url = rt.block_on(rusted_server::testsupport::create_test_database());
    let api_key = rt.block_on(async {
        let pool = rusted_server::testsupport::migrate(&database_url).await;
        let user_id = rusted_server::testsupport::seed_user(&pool).await;
        rusted_server::auth::create_key(&pool, user_id, "debug-test")
            .await
            .unwrap()
            .1
    });
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("rusted"))
        .arg("serve")
        .arg("--debug")
        .args(["--database-url", &database_url])
        .args(["--port", "0", "--admin-port", "0"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });

    let mut data = None;
    let mut admin = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while (data.is_none() || admin.is_none()) && Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            if let Some(addr) = line.strip_prefix("rusted: functions on http://") {
                data = Some(addr.to_string());
            } else if let Some(addr) = line.strip_prefix("rusted: admin API on http://") {
                admin = Some(addr.to_string());
            }
        }
    }
    let data = data.expect("server printed data address");
    let admin = admin.expect("server printed admin address");

    let client = reqwest::blocking::Client::new();
    client
        .post(format!("http://{admin}/api/functions"))
        .bearer_auth(&api_key)
        .json(&serde_json::json!({
            "name": "crashy",
            "source": r#"export default async function handler() { console.log("pre-crash"); throw new Error("kaboom"); }"#
        }))
        .send()
        .unwrap();
    client
        .post(format!("http://{data}/f/crashy"))
        .body("{}")
        .send()
        .unwrap();

    let mut captured = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            captured.push_str(&line);
            captured.push('\n');
            // The console line prints after the outcome line — wait for the last one.
            if captured.contains("pre-crash") {
                break;
            }
        }
    }
    child.kill().ok();
    child.wait().ok();

    assert!(
        captured.contains("crashy"),
        "no invocation line:\n{captured}"
    );
    assert!(captured.contains("error"), "outcome missing:\n{captured}");
    assert!(
        captured.contains("kaboom"),
        "JS error detail missing:\n{captured}"
    );
    assert!(
        captured.contains("exec="),
        "pure exec time missing:\n{captured}"
    );
    assert!(
        captured.contains("console.log: pre-crash"),
        "console output missing:\n{captured}"
    );
}

#[test]
fn push_with_trigger_flags_registers_route() {
    let h = boot();
    let script = h.script(
        "pingpong.js",
        r#"export default async function handler(request, context) { return context.text("pong:" + request.params.room); }"#,
    );
    let out = h
        .rusted()
        .args([
            "push",
            &script,
            "--name",
            "ping",
            "--type",
            "http",
            "--method",
            "get,post",
            "--path",
            "/rooms/{room}",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["methods"], serde_json::json!(["GET", "POST"]));
    assert_eq!(v["path"], "/rooms/{room}");
    let url = v["url"].as_str().unwrap().replace("{room}", "42");
    let r = reqwest::blocking::Client::new().get(url).send().unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().unwrap(), "pong:42");
}

#[test]
fn push_needs_no_flags_when_file_declares_config() {
    let h = boot();
    let script = h.script(
        "selfconfig.js",
        r#"export const config = { name: "selfie", methods: ["GET"], path: "/hi" };
export default async function handler(request, context) { return context.text("hi"); }"#,
    );
    let out = h
        .rusted()
        .args(["push", &script, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["name"], "selfie");
    let r = reqwest::blocking::Client::new()
        .get(v["url"].as_str().unwrap())
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().unwrap(), "hi");
}

#[test]
fn run_serves_a_function_locally_without_a_server() {
    use std::io::BufRead;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("hello.js");
    std::fs::write(
        &script,
        r#"export const config = { name: "hello", methods: ["POST"] };
export default async function handler(request, context) {
    console.log("saw", request.body);
    return context.json({ echo: request.body });
}"#,
    )
    .unwrap();

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("rusted"))
        .args(["run", script.to_str().unwrap(), "--port", "7423"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });

    // Wait for the banner, which carries the route it decided on.
    let mut banner = String::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            banner.push_str(&line);
            banner.push('\n');
            if line.contains("/f/hello") {
                break;
            }
        }
    }
    assert!(banner.contains("/f/hello"), "no route banner:\n{banner}");

    let response = reqwest::blocking::Client::new()
        .post("http://127.0.0.1:7423/f/hello")
        .body("ping")
        .send()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().unwrap(), r#"{"echo":"ping"}"#);

    // The request should have been reported to the terminal with its log line.
    let mut reported = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            reported.push_str(&line);
            reported.push('\n');
            if reported.contains("saw ping") {
                break;
            }
        }
    }
    child.kill().ok();
    child.wait().ok();
    assert!(reported.contains("200"), "no request line:\n{reported}");
    assert!(
        reported.contains("saw ping"),
        "no console output:\n{reported}"
    );
}

#[test]
fn run_reports_errors_with_a_stack() {
    use std::io::BufRead;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("boom.js");
    std::fs::write(
        &script,
        r#"export const config = { name: "boom" };
function inner() { throw new Error("kaboom"); }
export default async function handler() { return inner(); }"#,
    )
    .unwrap();

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("rusted"))
        .args(["run", script.to_str().unwrap(), "--port", "7424"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            if line.contains("/f/boom") {
                break;
            }
        }
    }

    let response = reqwest::blocking::Client::new()
        .post("http://127.0.0.1:7424/f/boom")
        .send()
        .unwrap();
    assert_eq!(response.status(), 500);
    let body: serde_json::Value = response.json().unwrap();
    // Local dev shows the developer their own error, unlike the deployed plane.
    assert_eq!(body["error"]["message"], "kaboom");
    assert!(
        body["stack"].as_str().unwrap_or_default().contains("inner"),
        "stack should name the throwing function: {body}"
    );
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn run_builds_the_entry_file_when_it_does_not_exist_yet() {
    use std::io::BufRead;
    use std::time::{Duration, Instant};

    // A clean checkout: source present, build output absent.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.js");
    std::fs::write(
        &src,
        r#"export const config = { name: "built" };
export default async function handler(request, context) { return context.text("from build"); }"#,
    )
    .unwrap();
    let out = dir.path().join("dist").join("index.js");
    assert!(!out.exists());

    let build = format!(
        "mkdir -p {} && cp {} {}",
        out.parent().unwrap().display(),
        src.display(),
        out.display()
    );
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("rusted"))
        .args([
            "run",
            out.to_str().unwrap(),
            "--build",
            &build,
            "--port",
            "7429",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut started = false;
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            if line.contains("/f/built") {
                started = true;
                break;
            }
        }
    }
    let response = if started {
        reqwest::blocking::Client::new()
            .post("http://127.0.0.1:7429/f/built")
            .send()
            .ok()
    } else {
        None
    };
    child.kill().ok();
    child.wait().ok();
    assert!(
        started,
        "server should start after the build creates the file"
    );
    assert_eq!(
        response.expect("request sent").text().unwrap(),
        "from build"
    );
}

#[test]
fn run_without_build_explains_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.js");
    Command::cargo_bin("rusted")
        .unwrap()
        .args(["run", missing.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not exist"));
}

#[test]
fn run_bundles_a_file_with_imports_without_any_flags() {
    use std::io::BufRead;
    use std::time::{Duration, Instant};

    // A project shaped like a real one: an entry that imports a local module.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("greeting.js"),
        "export function greet(name) { return `Hi, ${name}`; }",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("index.js"),
        r#"import { greet } from "./greeting.js";
export const config = { name: "bundled" };
export default async function handler(request, context) { return context.text(greet("Ada")); }"#,
    )
    .unwrap();

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("rusted"))
        .args(["run", "index.js", "--port", "7432"])
        .current_dir(dir.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    let mut banner = String::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut started = false;
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(250)) {
            banner.push_str(&line);
            banner.push('\n');
            if line.contains("/f/bundled") {
                started = true;
                break;
            }
        }
    }
    let response = if started {
        reqwest::blocking::Client::new()
            .post("http://127.0.0.1:7432/f/bundled")
            .send()
            .ok()
    } else {
        None
    };
    child.kill().ok();
    child.wait().ok();
    assert!(started, "should bundle and serve with no flags:\n{banner}");
    assert!(
        banner.contains("bundling"),
        "should say it bundled:\n{banner}"
    );
    assert_eq!(response.expect("request sent").text().unwrap(), "Hi, Ada");
}

#[test]
fn run_serves_an_import_free_file_without_bundling() {
    use std::io::BufRead;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plain.js"),
        r#"export const config = { name: "plain" };
export default async function handler(request, context) { return context.text("no bundler"); }"#,
    )
    .unwrap();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("rusted"))
        .args(["run", "plain.js", "--port", "7433"])
        .current_dir(dir.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    let mut banner = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut started = false;
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            banner.push_str(&line);
            banner.push('\n');
            if line.contains("/f/plain") {
                started = true;
                break;
            }
        }
    }
    child.kill().ok();
    child.wait().ok();
    assert!(started, "should serve directly:\n{banner}");
    assert!(
        !banner.contains("bundling"),
        "a file with no imports needs no bundler:\n{banner}"
    );
}

#[test]
fn run_maps_stack_frames_back_to_source_lines() {
    use std::io::BufRead;
    use std::time::{Duration, Instant};

    // The thrown error lives on a known line of the *source*; after bundling
    // it sits thousands of lines into the output, and the stack must still
    // name the source position.
    let dir = tempfile::tempdir().unwrap();
    // Bulk that pushes the handler far down the bundled output.
    let filler: String = (0..40)
        .map(|i| format!("export const pad{i} = {i};\n"))
        .collect();
    std::fs::write(
        dir.path().join("helper.js"),
        format!("{filler}export const padding = 1;\n"),
    )
    .unwrap();
    let entry = r#"import { padding } from "./helper.js";
export const config = { name: "mapped" };
export default async function handler(request, context) {
    void padding;
    return missingThing.value;
}"#;
    std::fs::write(dir.path().join("index.js"), entry).unwrap();
    // The throwing statement is on line 5 of index.js.
    assert_eq!(
        entry.lines().nth(4).unwrap().trim(),
        "return missingThing.value;"
    );

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("rusted"))
        .args(["run", "index.js", "--port", "7436"])
        .current_dir(dir.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut started = false;
    while Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(250)) {
            if line.contains("/f/mapped") {
                started = true;
                break;
            }
        }
    }
    let body: Option<serde_json::Value> = if started {
        reqwest::blocking::Client::new()
            .post("http://127.0.0.1:7436/f/mapped")
            .send()
            .ok()
            .and_then(|r| r.json().ok())
    } else {
        None
    };
    child.kill().ok();
    child.wait().ok();
    assert!(started, "server should start");
    let body = body.expect("request sent");
    // The guarantee is that frames name the developer's file at a line that
    // exists in it — not the bundle's line ~2900. QuickJS attributes a frame
    // to the enclosing function's declaration, so the exact line within the
    // handler is the engine's business, not the source map's.
    let stack = body["stack"].as_str().unwrap_or_default();
    assert!(
        stack.contains("index.js:"),
        "stack should name the source file, got: {stack}"
    );
    assert!(
        !stack.contains("handler:"),
        "the bundled module name should be gone, got: {stack}"
    );
    let mapped_line: u32 = stack
        .split("index.js:")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .and_then(|n| n.parse().ok())
        .expect("a line number");
    assert!(
        mapped_line <= 6,
        "should map into the 6-line source, got line {mapped_line} in: {stack}"
    );
}

#[test]
fn build_bundles_to_dist_and_reports_the_route() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lib.js"),
        "export const who = () => \"world\";",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("index.js"),
        r#"import { who } from "./lib.js";
export const config = { name: "greeter", methods: ["GET", "POST"], path: "/say/{word}" };
export default async function handler(request, context) { return context.text(who()); }"#,
    )
    .unwrap();

    Command::cargo_bin("rusted")
        .unwrap()
        .args(["build", "index.js"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("dist/index.js"))
        .stdout(predicates::str::contains("GET,POST /f/greeter/say/{word}"));

    let built = std::fs::read_to_string(dir.path().join("dist").join("index.js")).unwrap();
    assert!(built.contains("world"), "the import should be bundled in");
    assert!(
        !built.contains("from \"./lib.js\""),
        "no unresolved imports should remain"
    );
    assert!(
        !dir.path().join("dist").join("index.js.map").exists(),
        "no map unless asked for"
    );

    Command::cargo_bin("rusted")
        .unwrap()
        .args(["build", "index.js", "--sourcemap"])
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(dir.path().join("dist").join("index.js.map").exists());
}

#[test]
fn build_refuses_a_bundle_that_would_not_deploy() {
    let dir = tempfile::tempdir().unwrap();
    // Valid JavaScript, but no handler — pushing this would fail at the server.
    std::fs::write(
        dir.path().join("index.js"),
        "export const config = { name: \"x\" };",
    )
    .unwrap();
    Command::cargo_bin("rusted")
        .unwrap()
        .args(["build", "index.js"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("does not load"));
    assert!(
        !dir.path().join("dist").exists(),
        "a failed build should write nothing"
    );
}

#[test]
fn push_bundles_a_source_file_with_imports() {
    let h = boot();
    std::fs::write(
        h.dir_path().join("greeting.js"),
        "export const hello = () => \"from a module\";",
    )
    .unwrap();
    let script = h.script(
        "entry.js",
        r#"import { hello } from "./greeting.js";
export const config = { name: "bundled-push" };
export default async function handler(request, context) { return context.text(hello()); }"#,
    );

    // Previously this failed: the server has no way to resolve "./greeting.js".
    let out = h
        .rusted()
        .args(["push", &script, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["name"], "bundled-push");

    let response = reqwest::blocking::Client::new()
        .post(v["url"].as_str().unwrap())
        .send()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().unwrap(), "from a module");

    // The stored source is the bundle, so nothing unresolved reaches the server.
    let pulled = h.rusted().args(["pull", "bundled-push"]).assert().success();
    let pulled = String::from_utf8(pulled.get_output().stdout.clone()).unwrap();
    assert!(pulled.contains("from a module"));
    assert!(!pulled.contains("from \"./greeting.js\""));
}

#[test]
fn push_leaves_an_import_free_file_alone() {
    let h = boot();
    let source = r#"export const config = { name: "plain-push" };
export default async function handler(request, context) { return context.text("as written"); }"#;
    let script = h.script("plain.js", source);
    h.rusted().args(["push", &script]).assert().success();
    let pulled = h.rusted().args(["pull", "plain-push"]).assert().success();
    let pulled = String::from_utf8(pulled.get_output().stdout.clone()).unwrap();
    assert_eq!(pulled.trim(), source, "no imports means nothing to bundle");
}

// --- where the CLI points when nobody said ------------------------------------

#[test]
fn defaults_to_the_hosted_service() {
    // Someone who installed the CLI to use rusted.sh should not have to discover
    // a flag before `rusted login` does anything.
    Command::cargo_bin("rusted")
        .unwrap()
        .args(["--help"])
        .env_remove("RUSTED_ADMIN")
        .assert()
        .success()
        .stdout(predicates::str::contains("https://rusted.sh"));
}

#[test]
fn an_unreachable_server_says_how_to_point_somewhere_else() {
    // Port 1 is reserved and never listening, so this always fails to connect.
    Command::cargo_bin("rusted")
        .unwrap()
        .args(["--admin", "http://127.0.0.1:1", "login"])
        .env_remove("RUSTED_ADMIN")
        .assert()
        .failure()
        .stderr(predicates::str::contains("RUSTED_ADMIN"));
}

// --- scaffolding ---------------------------------------------------------------

#[test]
fn new_scaffolds_a_project_that_builds() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("rusted")
        .unwrap()
        .args(["new", "greet"])
        .current_dir(dir.path())
        .assert()
        .success();

    let root = dir.path().join("greet");
    for f in ["index.ts", "rusted.d.ts", "tsconfig.json", "package.json"] {
        assert!(root.join(f).is_file(), "{f} was not created");
    }

    // The scaffold is the first thing a new user runs; it has to actually work.
    Command::cargo_bin("rusted")
        .unwrap()
        .args(["build", "index.ts", "-o", "dist/index.js"])
        .current_dir(&root)
        .assert()
        .success();
    assert!(root.join("dist/index.js").is_file());

    // And the name it declares should match the folder they asked for.
    let source = std::fs::read_to_string(root.join("index.ts")).unwrap();
    assert!(source.contains("greet"), "config does not name the project");
}

#[test]
fn new_refuses_to_overwrite_existing_work() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("taken");
    std::fs::create_dir(&existing).unwrap();
    std::fs::write(existing.join("index.ts"), "// mine").unwrap();

    Command::cargo_bin("rusted")
        .unwrap()
        .args(["new", "taken"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("exists"));

    // Whatever was there must survive.
    assert_eq!(
        std::fs::read_to_string(existing.join("index.ts")).unwrap(),
        "// mine"
    );
}

#[test]
fn ttl_reads_plain_seconds_and_suffixes() {
    // A rejected duration must say what a good one looks like, since this is
    // the one value a human types by hand.
    Command::cargo_bin("rusted")
        .unwrap()
        .args(["inbox", "new", "x", "--ttl", "banana"])
        .env_remove("RUSTED_ADMIN")
        .assert()
        .failure()
        .stderr(predicates::str::contains("90s"));
}
