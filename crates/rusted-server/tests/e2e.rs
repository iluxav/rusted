use rusted_server::{start, ServerConfig, ServerHandle};
use serde_json::{json, Value};

const GREET: &str = r#"export default async function handler(request, context) {
    const input = await request.json();
    console.log("greeting", input.name);
    return context.json({ message: `Hello, ${input.name}` });
}"#;

struct TestServer {
    handle: ServerHandle,
    client: reqwest::Client,
    key: String,
    pool: sqlx::PgPool,
    user_id: uuid::Uuid,
    _dir: tempfile::TempDir,
}

impl TestServer {
    fn admin(&self, path: &str) -> String {
        format!("http://{}{path}", self.handle.admin_addr)
    }
    fn data(&self, path: &str) -> String {
        format!("http://{}{path}", self.handle.data_addr)
    }
}

async fn boot() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    boot_in(dir).await
}

async fn boot_in(dir: tempfile::TempDir) -> TestServer {
    boot_with(dir, 1500).await
}

async fn boot_with(dir: tempfile::TempDir, queue_wait_ms: u64) -> TestServer {
    let database_url = rusted_server::testsupport::create_test_database().await;
    boot_full(dir, queue_wait_ms, database_url, false).await
}

/// Puts the caller back on Dev so plan limits are the free-tier ones.
async fn downgrade_to_dev(t: &TestServer) {
    let plan = rusted_server::plans::latest_by_code(&t.pool, "dev")
        .await
        .unwrap()
        .unwrap();
    rusted_server::plans::subscribe(&t.pool, t.user_id, plan.id)
        .await
        .unwrap();
    // Give the NOTIFY listener a moment to drop the cached plan.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}

/// Raises the caller's plan so limit-agnostic tests aren't tripped by Dev's
/// 4-function / 50 ms budget.
async fn upgrade_to_extra(t: &TestServer) {
    let plan = rusted_server::plans::latest_by_code(&t.pool, "extra")
        .await
        .unwrap()
        .expect("extra plan seeded");
    rusted_server::plans::subscribe(&t.pool, t.user_id, plan.id)
        .await
        .unwrap();
}

async fn boot_full(
    dir: tempfile::TempDir,
    queue_wait_ms: u64,
    database_url: String,
    require_auth: bool,
) -> TestServer {
    let url_for_seed = database_url.clone();
    let handle = start(ServerConfig {
        data_port: 0,
        admin_port: 0,
        queue_wait_ms,
        debug: false,
        database_url,
        require_auth,
        host: "127.0.0.1".to_string(),
        public_url: None,
    })
    .await
    .expect("server should start");
    let pool = rusted_server::testsupport::migrate(&url_for_seed).await;
    let user_id = rusted_server::testsupport::seed_user(&pool).await;
    let (_, key) = rusted_server::auth::create_key(&pool, user_id, "e2e")
        .await
        .expect("seed api key");
    let t = TestServer {
        handle,
        client: reqwest::Client::new(),
        key,
        pool,
        user_id,
        _dir: dir,
    };
    upgrade_to_extra(&t).await;
    t
}

async fn push(t: &TestServer, name: &str, source: &str) -> reqwest::Response {
    t.admin_post("/api/functions", json!({ "name": name, "source": source }))
        .await
}

impl TestServer {
    async fn admin_post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(self.admin(path))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn admin_get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(self.admin(path))
            .bearer_auth(&self.key)
            .send()
            .await
            .unwrap()
    }
    async fn admin_delete(&self, path: &str) -> reqwest::Response {
        self.client
            .delete(self.admin(path))
            .bearer_auth(&self.key)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn push_then_http_call_roundtrip() {
    let t = boot().await;
    let r = push(&t, "greet", GREET).await;
    assert_eq!(r.status(), 200);
    let pushed: Value = r.json().await.unwrap();
    assert_eq!(pushed["revision"], 1);
    let url = pushed["url"].as_str().expect("push returns url");

    let r = t
        .client
        .post(url)
        .body(r#"{"name":"Ada"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    assert_eq!(r.text().await.unwrap(), r#"{"message":"Hello, Ada"}"#);
}

#[tokio::test]
async fn invoke_returns_outcome_logs_and_timings() {
    let t = boot().await;
    push(&t, "greet", GREET).await;
    let r = t
        .admin_post(
            "/api/invoke",
            json!({ "name": "greet", "body": r#"{"name":"Bob"}"# }),
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["outcome"], "success");
    assert_eq!(v["response"], r#"{"message":"Hello, Bob"}"#);
    assert_eq!(v["logs"][0]["message"], "greeting Bob");
    assert!(v["wall_ms"].as_f64().unwrap() >= 0.0);
}

#[tokio::test]
async fn push_rejects_source_that_does_not_compile() {
    let t = boot().await;
    let r = push(&t, "broken", "export default function (").await;
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "compile_error");
}

#[tokio::test]
async fn push_rejects_invalid_names() {
    let t = boot().await;
    let r = push(&t, "Not A Valid/Name", GREET).await;
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "invalid_name");
}

#[tokio::test]
async fn unknown_function_is_404_everywhere() {
    let t = boot().await;
    assert_eq!(
        t.client
            .post(t.data("/f/ghost"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(t.admin_get("/api/functions/ghost").await.status(), 404);
}

#[tokio::test]
async fn temp_run_works_then_expires() {
    let t = boot().await;
    let r = t
        .admin_post("/api/runs", json!({ "source": GREET, "ttl_seconds": 1 }))
        .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let url = v["url"].as_str().unwrap().to_string();
    assert!(v["expires_at"].as_u64().is_some());

    let r = t
        .client
        .post(&url)
        .body(r#"{"name":"Tmp"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Condition-based wait: poll until the TTL sweep (or lazy check) kills it.
    for _ in 0..100 {
        let status = t.client.post(&url).send().await.unwrap().status();
        if status == 404 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("temp run never expired");
}

#[tokio::test]
async fn list_pull_delete_lifecycle() {
    let t = boot().await;
    push(&t, "greet", GREET).await;

    let v: Value = t.admin_get("/api/functions").await.json().await.unwrap();
    assert_eq!(v["functions"][0]["name"], "greet");

    let v: Value = t
        .admin_get("/api/functions/greet?source=true")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["source"], GREET);

    let r = t.admin_delete("/api/functions/greet").await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        t.client
            .post(t.data("/f/greet"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn oversized_request_body_is_rejected_before_execution() {
    let t = boot().await;
    push(&t, "greet", GREET).await;
    let r = t
        .client
        .post(t.data("/f/greet"))
        .body("x".repeat(300 * 1024))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "body_too_large");
}

#[tokio::test]
async fn function_error_maps_to_500_with_message() {
    let t = boot().await;
    push(
        &t,
        "cursed",
        r#"export default async function handler() { console.log("about to die"); throw new Error("boom"); }"#,
    )
    .await;
    let r = t.client.post(t.data("/f/cursed")).send().await.unwrap();
    assert_eq!(r.status(), 500);
    let body = r.text().await.unwrap();
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "function_error");
    // Endpoint callers are third parties: no JS internals, no console output.
    assert!(!body.contains("boom"), "JS error leaked to caller: {body}");
    assert!(
        !body.contains("about to die"),
        "logs leaked to caller: {body}"
    );

    // The owner sees the real error through the invocation history.
    let v: Value = t
        .admin_get("/api/functions/cursed")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["recent"][0]["outcome"], "error");
    assert!(v["recent"][0]["detail"].as_str().unwrap().contains("boom"));
    assert_eq!(v["recent"][0]["logs"][0]["message"], "about to die");
}

#[tokio::test]
async fn hostile_function_maps_to_limit_exceeded() {
    let t = boot().await;
    push(
        &t,
        "spinner",
        r#"export default async function handler() { while (true) {} }"#,
    )
    .await;
    let r = t.client.post(t.data("/f/spinner")).send().await.unwrap();
    assert_eq!(r.status(), 429);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "limit_exceeded");
}

#[tokio::test]
async fn verify_endpoint_accepts_and_rejects() {
    let t = boot().await;
    let ok = t
        .admin_post("/api/verify", json!({ "source": GREET }))
        .await;
    assert_eq!(ok.status(), 200);

    let bad = t
        .admin_post("/api/verify", json!({ "source": "export default (" }))
        .await;
    assert_eq!(bad.status(), 422);
}

#[tokio::test]
async fn concurrent_invocations_of_one_function_serialize() {
    let t = boot().await;
    push(
        &t,
        "busy",
        r#"export default async function handler() {
            let x = 0;
            for (let i = 0; i < 500000; i++) { x += i; }
            return String(x);
        }"#,
    )
    .await;
    let mut handles = Vec::new();
    for _ in 0..4 {
        let client = t.client.clone();
        let url = t.data("/f/busy");
        handles.push(tokio::spawn(async move {
            client.post(url).send().await.unwrap().status()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), 200);
    }
}

#[tokio::test]
async fn functions_survive_server_restart() {
    let database_url = rusted_server::testsupport::create_test_database().await;
    let dir = tempfile::tempdir().unwrap();
    let t = boot_full(dir, 1500, database_url.clone(), false).await;
    push(&t, "greet", GREET).await;
    let dir = t._dir;
    drop(t.handle);

    let t = boot_full(dir, 1500, database_url, false).await;
    let r = t
        .client
        .post(t.data("/f/greet"))
        .body(r#"{"name":"Back"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), r#"{"message":"Hello, Back"}"#);
}

#[tokio::test]
async fn calls_beyond_the_plans_concurrency_are_turned_away() {
    let dir = tempfile::tempdir().unwrap();
    let t = boot_with(dir, 10).await;
    downgrade_to_dev(&t).await;
    let dev = rusted_server::plans::latest_by_code(&t.pool, "dev")
        .await
        .unwrap()
        .unwrap();
    push(
        &t,
        "spinner",
        r#"export default async function handler() { while (true) {} }"#,
    )
    .await;

    // Occupy every permit the plan grants, then ask for one more.
    let mut running = Vec::new();
    for _ in 0..dev.limits.concurrency {
        let client = t.client.clone();
        let url = t.data("/f/spinner");
        running.push(tokio::spawn(async move {
            client.post(url).send().await.unwrap()
        }));
    }
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    let extra = t.client.post(t.data("/f/spinner")).send().await.unwrap();
    assert_eq!(extra.status(), 429);
    let v: Value = extra.json().await.unwrap();
    assert_eq!(v["error"]["code"], "busy");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains(&dev.limits.concurrency.to_string()));

    // The occupants run to their wall deadline and report the limit.
    for handle in running {
        let v: Value = handle.await.unwrap().json().await.unwrap();
        assert_eq!(v["error"]["code"], "limit_exceeded");
    }
}

#[tokio::test]
async fn adhoc_invoke_with_source_works() {
    let t = boot().await;
    let r = t.admin_post("/api/invoke", json!({
            "source": "export default async function handler(request) { return request.body.toUpperCase(); }",
            "body": "shout"
        })).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["outcome"], "success");
    assert_eq!(v["response"], "SHOUT");
}

#[tokio::test]
async fn list_includes_live_temp_runs() {
    let t = boot().await;
    t.admin_post("/api/runs", json!({ "source": GREET, "ttl_seconds": 60 }))
        .await;
    let v: Value = t.admin_get("/api/functions").await.json().await.unwrap();
    assert_eq!(v["runs"].as_array().unwrap().len(), 1);
    assert!(v["runs"][0]["url"].as_str().unwrap().contains("/r/"));
}

#[tokio::test]
async fn detail_shows_recent_invocations_and_delete_clears_them() {
    let t = boot().await;
    push(&t, "greet", GREET).await;
    t.client
        .post(t.data("/f/greet"))
        .body(r#"{"name":"Ada"}"#)
        .send()
        .await
        .unwrap();
    let v: Value = t
        .admin_get("/api/functions/greet")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["recent"][0]["outcome"], "success");
    assert_eq!(v["recent"][0]["logs"][0]["message"], "greeting Ada");

    t.admin_delete("/api/functions/greet").await;
    push(&t, "greet", GREET).await;
    let v: Value = t
        .admin_get("/api/functions/greet")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        v["recent"].as_array().unwrap().len(),
        0,
        "records must not survive delete + re-push"
    );
}

#[tokio::test]
async fn sweeping_expired_runs_also_prunes_locks_and_records() {
    use rusted_server::state::{AppState, TempRun};
    use rusted_server::store::Store;

    let database_url = rusted_server::testsupport::create_test_database().await;
    let pool = rusted_server::testsupport::migrate(&database_url).await;
    let (recorder, _writer) = rusted_server::analytics::start(pool.clone());
    let state = std::sync::Arc::new(AppState::new(
        Store::new(pool.clone()),
        pool,
        recorder,
        1500,
        false,
        false,
        None,
    ));
    state.temp_runs.lock().unwrap().insert(
        "dead".into(),
        TempRun {
            source: String::new(),
            expires_at: 0,
        },
    );
    state.fn_locks.lock().unwrap().insert(
        "run:dead".into(),
        (1, std::sync::Arc::new(tokio::sync::Semaphore::new(1))),
    );
    state
        .records
        .lock()
        .unwrap()
        .insert("run:dead".into(), Default::default());

    rusted_server::api::sweep_once(&state);

    assert!(state.temp_runs.lock().unwrap().is_empty());
    assert!(state.fn_locks.lock().unwrap().is_empty(), "lock leaked");
    assert!(state.records.lock().unwrap().is_empty(), "records leaked");
}

#[tokio::test]
async fn context_text_beats_json_sniffing_on_the_data_plane() {
    let t = boot().await;
    // "123" parses as JSON, so sniffing alone would mislabel it.
    push(
        &t,
        "texty",
        r#"export default async function handler(request, context) { return context.text("123"); }"#,
    )
    .await;
    let r = t.client.post(t.data("/f/texty")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    assert_eq!(r.text().await.unwrap(), "123");
}

#[tokio::test]
async fn push_and_run_report_size_and_allocated_limits() {
    let t = boot().await;
    let r = push(&t, "greet", GREET).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["size_bytes"], GREET.len());
    assert_eq!(v["limits"]["plan"], "Extra");
    assert_eq!(v["limits"]["wall_ms"], 30000);
    assert_eq!(v["limits"]["memory_bytes"], 32 * 1024 * 1024);
    assert_eq!(v["limits"]["request_body_bytes"], 256 * 1024);
    assert_eq!(v["limits"]["response_body_bytes"], 256 * 1024);
    let extra = rusted_server::plans::latest_by_code(&t.pool, "extra")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v["limits"]["outbound_reqs"], extra.limits.outbound_reqs);
    assert_eq!(v["limits"]["concurrency"], extra.limits.concurrency);

    let r = t
        .admin_post("/api/runs", json!({ "source": GREET, "ttl_seconds": 30 }))
        .await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["size_bytes"], GREET.len());
    assert_eq!(v["limits"]["wall_ms"], 30000);
}

#[tokio::test]
async fn method_and_path_routing_with_params() {
    let t = boot().await;
    let r = t.admin_post("/api/functions", json!({
            "name": "api",
            "source": r#"export default async function handler(request, context) {
                return context.json({ id: request.params.id, m: request.method, v: request.query.verbose ?? null });
            }"#,
            "methods": ["GET", "POST"],
            "path": "/users/{id}"
        })).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["methods"], json!(["GET", "POST"]));
    assert_eq!(v["path"], "/users/{id}");
    assert!(v["url"].as_str().unwrap().ends_with("/f/api/users/{id}"));

    let r = t
        .client
        .get(t.data("/f/api/users/42?verbose=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), r#"{"id":"42","m":"GET","v":"1"}"#);

    let r = t
        .client
        .delete(t.data("/f/api/users/42"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 405);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "method_not_allowed");

    assert_eq!(
        t.client
            .get(t.data("/f/api/users"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        t.client
            .get(t.data("/f/api"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn push_rejects_invalid_trigger_config() {
    let t = boot().await;
    let cases = [
        json!({ "name": "x", "source": GREET, "methods": ["TELEPORT"] }),
        json!({ "name": "x", "source": GREET, "path": "/a?b=c" }),
        json!({ "name": "x", "source": GREET, "path": "users" }),
        json!({ "name": "x", "source": GREET, "type": "cron" }),
    ];
    for case in cases {
        let r = t.admin_post("/api/functions", case.clone()).await;
        assert_eq!(r.status(), 422, "should reject: {case}");
    }
}

#[tokio::test]
async fn push_without_trigger_fields_preserves_existing_route() {
    let t = boot().await;
    let ping = r#"export default async function handler(request, context) { return context.text("pong"); }"#;
    t.admin_post(
        "/api/functions",
        json!({ "name": "api", "source": ping, "methods": ["GET"], "path": "/ping" }),
    )
    .await;
    // Plain re-push (new revision) must not wipe the trigger config.
    push(&t, "api", ping).await;
    let r = t.client.get(t.data("/f/api/ping")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "pong");
}

const CONFIGURED: &str = r#"export const config = { name: "cfg-fn", methods: ["GET"], path: "/ping" };
export default async function handler(request, context) { return context.text("pong"); }"#;

#[tokio::test]
async fn push_reads_intent_from_config_export() {
    let t = boot().await;
    // No name, no methods, no path in the request — everything from the file.
    let r = t
        .admin_post("/api/functions", json!({ "source": CONFIGURED }))
        .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["name"], "cfg-fn");
    assert_eq!(v["methods"], json!(["GET"]));
    assert_eq!(v["path"], "/ping");

    let r = t.client.get(t.data("/f/cfg-fn/ping")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "pong");
}

#[tokio::test]
async fn explicit_fields_override_config_export() {
    let t = boot().await;
    let r = t
        .admin_post(
            "/api/functions",
            json!({ "name": "override", "source": CONFIGURED, "methods": ["POST"] }),
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["name"], "override");
    assert_eq!(v["methods"], json!(["POST"]));
    // Path still comes from the file config.
    assert_eq!(v["path"], "/ping");
}

#[tokio::test]
async fn push_without_any_name_is_rejected() {
    let t = boot().await;
    let r = t
        .admin_post("/api/functions", json!({ "source": GREET }))
        .await;
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "missing_name");
}

#[tokio::test]
async fn verify_reports_discovered_config() {
    let t = boot().await;
    let r = t
        .admin_post("/api/verify", json!({ "source": CONFIGURED }))
        .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["valid"], true);
    assert_eq!(v["config"]["name"], "cfg-fn");

    // A typo in the config export fails verify with a pointed message.
    let r = t.admin_post("/api/verify", json!({ "source": "export const config = { metods: [\"GET\"] };\nexport default async function handler() {}" })).await;
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("metods"));
}

#[tokio::test]
async fn require_auth_gates_the_data_plane() {
    let database_url = rusted_server::testsupport::create_test_database().await;
    let dir = tempfile::tempdir().unwrap();
    let t = boot_full(dir, 1500, database_url, true).await;
    push(&t, "greet", GREET).await;
    let pool = t.pool.clone();
    let user_id = t.user_id;
    let (key_id, token) = rusted_server::auth::create_key(&pool, user_id, "test-key")
        .await
        .unwrap();

    // No key → 401 with the standard envelope.
    let r = t.client.post(t.data("/f/greet")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "unauthorized");

    // Wrong key → 401.
    let r = t
        .client
        .post(t.data("/f/greet"))
        .bearer_auth("rk_live_999_deadbeef")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Valid key → the function runs.
    let r = t
        .client
        .post(t.data("/f/greet"))
        .bearer_auth(&token)
        .body(r#"{"name":"Ada"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Revoke + NOTIFY → the cached verdict dies and calls start failing.
    rusted_server::auth::revoke_key(
        &pool,
        &rusted_server::auth::AuthCaches::default(),
        user_id,
        key_id,
    )
    .await
    .unwrap();
    for _ in 0..100 {
        let status = t
            .client
            .post(t.data("/f/greet"))
            .bearer_auth(&token)
            .body(r#"{"name":"Ada"}"#)
            .send()
            .await
            .unwrap()
            .status();
        if status == 401 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("revoked key never stopped working");
}

#[tokio::test]
async fn dev_plan_caps_function_count() {
    let t = boot().await;
    downgrade_to_dev(&t).await;
    let dev = rusted_server::plans::latest_by_code(&t.pool, "dev")
        .await
        .unwrap()
        .unwrap();
    for i in 0..dev.limits.max_functions {
        let r = push(&t, &format!("fn-{i}"), GREET).await;
        assert_eq!(r.status(), 200, "function {i} should fit the Dev plan");
    }
    let r = push(&t, "fn-overflow", GREET).await;
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "plan_limit");
    assert!(v["error"]["message"].as_str().unwrap().contains("Dev"));

    // Re-pushing an existing function doesn't consume another slot.
    assert_eq!(push(&t, "fn-0", GREET).await.status(), 200);
}

#[tokio::test]
async fn dev_plan_caps_script_size() {
    let t = boot().await;
    downgrade_to_dev(&t).await;
    let padding = "/".repeat(300 * 1024);
    let big = format!("{padding}\n{GREET}");
    let r = push(&t, "chonky", &big).await;
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "plan_limit");
}

#[tokio::test]
async fn rate_limit_returns_429_with_retry_after() {
    let t = boot().await;
    push(
        &t,
        "chatty",
        r#"export default async function handler(request, context) { return context.text("ok"); }"#,
    )
    .await;
    // Dev allows 60/min; the harness is on Extra (600/min), so drive Dev.
    downgrade_to_dev(&t).await;
    let mut limited = None;
    for _ in 0..70 {
        let r = t.client.post(t.data("/f/chatty")).send().await.unwrap();
        if r.status() == 429 {
            limited = Some(r);
            break;
        }
    }
    let r = limited.expect("rate limit should trip within 70 calls");
    assert!(r.headers().contains_key("retry-after"));
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "rate_limited");
}

#[tokio::test]
async fn plan_execution_budget_replaces_the_fixed_wall_limit() {
    let t = boot().await;
    // A ~120ms busy loop: fits Extra's 30s budget, exceeds Dev's 50ms.
    let slow = r#"export default async function handler(request, context) {
        const until = Date.now() + 120;
        while (Date.now() < until) {}
        return context.text("done");
    }"#;
    push(&t, "slow", slow).await;
    let r = t.client.post(t.data("/f/slow")).send().await.unwrap();
    assert_eq!(r.status(), 200, "Extra's 30s budget should allow 120ms");

    downgrade_to_dev(&t).await;
    let r = t.client.post(t.data("/f/slow")).send().await.unwrap();
    assert_eq!(r.status(), 429, "Dev's 50ms budget should terminate it");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "limit_exceeded");
}

#[tokio::test]
async fn functions_are_scoped_to_their_owner() {
    let t = boot().await;
    push(&t, "mine", GREET).await;

    // A second account with its own key sees none of the first's functions.
    let other_user = rusted_server::testsupport::seed_user(&t.pool).await;
    let (_, other_key) = rusted_server::auth::create_key(&t.pool, other_user, "other")
        .await
        .unwrap();
    let v: Value = t
        .client
        .get(t.admin("/api/functions"))
        .bearer_auth(&other_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["functions"].as_array().unwrap().len(), 0);

    let r = t
        .client
        .get(t.admin("/api/functions/mine"))
        .bearer_auth(&other_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn admin_api_rejects_calls_without_a_key() {
    let t = boot().await;
    let r = t
        .client
        .get(t.admin("/api/functions"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "unauthorized");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("RUSTED_API_KEY"));
}

/// Console pages have no UI tests by design, but they must never panic — a
/// column type changing under them used to surface only in the browser.
#[tokio::test]
async fn every_console_page_renders() {
    let t = boot().await;
    push(&t, "greet", GREET).await;
    // A key and an invocation so the keys/lambda pages have real rows to render.
    rusted_server::auth::create_key(&t.pool, t.user_id, "smoke")
        .await
        .unwrap();
    t.client
        .post(t.data("/f/greet"))
        .body(r#"{"name":"Ada"}"#)
        .send()
        .await
        .unwrap();

    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let cookie = format!("rusted_session={session}");
    for path in [
        "/",
        "/login",
        "/console",
        "/console/dashboard",
        "/console/keys",
        "/console/billing",
        "/console/checkout/pro",
        "/console/lambda/greet",
        "/console/lambda/does-not-exist",
        "/console/invocations",
        "/console/invocations?function=greet&errors=1",
        "/console/invocations?page=2",
        "/device",
        "/device?code=ZZZZ-ZZZZ",
    ] {
        let r = t
            .client
            .get(format!("http://{}{path}", t.handle.admin_addr))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path} should render");
        let body = r.text().await.unwrap();
        assert!(!body.is_empty(), "{path} rendered nothing");
    }
}

#[tokio::test]
async fn console_pages_redirect_when_signed_out() {
    let t = boot().await;
    for path in ["/console", "/console/keys", "/console/billing"] {
        let r = t
            .client
            .get(format!("http://{}{path}", t.handle.admin_addr))
            .send()
            .await
            .unwrap();
        // reqwest follows the redirect to /login, which renders.
        assert_eq!(r.status(), 200);
        assert!(
            r.url().path() == "/login",
            "{path} should send a signed-out visitor to /login, landed on {}",
            r.url().path()
        );
    }
}

#[tokio::test]
async fn invocations_are_recorded_without_blocking_the_request() {
    let t = boot().await;
    push(&t, "greet", GREET).await;
    push(
        &t,
        "cursed",
        r#"export default async function handler() { throw new Error("nope"); }"#,
    )
    .await;
    for _ in 0..3 {
        t.client
            .post(t.data("/f/greet"))
            .body(r#"{"name":"Ada"}"#)
            .send()
            .await
            .unwrap();
    }
    t.client.post(t.data("/f/cursed")).send().await.unwrap();

    // The writer batches, so poll rather than assuming a synchronous write.
    let mut summary = rusted_server::analytics::summary(&t.pool, t.user_id, 7).await;
    for _ in 0..40 {
        if summary.invocations >= 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        summary = rusted_server::analytics::summary(&t.pool, t.user_id, 7).await;
    }
    assert_eq!(summary.invocations, 4, "all four calls should be recorded");
    assert_eq!(summary.errors, 1, "the throwing function is one error");
    assert!(summary.p95_exec_ms > 0.0, "exec time should be recorded");

    let recent = rusted_server::analytics::recent(&t.pool, t.user_id, 10, 0, None, false).await;
    assert_eq!(recent.len(), 4);
    let failed = recent.iter().find(|r| r.outcome == "error").unwrap();
    assert_eq!(failed.function, "cursed");
    assert!(failed.detail.as_deref().unwrap().contains("nope"));

    let buckets = rusted_server::analytics::per_day(&t.pool, t.user_id, 7).await;
    assert_eq!(buckets.len(), 7, "every day in the window gets a bucket");
    assert_eq!(buckets.last().unwrap().value, 4, "today holds all four");
}

#[tokio::test]
async fn a_full_analytics_queue_never_fails_a_request() {
    let t = boot().await;
    push(&t, "greet", GREET).await;
    // Far more calls than the batch size, back to back: the recorder may shed
    // records under pressure, but every request must still succeed.
    for _ in 0..40 {
        let r = t
            .client
            .post(t.data("/f/greet"))
            .body(r#"{"name":"Ada"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
}

#[tokio::test]
async fn invocation_filters_narrow_by_function_and_outcome() {
    let t = boot().await;
    push(&t, "greet", GREET).await;
    push(
        &t,
        "cursed",
        r#"export default async function handler() { throw new Error("kaboom"); }"#,
    )
    .await;
    for _ in 0..3 {
        t.client
            .post(t.data("/f/greet"))
            .body(r#"{"name":"Ada"}"#)
            .send()
            .await
            .unwrap();
    }
    t.client.post(t.data("/f/cursed")).send().await.unwrap();

    let mut all = rusted_server::analytics::recent(&t.pool, t.user_id, 25, 0, None, false).await;
    for _ in 0..40 {
        if all.len() >= 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        all = rusted_server::analytics::recent(&t.pool, t.user_id, 25, 0, None, false).await;
    }
    assert_eq!(all.len(), 4);
    assert!(all.iter().all(|r| r.wall_ms > 0.0 && r.cpu_ms >= 0.0));

    let errors = rusted_server::analytics::recent(&t.pool, t.user_id, 25, 0, None, true).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].function, "cursed");
    assert!(errors[0].detail.as_deref().unwrap().contains("kaboom"));

    let greets =
        rusted_server::analytics::recent(&t.pool, t.user_id, 25, 0, Some("greet"), false).await;
    assert_eq!(greets.len(), 3);

    // Both filters together: greet has no failures.
    let none =
        rusted_server::analytics::recent(&t.pool, t.user_id, 25, 0, Some("greet"), true).await;
    assert!(none.is_empty());

    let names = rusted_server::analytics::invoked_functions(&t.pool, t.user_id).await;
    assert_eq!(names, vec!["cursed".to_string(), "greet".to_string()]);
}

#[tokio::test]
async fn invocation_pages_are_twenty_rows() {
    let t = boot().await;
    push(&t, "greet", GREET).await;
    for _ in 0..25 {
        t.client
            .post(t.data("/f/greet"))
            .body(r#"{"name":"Ada"}"#)
            .send()
            .await
            .unwrap();
    }
    let mut all = rusted_server::analytics::recent(&t.pool, t.user_id, 30, 0, None, false).await;
    for _ in 0..40 {
        if all.len() >= 25 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        all = rusted_server::analytics::recent(&t.pool, t.user_id, 30, 0, None, false).await;
    }
    assert_eq!(all.len(), 25);

    let first = rusted_server::analytics::recent(&t.pool, t.user_id, 20, 0, None, false).await;
    let second = rusted_server::analytics::recent(&t.pool, t.user_id, 20, 20, None, false).await;
    assert_eq!(first.len(), 20);
    assert_eq!(second.len(), 5, "the tail lands on page two");
    // Pages must not overlap: newest-first ordering with a clean offset.
    assert!(first.last().unwrap().at >= second.first().unwrap().at);
}

#[tokio::test]
async fn plan_endpoint_reports_the_callers_tier() {
    let t = boot().await; // the harness subscribes to Extra
    let v: Value = t.admin_get("/api/plan").await.json().await.unwrap();
    assert_eq!(v["code"], "extra");
    assert_eq!(v["limits"]["wall_ms"], 30000);

    let r = t.client.get(t.admin("/api/plan")).send().await.unwrap();
    assert_eq!(r.status(), 401, "the plan is not public");
}

#[tokio::test]
async fn a_plan_allows_parallel_calls_to_one_function() {
    let t = boot().await; // Extra: 20 concurrent
    push(
        &t,
        "busy",
        r#"export default async function handler(request, context) {
            const until = Date.now() + 120;
            while (Date.now() < until) {}
            return context.text("done");
        }"#,
    )
    .await;

    // Eight calls at once. Serialized at 120ms each that is ~960ms; in
    // parallel it should finish in a fraction of that, and none may 429.
    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let client = t.client.clone();
        let url = t.data("/f/busy");
        handles.push(tokio::spawn(async move {
            client.post(url).send().await.unwrap().status()
        }));
    }
    for handle in handles {
        assert_eq!(handle.await.unwrap(), 200, "no call should be turned away");
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(900),
        "calls should overlap, took {elapsed:?}"
    );
}

#[tokio::test]
async fn the_dev_plan_can_reach_the_internet() {
    // The free tier has to be able to demonstrate fetch, or every example
    // that calls an API starts at the paid tier.
    let t = boot().await;
    downgrade_to_dev(&t).await;
    let plan = rusted_server::plans::latest_by_code(&t.pool, "dev")
        .await
        .unwrap()
        .unwrap();
    assert!(
        plan.limits.outbound_reqs >= 2,
        "Dev should allow outbound calls, got {}",
        plan.limits.outbound_reqs
    );
    assert!(
        plan.limits.concurrency >= 2,
        "Dev should allow parallel calls"
    );
    assert_eq!(
        plan.version, 2,
        "the newest Dev version is the generous one"
    );
}

#[tokio::test]
async fn device_flow_grants_a_key_once_a_human_approves() {
    let t = boot().await;

    // The client has no credential yet — that's the point of this flow.
    let start: Value = t
        .client
        .post(t.admin("/api/device/code"))
        .json(&json!({ "label": "cli on laptop" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let device_code = start["device_code"].as_str().unwrap().to_string();
    let user_code = start["user_code"].as_str().unwrap().to_string();
    assert!(start["verification_uri"]
        .as_str()
        .unwrap()
        .ends_with("/device"));

    let poll = |code: String| {
        let client = t.client.clone();
        let url = t.admin("/api/device/token");
        async move {
            client
                .post(url)
                .json(&json!({ "device_code": code }))
                .send()
                .await
                .unwrap()
        }
    };

    // Before approval, polling says exactly that.
    let v: Value = poll(device_code.clone()).await.json().await.unwrap();
    assert_eq!(v["error"]["code"], "authorization_pending");

    // A human approves it in the console.
    assert!(
        rusted_server::device::decide(&t.pool, &user_code, t.user_id, true)
            .await
            .unwrap()
    );

    let v: Value = poll(device_code.clone()).await.json().await.unwrap();
    let key = v["api_key"].as_str().expect("a key after approval");
    assert!(key.starts_with("rk_live_"));

    // The granted key really works.
    let r = t
        .client
        .get(t.admin("/api/functions"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // And the code cannot be redeemed twice.
    let v: Value = poll(device_code).await.json().await.unwrap();
    assert_eq!(v["error"]["code"], "expired_token");
}

#[tokio::test]
async fn a_declined_device_request_grants_nothing() {
    let t = boot().await;
    let start: Value = t
        .client
        .post(t.admin("/api/device/code"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let user_code = start["user_code"].as_str().unwrap().to_string();

    assert!(
        rusted_server::device::decide(&t.pool, &user_code, t.user_id, false)
            .await
            .unwrap()
    );
    let v: Value = t
        .client
        .post(t.admin("/api/device/token"))
        .json(&json!({ "device_code": start["device_code"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["error"]["code"], "access_denied");

    // A decided request is no longer pending, so it can't be flipped later.
    assert!(
        !rusted_server::device::decide(&t.pool, &user_code, t.user_id, true)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn an_unknown_device_code_reveals_nothing() {
    let t = boot().await;
    let v: Value = t
        .client
        .post(t.admin("/api/device/token"))
        .json(&json!({ "device_code": "not-a-real-code" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["error"]["code"], "expired_token");
    assert!(rusted_server::device::lookup(&t.pool, "ZZZZ-ZZZZ")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_function_can_set_its_status_and_headers_over_http() {
    let t = boot().await;
    push(
        &t,
        "accepted",
        r#"export default async function handler(request, context) {
            return context.json({ queued: true }, {
                status: 202,
                headers: { "x-request-id": "r-42", "cache-control": "no-store" },
            });
        }"#,
    )
    .await;
    let r = t.client.post(t.data("/f/accepted")).send().await.unwrap();
    assert_eq!(r.status(), 202);
    assert_eq!(r.headers().get("x-request-id").unwrap(), "r-42");
    assert_eq!(r.headers().get("cache-control").unwrap(), "no-store");
    // Framing stays the platform's business.
    assert!(r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    assert_eq!(r.text().await.unwrap(), r#"{"queued":true}"#);
}

#[tokio::test]
async fn a_refused_header_fails_the_call_rather_than_shipping_it() {
    let t = boot().await;
    push(
        &t,
        "sneaky",
        r#"export default async function handler(request, context) {
            return context.text("x", { headers: { "content-length": "0" } });
        }"#,
    )
    .await;
    let r = t.client.post(t.data("/f/sneaky")).send().await.unwrap();
    assert_eq!(r.status(), 500);
    // The caller learns nothing about why; the owner sees it in the history.
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "function_error");
    let detail = rusted_server::analytics::recent(&t.pool, t.user_id, 5, 0, None, true).await;
    for _ in 0..30 {
        if !detail.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
