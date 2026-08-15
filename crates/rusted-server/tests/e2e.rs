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

    // The claim: with every permit occupied, one more simultaneous call is
    // turned away with `busy` naming the plan's allowance. Which of the
    // racers gets refused is scheduling, not semantics — so fire one more
    // call than there are permits, all at once, and require that someone
    // was refused. Rounds absorb the pathological schedule where a loaded
    // machine serializes the requests entirely.
    let mut saw_busy = false;
    'rounds: for _ in 0..10 {
        let mut racers = Vec::new();
        for _ in 0..=dev.limits.concurrency {
            let client = t.client.clone();
            let url = t.data("/f/spinner");
            racers.push(tokio::spawn(async move {
                client.post(url).send().await.unwrap()
            }));
        }
        for handle in racers {
            let response = handle.await.unwrap();
            let status = response.status();
            let v: Value = response.json().await.unwrap();
            if status == 429 && v["error"]["code"] == "busy" {
                assert!(v["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(&dev.limits.concurrency.to_string()));
                saw_busy = true;
            } else {
                // Ran instead: a spinner that spins to Dev's wall limit.
                assert_eq!(v["error"]["code"], "limit_exceeded", "{v}");
            }
        }
        if saw_busy {
            break 'rounds;
        }
    }
    assert!(
        saw_busy,
        "no call was ever refused while every permit was held"
    );
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

const CONFIGURED: &str = r#"export const http = { name: "cfg-fn", methods: ["GET"], path: "/ping" };
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

const MCP_FN: &str = r#"
export const mcp = {
  name: "sluggy",
  tools: {
    slugify: {
      description: "Turn a title into a URL slug",
      inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
      async handler({ text }) { return text.toLowerCase().replace(/[^a-z0-9]+/g, "-"); },
    },
  },
};
"#;

#[tokio::test]
async fn pushing_an_mcp_module_deploys_an_mcp_function() {
    let t = boot().await;
    let r = t
        .admin_post("/api/functions", json!({ "source": MCP_FN }))
        .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["name"], "sluggy");
    assert_eq!(v["kind"], "mcp");
    assert_eq!(v["tools"], json!(["slugify"]));
    assert!(v["url"].as_str().unwrap().ends_with("/f/sluggy"));
}

#[tokio::test]
async fn an_mcp_push_refuses_http_trigger_fields() {
    let t = boot().await;
    let cases = [
        json!({ "source": MCP_FN, "methods": ["GET"] }),
        json!({ "source": MCP_FN, "path": "/slugs" }),
    ];
    for case in cases {
        let r = t.admin_post("/api/functions", case.clone()).await;
        assert_eq!(r.status(), 422, "should reject: {case}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["code"], "unsupported_trigger");
    }
}

#[tokio::test]
async fn a_module_with_both_surfaces_is_a_compile_error() {
    let t = boot().await;
    let both = format!("{MCP_FN}\nexport default async function h() {{}}");
    let r = t
        .admin_post("/api/functions", json!({ "source": both }))
        .await;
    assert_eq!(r.status(), 422);
}

#[tokio::test]
async fn redeploying_an_http_function_as_mcp_switches_its_kind() {
    let t = boot().await;
    let r = push(&t, "sluggy", GREET).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["kind"], "http");
    let r = push(&t, "sluggy", MCP_FN).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["kind"], "mcp");
    let r = t.admin_get("/api/functions/sluggy").await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["kind"], "mcp");
    assert_eq!(v["tools"], json!(["slugify"]));
}

#[tokio::test]
async fn redeploying_an_mcp_function_as_http_switches_back() {
    let t = boot().await;
    let r = push(&t, "sluggy", MCP_FN).await;
    assert_eq!(r.status(), 200);
    let r = push(&t, "sluggy", GREET).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["kind"], "http");
    let r = t.admin_get("/api/functions/sluggy").await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["kind"], "http");
    assert!(v.get("tools").is_none(), "http detail must not carry tools");
    // Since `public: true` became an http declaration too, the detail names
    // the flag for every kind — a plain handler is simply not public.
    assert_eq!(v["public"], json!(false));
}

#[tokio::test]
async fn invoking_an_mcp_function_as_http_is_refused() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    // /api/invoke runs a module as http; an mcp module has no request handler,
    // so the mismatch is refused up front rather than surfacing as a script error.
    let r = t
        .admin_post("/api/invoke", json!({ "name": "sluggy" }))
        .await;
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "kind_mismatch", "{v}");
    assert!(
        v["error"]["url"].as_str().unwrap().ends_with("/f/sluggy"),
        "the refusal should point at the mcp endpoint: {v}"
    );
}

// --- Serving deployed MCP functions over Streamable HTTP ---------------------

/// One JSON-RPC message to a deployed mcp function, as its owner.
async fn mcp_fn(t: &TestServer, name: &str, msg: Value) -> (u16, Value) {
    let r = t
        .client
        .post(t.data(&format!("/f/{name}")))
        .bearer_auth(&t.key)
        .json(&msg)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(json!(null));
    (status, body)
}

#[tokio::test]
async fn an_mcp_function_speaks_the_protocol() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    // initialize: served from metadata, serverInfo.version is the revision
    let (s, v) = mcp_fn(
        &t,
        "sluggy",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["result"]["serverInfo"]["name"], "sluggy", "{v}");
    assert_eq!(v["result"]["serverInfo"]["version"], "rev-1", "{v}");
    assert_eq!(
        v["result"]["capabilities"]["tools"]["listChanged"],
        json!(false),
        "{v}"
    );
    // tools/list
    let (_, v) = mcp_fn(
        &t,
        "sluggy",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    assert_eq!(v["result"]["tools"][0]["name"], "slugify", "{v}");
    assert_eq!(
        v["result"]["tools"][0]["inputSchema"]["type"], "object",
        "{v}"
    );
    // tools/call happy path
    let (_, v) = mcp_fn(
        &t,
        "sluggy",
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"slugify","arguments":{"text":"Hello World"}}}),
    )
    .await;
    assert_eq!(v["result"]["content"][0]["text"], "hello-world", "{v}");
    assert_ne!(v["result"]["isError"], json!(true), "{v}");
}

#[tokio::test]
async fn bad_arguments_are_a_tool_result_not_an_invocation() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    let (s, v) = mcp_fn(
        &t,
        "sluggy",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"slugify","arguments":{"text": 42}}}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["result"]["isError"], json!(true), "{v}");
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("text"),
        "should name the violating property: {text}"
    );
}

#[tokio::test]
async fn unknown_tools_and_methods_fail_correctly() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    // Unknown tool: a result the model can read, never a protocol error.
    let (s, v) = mcp_fn(
        &t,
        "sluggy",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"nope","arguments":{}}}),
    )
    .await;
    assert_eq!(s, 200);
    assert!(v["error"].is_null(), "must not be a protocol error: {v}");
    assert_eq!(v["result"]["isError"], json!(true), "{v}");
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"),
        "{v}"
    );
    // Unknown method: the JSON-RPC method-not-found error.
    let (s, v) = mcp_fn(
        &t,
        "sluggy",
        json!({"jsonrpc":"2.0","id":2,"method":"resources/list"}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["error"]["code"], json!(-32601), "{v}");
}

#[tokio::test]
async fn an_mcp_function_requires_an_owner_key() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
    // no key → 401 with WWW-Authenticate
    let r = t
        .client
        .post(t.data("/f/sluggy"))
        .json(&init)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(r.headers().contains_key("www-authenticate"));
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "unauthorized", "{v}");
    // a different user's key → the same 401, revealing nothing
    let other_id = rusted_server::testsupport::seed_user(&t.pool).await;
    let (_, other_key) = rusted_server::auth::create_key(&t.pool, other_id, "other")
        .await
        .unwrap();
    let r = t
        .client
        .post(t.data("/f/sluggy"))
        .bearer_auth(&other_key)
        .json(&init)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(r.headers().contains_key("www-authenticate"));
    // the owner's key works
    let (s, v) = mcp_fn(&t, "sluggy", init).await;
    assert_eq!(s, 200);
    assert_eq!(v["result"]["serverInfo"]["name"], "sluggy", "{v}");
}

#[tokio::test]
async fn a_public_mcp_function_needs_no_key() {
    let t = boot().await;
    let public_fn = r#"
export const mcp = {
  name: "open-sluggy",
  public: true,
  tools: {
    slugify: {
      description: "Turn a title into a URL slug",
      inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
      async handler({ text }) { return text.toLowerCase().replace(/[^a-z0-9]+/g, "-"); },
    },
  },
};
"#;
    push(&t, "open-sluggy", public_fn).await;
    let r = t
        .client
        .post(t.data("/f/open-sluggy"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["result"]["serverInfo"]["name"], "open-sluggy", "{v}");
}

#[tokio::test]
async fn only_tools_call_spends_the_rate_limit() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    downgrade_to_dev(&t).await; // rate 60/min
    for _ in 0..70 {
        // more list calls than the rate allows — all succeed
        let (s, v) = mcp_fn(
            &t,
            "sluggy",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert!(v["result"]["tools"].is_array(), "{v}");
    }
    // tools/call does spend it: within 70 calls one comes back rate-limited,
    // as a tool result the model can act on.
    let mut limited = None;
    for _ in 0..70 {
        let (s, v) = mcp_fn(
            &t,
            "sluggy",
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"slugify","arguments":{"text":"hi"}}}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        if v["result"]["isError"] == json!(true) {
            limited = Some(v);
            break;
        }
    }
    let v = limited.expect("rate limit should trip within 70 calls");
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("rate limit"),
        "{v}"
    );
}

#[tokio::test]
async fn a_notification_gets_202() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    let r = t
        .client
        .post(t.data("/f/sluggy"))
        .bearer_auth(&t.key)
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    assert!(r.text().await.unwrap().is_empty());
}

#[tokio::test]
async fn mcp_auth_rejects_malformed_bearer_credentials() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
    // Every near-miss on the credential is the same 401: wrong scheme case,
    // bare key, empty token, padded token, garbage.
    let bad = [
        format!("bearer {}", t.key), // scheme is matched strictly
        t.key.clone(),               // no scheme at all
        "Bearer".to_string(),
        "Bearer ".to_string(),
        // (no trailing-space case: HTTP strips trailing OWS from header
        // values before the handler sees them, so it's the same credential)
        format!("Bearer  {}", t.key), // double space
        "Bearer rk_live_".to_string(),
        "Basic dXNlcjpwYXNz".to_string(),
    ];
    for header in bad {
        let r = t
            .client
            .post(t.data("/f/sluggy"))
            .header("authorization", &header)
            .json(&init)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "header {header:?} must not authenticate");
    }
}

#[tokio::test]
async fn mcp_sub_paths_and_wrong_methods_are_refused() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    // A sub-path is 404 even with the owner's key — the function has one URL.
    let r = t
        .client
        .post(t.data("/f/sluggy/extra"))
        .bearer_auth(&t.key)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    // Non-POST is 405.
    let r = t
        .client
        .get(t.data("/f/sluggy"))
        .bearer_auth(&t.key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 405);
}

#[tokio::test]
async fn non_object_and_missing_arguments_are_schema_violations() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    // arguments: 42 — validated against the object schema, refused as a tool
    // result before any sandbox boots.
    let (s, v) = mcp_fn(
        &t,
        "sluggy",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"slugify","arguments":42}}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["result"]["isError"], json!(true), "{v}");
    assert!(
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid arguments"),
        "{v}"
    );
    // arguments absent — validates {} against the schema, which requires text.
    let (s, v) = mcp_fn(
        &t,
        "sluggy",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"slugify"}}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["result"]["isError"], json!(true), "{v}");
}

#[tokio::test]
async fn a_hostile_schema_pattern_is_a_tool_error_not_a_500() {
    let t = boot().await;
    // A classic catastrophic-backtracking pattern. Deploy accepts it (it
    // compiles); at call time the match must come back as a schema violation
    // (or backtrack-limit error) inside a tool result — never a 500 or a hang.
    let hostile = r#"
export const mcp = {
  name: "hostile",
  tools: {
    check: {
      description: "pattern gate",
      inputSchema: { type: "object", properties: {
        text: { type: "string", pattern: "^(a+)+$" } }, required: ["text"] },
      async handler({ text }) { return "ok:" + text.length; },
    },
  },
};
"#;
    push(&t, "hostile", hostile).await;
    let payload = format!("{}!", "a".repeat(40));
    let started = std::time::Instant::now();
    let (s, v) = mcp_fn(
        &t,
        "hostile",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"check","arguments":{"text": payload}}}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["result"]["isError"], json!(true), "{v}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "pattern matching must be bounded, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_batch_gets_a_batch_reply() {
    let t = boot().await;
    push(&t, "sluggy", MCP_FN).await;
    let (s, v) = mcp_fn(
        &t,
        "sluggy",
        json!([
            {"jsonrpc":"2.0","id":1,"method":"ping"},
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","id":2,"method":"tools/list"}
        ]),
    )
    .await;
    assert_eq!(s, 200);
    let replies = v.as_array().expect("batch reply is an array");
    assert_eq!(replies.len(), 2, "notification gets no reply: {v}");
    assert_eq!(replies[0]["id"], json!(1), "{v}");
    assert_eq!(replies[1]["id"], json!(2), "{v}");
}

#[tokio::test]
async fn a_json_tool_result_carries_structured_content() {
    let t = boot().await;
    let objecty = r#"
export const mcp = {
  name: "objecty",
  tools: {
    stats: {
      description: "returns an object",
      inputSchema: { type: "object" },
      async handler() { return { count: 3, ok: true }; },
    },
  },
};
"#;
    push(&t, "objecty", objecty).await;
    let (s, v) = mcp_fn(
        &t,
        "objecty",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"stats","arguments":{}}}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["result"]["structuredContent"]["count"], json!(3), "{v}");
    // The text content carries the same JSON serialized, per spec.
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["count"], json!(3), "{v}");
}

/// The shipped example deploys and behaves exactly like the hand-rolled http
/// server it replaced: same two tools, same slugify output, word_count's
/// counts now doubling as structured content.
#[tokio::test]
async fn deploying_the_example_mcp_server_works() {
    let t = boot().await;
    let example = include_str!("../../../examples/mcp-server/index.js");
    let r = t
        .admin_post("/api/functions", json!({ "source": example }))
        .await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["name"], "mcp");
    assert_eq!(v["kind"], "mcp");

    let (s, v) = mcp_fn(
        &t,
        "mcp",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["result"]["serverInfo"]["name"], "mcp", "{v}");

    let (_, v) = mcp_fn(
        &t,
        "mcp",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let mut names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["slugify", "word_count"], "{v}");

    let (_, v) = mcp_fn(
        &t,
        "mcp",
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"slugify","arguments":{"text":"Hello World"}}}),
    )
    .await;
    assert_eq!(v["result"]["content"][0]["text"], "hello-world", "{v}");
    assert_ne!(v["result"]["isError"], json!(true), "{v}");

    // The old server returned the counts as JSON text; the mcp surface sends
    // the same object, mirrored into structuredContent.
    let (_, v) = mcp_fn(
        &t,
        "mcp",
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
        "params":{"name":"word_count","arguments":{"text":"one two\nthree"}}}),
    )
    .await;
    assert_ne!(v["result"]["isError"], json!(true), "{v}");
    assert_eq!(
        v["result"]["structuredContent"],
        json!({"words": 3, "characters": 13, "lines": 2}),
        "{v}"
    );
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["words"], json!(3), "{v}");
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
    assert_eq!(v["kind"], "http");
    assert_eq!(v["config"]["name"], "cfg-fn");

    // A typo in the http export fails verify with a pointed message.
    let r = t.admin_post("/api/verify", json!({ "source": "export const http = { metods: [\"GET\"] };\nexport default async function handler() {}" })).await;
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
    // A ~1500ms busy loop: fits Extra's 30s budget, exceeds Dev's 1s.
    let slow = r#"export default async function handler(request, context) {
        const until = Date.now() + 1500;
        while (Date.now() < until) {}
        return context.text("done");
    }"#;
    push(&t, "slow", slow).await;
    let r = t.client.post(t.data("/f/slow")).send().await.unwrap();
    assert_eq!(r.status(), 200, "Extra's 30s budget should allow 1500ms");

    downgrade_to_dev(&t).await;
    let r = t.client.post(t.data("/f/slow")).send().await.unwrap();
    assert_eq!(r.status(), 429, "Dev's 1s budget should terminate it");
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
    enable_secret_store();
    let t = boot().await;
    push(&t, "greet", GREET).await;
    // A key, a secret, and an invocation so the keys/secrets/lambda pages have
    // real rows to render.
    rusted_server::auth::create_key(&t.pool, t.user_id, "smoke")
        .await
        .unwrap();
    rusted_server::secrets::SecretStore::new(t.pool.clone())
        .set(t.user_id, "prod", "SMOKE_SECRET", "v")
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
        "/console/secrets",
        "/console/billing",
        "/console/checkout/pro",
        "/console/function/greet",
        "/console/function/does-not-exist",
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

/// The lambda page for an mcp function swaps the http affordances for the
/// tools table, the client-config block, and the tools/call test template.
#[tokio::test]
async fn console_lambda_page_renders_mcp_tools() {
    let t = boot().await;
    let r = t
        .admin_post("/api/functions", json!({ "source": MCP_FN }))
        .await;
    assert_eq!(r.status(), 200);

    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let r = t
        .client
        .get(format!(
            "http://{}/console/function/sluggy",
            t.handle.admin_addr
        ))
        .header("cookie", format!("rusted_session={session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("slugify"), "tool name should render");
    assert!(
        body.contains("Turn a title into a URL slug"),
        "tool description should render"
    );
    assert!(body.contains("mcpServers"), "client config should render");
    assert!(
        body.contains("Authorization"),
        "a non-public function's config should mention the key header"
    );

    // The tester posts the tools/call envelope through the console proxy; the
    // token field must carry the key a non-public mcp function requires.
    let r = t
        .client
        .post(format!("http://{}/console/test", t.handle.admin_addr))
        .header("cookie", format!("rusted_session={session}"))
        .form(&[
            ("method", "POST".to_string()),
            ("url", t.data("/f/sluggy")),
            ("token", t.key.clone()),
            (
                "body",
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slugify","arguments":{"text":"Hello World"}}}"#
                    .to_string(),
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let result = r.text().await.unwrap();
    assert!(
        result.contains("hello-world"),
        "the tool should run through the tester: {result}"
    );
}

/// The missing-lambda page echoes the requested name, which is raw URL input
/// rendered through `inner|safe` — it must arrive HTML-escaped.
#[tokio::test]
async fn missing_lambda_page_escapes_the_requested_name() {
    let t = boot().await;
    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let r = t
        .client
        .get(format!(
            "http://{}/console/function/%3Cimg%20src%3Dx%20onerror%3Dalert(1)%3E",
            t.handle.admin_addr
        ))
        .header("cookie", format!("rusted_session={session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(
        !body.contains("<img src=x"),
        "the URL's name must not land in the page as markup"
    );
    assert!(
        body.contains("&lt;img src=x onerror=alert(1)&gt;"),
        "the name should render escaped"
    );
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
    // An API call measured 126-306ms from the server, so a budget that cannot
    // fit one makes the outbound allowance above meaningless.
    assert!(
        plan.limits.exec_ms >= 1000,
        "Dev must fit a real API call, got {}ms",
        plan.limits.exec_ms
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

/// The checkout route takes a plan code from the URL. Only the plans actually
/// on offer may be selected that way — an internal plan must not be reachable
/// by guessing its name, or any signed-in user could grant themselves one.
#[tokio::test]
async fn checkout_refuses_plans_that_are_not_on_offer() {
    let t = boot().await;
    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let cookie = format!("rusted_session={session}");

    let cache = rusted_server::plans::PlanCache::default();
    let before = rusted_server::plans::effective_plan(&t.pool, &cache, Some(t.user_id)).await;

    let r = t
        .client
        .post(format!(
            "http://{}/console/checkout/unlimited",
            t.handle.admin_addr
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success() || r.status().is_redirection(),
        "unexpected status {}",
        r.status()
    );

    cache.clear();
    let after = rusted_server::plans::effective_plan(&t.pool, &cache, Some(t.user_id)).await;
    assert_eq!(
        before.code, after.code,
        "subscribed to '{}' by naming it in the URL",
        after.code
    );
    assert_ne!(after.code, "unlimited");
}

/// The router's body limit must sit above the largest plan's script cap, or an
/// oversized push is refused by the transport with "Payload Too Large" instead
/// of by the plan check, which names the limit and how to raise it.
#[tokio::test]
async fn the_plan_refuses_oversized_scripts_not_the_router() {
    let t = boot().await;

    // Past the old 512KB transport ceiling, but inside Extra's 5MB: must deploy.
    let ok_source = format!(
        "const PAD = \"{}\";\nexport default async function handler(request, context) {{ return context.json({{ n: PAD.length }}); }}",
        "x".repeat(700_000)
    );
    let r = t
        .client
        .post(t.admin("/api/functions"))
        .header("authorization", format!("Bearer {}", t.key))
        .json(&serde_json::json!({ "source": ok_source, "name": "big-but-allowed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "700KB is within the plan and must not be refused by the router"
    );

    // Past the plan too: the refusal must come from the plan check, which says
    // which plan allows what, rather than a bare 413 from the transport.
    let too_big = format!(
        "const PAD = \"{}\";\nexport default async function handler(request, context) {{ return context.json({{ n: PAD.length }}); }}",
        "x".repeat(6_000_000)
    );
    let r = t
        .client
        .post(t.admin("/api/functions"))
        .header("authorization", format!("Bearer {}", t.key))
        .json(&serde_json::json!({ "source": too_big, "name": "too-big" }))
        .send()
        .await
        .unwrap();
    let status = r.status();
    assert_ne!(
        status, 413,
        "rejected by the router, so the caller never learns which plan they need"
    );
    let body: serde_json::Value = r.json().await.unwrap_or(serde_json::json!({}));
    assert_eq!(body["error"]["code"], "plan_limit", "body: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("plan allows"),
        "should name the plan's allowance: {body}"
    );
}

// --- MCP: one tool, so a model spends context on the task, not on schemas ----

async fn mcp(t: &TestServer, msg: serde_json::Value) -> (u16, serde_json::Value) {
    let r = t
        .client
        .post(t.admin("/mcp"))
        .header("authorization", format!("Bearer {}", t.key))
        .json(&msg)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(serde_json::json!(null));
    (status, body)
}

#[tokio::test]
async fn mcp_handshake_and_tool_listing() {
    let t = boot().await;
    let (_, init) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                           "params":{"protocolVersion":"2025-06-18"}}),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
    assert!(init["result"]["serverInfo"]["name"].is_string(), "{init}");

    let (_, list) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(
        names,
        vec![
            "execute",
            "deploy",
            "inbox_create",
            "inbox_read",
            "list",
            "delete"
        ],
        "run, publish, receive, and clean up: {list}"
    );
}

#[tokio::test]
async fn mcp_execute_runs_code_and_returns_logs() {
    let t = boot().await;
    let (_, res) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"execute",
            "arguments":{
                "code":"export default async function handler(request, context) { const { xs } = await request.json(); console.log('n=', xs.length); return context.json({ sum: xs.reduce((a,b)=>a+b,0) }); }",
                "input":{"xs":[1,2,3,4]}
            }}}),
    )
    .await;
    let text = res["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("\"sum\":10"),
        "should carry the result: {res}"
    );
    assert!(text.contains("n= 4"), "should carry console output: {res}");
    assert_ne!(res["result"]["isError"], serde_json::json!(true), "{res}");
}

/// A model can only correct code it gets feedback on, so a failing script is a
/// tool result carrying the error — never a JSON-RPC protocol error.
#[tokio::test]
async fn mcp_execute_reports_broken_code_as_a_result_the_model_can_read() {
    let t = boot().await;
    let (_, res) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
            "name":"execute",
            "arguments":{"code":"export default async function handler(r, c) { return c.json({ v: notDefinedAnywhere.x }); }"}}}),
    )
    .await;
    assert!(
        res["error"].is_null(),
        "must not be a protocol error: {res}"
    );
    assert_eq!(res["result"]["isError"], serde_json::json!(true), "{res}");
    let text = res["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("notDefinedAnywhere"),
        "the model needs to see what broke: {res}"
    );
}

#[tokio::test]
async fn mcp_notifications_get_202_and_no_body() {
    let t = boot().await;
    let r = t
        .client
        .post(t.admin("/mcp"))
        .header("authorization", format!("Bearer {}", t.key))
        .json(&serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    assert!(r.text().await.unwrap().is_empty());
}

#[tokio::test]
async fn mcp_requires_a_key() {
    let t = boot().await;
    let r = t
        .client
        .post(t.admin("/mcp"))
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        401,
        "arbitrary code execution must be authenticated"
    );
}

// --- binary bodies ------------------------------------------------------------

/// `from_utf8_lossy` does not reject invalid bytes, it replaces each one with
/// U+FFFD. A PNG posted to a function arrived as mojibake with no error at all:
/// 1024 random bytes became 967 characters, 409 of them replacements. Silent
/// corruption is worse than a refusal, because nothing tells the caller.
#[tokio::test]
async fn a_body_that_is_not_utf8_is_refused_rather_than_mangled() {
    let t = boot().await;
    // Echoes what it received, so corruption would be visible in the response.
    push(
        &t,
        "echo",
        r#"export default async function handler(request, context) {
            return context.json({ length: request.body.length });
        }"#,
    )
    .await;

    // A real PNG signature: 0x89 and 0x1A are not valid UTF-8 lead bytes.
    let png: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
    ];
    let r = t
        .client
        .post(t.data("/f/echo"))
        .body(png)
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 400, "binary must not be silently accepted");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "invalid_body", "{v}");
    let msg = v["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("UTF-8"), "the caller needs to know why: {msg}");
}

/// The refusal must be precise: valid UTF-8 that happens to be multi-byte is
/// ordinary text and has to keep working byte-for-byte.
#[tokio::test]
async fn multibyte_utf8_still_round_trips_intact() {
    let t = boot().await;
    push(
        &t,
        "echo",
        r#"export default async function handler(request, context) {
            const body = await request.json();
            return context.json({ back: body.text });
        }"#,
    )
    .await;

    let text = "שלום · 你好 · emoji 🚀 · naïve café";
    let r = t
        .client
        .post(t.data("/f/echo"))
        .header("content-type", "application/json")
        .body(serde_json::json!({ "text": text }).to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v["back"].as_str().unwrap(),
        text,
        "multi-byte UTF-8 must survive unchanged"
    );
}

// --- OAuth for MCP clients ----------------------------------------------------

/// A hosted assistant cannot be handed a Bearer key: it discovers the server,
/// registers itself, and sends the user through a browser. This walks that
/// path end to end, because the pieces are only worth anything together.
#[tokio::test]
async fn an_mcp_client_can_register_authorize_and_get_a_token() {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let t = boot().await;

    // 1. Unauthenticated, /mcp must point at the metadata rather than just refuse.
    let r = t
        .client
        .post(t.admin("/mcp"))
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let challenge = r
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        challenge.contains("resource_metadata="),
        "a client has no way to discover the auth server: {challenge:?}"
    );

    // 2. Protected resource metadata names an authorization server.
    let meta: Value = t
        .client
        .get(t.admin("/.well-known/oauth-protected-resource"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let auth_server = meta["authorization_servers"][0]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!auth_server.is_empty(), "{meta}");

    // 3. Authorization server metadata advertises what is implemented.
    let as_meta: Value = t
        .client
        .get(t.admin("/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(as_meta["code_challenge_methods_supported"][0], "S256");
    assert!(as_meta["registration_endpoint"].is_string(), "{as_meta}");

    // 4. Register dynamically.
    let reg: Value = t
        .client
        .post(t.admin("/oauth/register"))
        .json(&serde_json::json!({
            "client_name": "Test Assistant",
            "redirect_uris": ["https://client.example/cb"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = reg["client_id"].as_str().expect("client_id").to_string();

    // 5. Approve, as the signed-in human would.
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let no_follow = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let approval = no_follow
        .post(t.admin("/oauth/authorize"))
        .header("cookie", format!("rusted_session={session}"))
        .form(&[
            ("decision", "approve"),
            ("client_id", &client_id),
            ("redirect_uri", "https://client.example/cb"),
            ("response_type", "code"),
            ("code_challenge", &code_challenge),
            ("code_challenge_method", "S256"),
            ("state", "xyz"),
        ])
        .send()
        .await
        .unwrap();
    assert!(
        approval.status().is_redirection(),
        "expected a redirect back to the client, got {}",
        approval.status()
    );
    let location = approval
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        location.contains("state=xyz"),
        "state must round-trip: {location}"
    );
    let code = location
        .split("code=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .expect("a code in the redirect")
        .to_string();

    // 6. Exchange it, proving possession with the PKCE verifier.
    let token: Value = t
        .client
        .post(t.admin("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", "https://client.example/cb"),
            ("client_id", &client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let access_token = token["access_token"]
        .as_str()
        .expect("access_token")
        .to_string();
    assert_eq!(token["token_type"], "Bearer", "{token}");

    // 7. The token works on /mcp — the point of all of it.
    let used: Value = t
        .client
        .post(t.admin("/mcp"))
        .header("authorization", format!("Bearer {access_token}"))
        .json(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(used["result"]["tools"][0]["name"], "execute", "{used}");

    // 8. And the code cannot be spent twice.
    let replay = t
        .client
        .post(t.admin("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", "https://client.example/cb"),
            ("client_id", &client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        replay.status(),
        400,
        "an authorization code must be single use"
    );
}

/// PKCE is the only thing standing in for a client secret here, so a token
/// request without the matching verifier must fail.
#[tokio::test]
async fn a_wrong_pkce_verifier_is_refused() {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let t = boot().await;
    let reg: Value = t
        .client
        .post(t.admin("/oauth/register"))
        .json(&serde_json::json!({ "client_name": "T", "redirect_uris": ["https://x.example/cb"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = reg["client_id"].as_str().unwrap().to_string();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(b"the-real-verifier"));
    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let no_follow = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let approval = no_follow
        .post(t.admin("/oauth/authorize"))
        .header("cookie", format!("rusted_session={session}"))
        .form(&[
            ("decision", "approve"),
            ("client_id", &client_id),
            ("redirect_uri", "https://x.example/cb"),
            ("response_type", "code"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    let location = approval.headers()["location"].to_str().unwrap().to_string();
    let code = location
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();

    let r = t
        .client
        .post(t.admin("/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", "https://x.example/cb"),
            ("client_id", &client_id),
            ("code_verifier", "not-the-real-verifier"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "a wrong verifier must not yield a token");
}

/// An unregistered redirect URI must never reach a browser: that is the open
/// redirect the spec spends a section on.
#[tokio::test]
async fn an_unregistered_redirect_uri_is_refused() {
    let t = boot().await;
    let reg: Value = t
        .client
        .post(t.admin("/oauth/register"))
        .json(&serde_json::json!({ "client_name": "T", "redirect_uris": ["https://good.example/cb"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = reg["client_id"].as_str().unwrap().to_string();
    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let r = t
        .client
        .post(t.admin("/oauth/authorize"))
        .header("cookie", format!("rusted_session={session}"))
        .form(&[
            ("decision", "approve"),
            ("client_id", &client_id),
            ("redirect_uri", "https://attacker.example/steal"),
            ("response_type", "code"),
            ("code_challenge", "irrelevant"),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "must not redirect to an unregistered URI");

    // Registration itself must refuse an unsafe target too.
    let bad = t
        .client
        .post(t.admin("/oauth/register"))
        .json(&serde_json::json!({ "redirect_uris": ["http://attacker.example/cb"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400, "plain http off loopback must be refused");
}

/// Asked to "push a script and call its URL", a model could only offer to run
/// code once and had to ask the human for a URL — because nothing exposed
/// deploying. This is that round trip: publish, get an address, call it.
#[tokio::test]
async fn mcp_deploy_returns_a_url_that_answers() {
    let t = boot().await;
    let (_, res) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"deploy",
            "arguments":{
                "name":"greeter",
                "methods":["GET","POST"],
                "code":"export default async function handler(request, context) { return context.json({ hello: \"world\" }); }"
            }}}),
    )
    .await;
    assert_ne!(res["result"]["isError"], serde_json::json!(true), "{res}");
    let payload: Value =
        serde_json::from_str(res["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let url = payload["url"].as_str().expect("a url").to_string();
    assert!(url.contains("/f/greeter"), "{payload}");

    // The URL must actually answer, with no key.
    let r = t.client.get(&url).send().await.unwrap();
    assert_eq!(r.status(), 200, "the deployed URL should answer");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["hello"], "world");

    // It shows up in the list...
    let (_, listed) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                           "params":{"name":"list","arguments":{}}}),
    )
    .await;
    let text = listed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("greeter"), "{text}");

    // ...and deleting it takes the URL away.
    let (_, deleted) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                           "params":{"name":"delete","arguments":{"name":"greeter"}}}),
    )
    .await;
    assert_ne!(
        deleted["result"]["isError"],
        serde_json::json!(true),
        "{deleted}"
    );
    let after = t.client.get(&url).send().await.unwrap();
    assert_eq!(
        after.status(),
        404,
        "a deleted function should stop answering"
    );
}

/// The platform's list tool must describe an mcp function as one — kind and
/// tool names — not pretend it answers HTTP methods it would 405.
#[tokio::test]
async fn mcp_list_describes_each_function_by_its_kind() {
    let t = boot().await;
    let (_, deployed) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"deploy",
            "arguments":{ "code": MCP_FN }}}),
    )
    .await;
    assert_ne!(
        deployed["result"]["isError"],
        serde_json::json!(true),
        "{deployed}"
    );
    push(&t, "greeter", GREET).await;

    let (_, listed) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                           "params":{"name":"list","arguments":{}}}),
    )
    .await;
    let payload: Value =
        serde_json::from_str(listed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let entry = |name: &str| -> Value {
        payload["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing from {payload}"))
            .clone()
    };

    let sluggy = entry("sluggy");
    assert_eq!(sluggy["kind"], "mcp", "{sluggy}");
    assert_eq!(sluggy["tools"], serde_json::json!(["slugify"]), "{sluggy}");
    assert!(
        sluggy.get("methods").is_none(),
        "an mcp entry has no HTTP methods: {sluggy}"
    );

    let greeter = entry("greeter");
    assert_eq!(greeter["kind"], "http", "{greeter}");
    assert!(greeter["methods"].is_array(), "{greeter}");
}

/// Deleting is scoped to what you deployed. Saying "not found" rather than
/// "forbidden" also avoids confirming that someone else's function exists.
#[tokio::test]
async fn mcp_delete_only_touches_your_own_functions() {
    let t = boot().await;
    push(&t, "someone-elses", GREET).await;
    sqlx::query("UPDATE functions SET user_id = NULL WHERE name = 'someone-elses'")
        .execute(&t.pool)
        .await
        .unwrap();
    // The cached record carries the owner, so the server has to be told —
    // through the same NOTIFY the real system uses.
    sqlx::query("SELECT pg_notify('rusted_invalidations', 'function:someone-elses')")
        .execute(&t.pool)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (_, res) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                           "params":{"name":"delete","arguments":{"name":"someone-elses"}}}),
    )
    .await;
    assert_eq!(res["result"]["isError"], serde_json::json!(true), "{res}");
    let text = res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no function named"), "{text}");
}

// --- inboxes ------------------------------------------------------------------

/// The whole point: an agent with no inbound address hands out a URL, something
/// on the internet posts to it, and the agent reads what arrived.
#[tokio::test]
async fn an_inbox_receives_from_anyone_and_is_read_by_its_owner() {
    let t = boot().await;

    let (_, created) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"inbox_create",
            "arguments":{ "name":"stripe-data", "ttl_seconds": 120 }}}),
    )
    .await;
    let info: Value =
        serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let url = info["url"].as_str().expect("a url").to_string();
    assert!(url.contains("/inbox/"), "{info}");

    // Nothing yet: alive and empty, which must be distinguishable from gone.
    let (_, empty) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"inbox_read","arguments":{"name":"stripe-data"}}}),
    )
    .await;
    assert_ne!(
        empty["result"]["isError"],
        serde_json::json!(true),
        "{empty}"
    );
    let waiting: Value =
        serde_json::from_str(empty["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(waiting["messages"].as_array().unwrap().len(), 0);
    assert!(
        waiting["note"].is_string(),
        "should say to keep polling: {waiting}"
    );

    // A stranger posts — no credentials at all.
    let anon = reqwest::Client::new();
    let posted = anon
        .post(&url)
        .json(&serde_json::json!({ "type": "payment_intent.succeeded", "amount": 4200 }))
        .send()
        .await
        .unwrap();
    assert_eq!(posted.status(), 202, "anyone holding the URL may write");

    let (_, got) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"inbox_read","arguments":{"name":"stripe-data"}}}),
    )
    .await;
    let read: Value =
        serde_json::from_str(got["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(read["messages"][0]["amount"], 4200, "{read}");

    // Not draining, so it is still there on a second read.
    let (_, again) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
            "name":"inbox_read","arguments":{"name":"stripe-data"}}}),
    )
    .await;
    let reread: Value =
        serde_json::from_str(again["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(reread["messages"].as_array().unwrap().len(), 1);
}

/// Holding the write URL must never grant reading — that is the whole reason
/// the address and the name are different things.
#[tokio::test]
async fn the_write_url_cannot_be_used_to_read() {
    let t = boot().await;
    let (_, created) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"inbox_create","arguments":{"name":"secrets","ttl_seconds":120}}}),
    )
    .await;
    let info: Value =
        serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let url = info["url"].as_str().unwrap().to_string();

    let anon = reqwest::Client::new();
    anon.post(&url)
        .json(&serde_json::json!({ "code": "an-oauth-code" }))
        .send()
        .await
        .unwrap();

    // GET on the write address is not a route at all.
    let peek = anon.get(&url).send().await.unwrap();
    assert!(
        peek.status() == 405 || peek.status() == 404,
        "the write address must not serve reads, got {}",
        peek.status()
    );

    // And the owner's read path needs the key.
    let unauthenticated = anon
        .get(t.admin("/api/inboxes/secrets"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauthenticated.status(),
        401,
        "reading must require the key"
    );
}

/// upsert keeps the latest; drain takes the message off on first read.
#[tokio::test]
async fn upsert_and_drain_behave_as_named() {
    let t = boot().await;
    let (_, created) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"inbox_create",
            "arguments":{"name":"oauth-cb","ttl_seconds":120,"store":"upsert","drain":true}}}),
    )
    .await;
    let info: Value =
        serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let url = info["url"].as_str().unwrap().to_string();

    let anon = reqwest::Client::new();
    for code in ["first", "second", "third"] {
        anon.post(&url)
            .json(&serde_json::json!({ "code": code }))
            .send()
            .await
            .unwrap();
    }

    let (_, got) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"inbox_read","arguments":{"name":"oauth-cb"}}}),
    )
    .await;
    let read: Value =
        serde_json::from_str(got["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        read["messages"].as_array().unwrap().len(),
        1,
        "upsert keeps one: {read}"
    );
    assert_eq!(read["messages"][0]["code"], "third", "and it is the latest");
    assert_eq!(read["drained"], true);

    // Drained, so it is gone — and the write URL is gone with it.
    let (_, after) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"inbox_read","arguments":{"name":"oauth-cb"}}}),
    )
    .await;
    assert_eq!(
        after["result"]["isError"],
        serde_json::json!(true),
        "{after}"
    );
    let gone = anon.post(&url).body("{}").send().await.unwrap();
    assert_eq!(gone.status(), 410, "a dead inbox must tell senders to stop");
}

/// A public write endpoint is an unbounded write primitive unless it is capped,
/// and a body that is not text must be refused rather than stored mangled.
#[tokio::test]
async fn the_public_endpoint_is_bounded() {
    let t = boot().await;
    let (_, created) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"inbox_create","arguments":{"name":"bounded","ttl_seconds":120}}}),
    )
    .await;
    let info: Value =
        serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let url = info["url"].as_str().unwrap().to_string();
    let anon = reqwest::Client::new();

    let too_big = anon
        .post(&url)
        .body("x".repeat(rusted_server::inbox::MAX_MESSAGE_BYTES + 1))
        .send()
        .await
        .unwrap();
    assert_eq!(too_big.status(), 413, "oversized messages must be refused");

    let binary = anon
        .post(&url)
        .body(vec![0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
        .send()
        .await
        .unwrap();
    assert_eq!(
        binary.status(),
        400,
        "non-UTF-8 must be refused, not mangled"
    );

    // An address nobody issued is gone, not "not found" — no oracle.
    let nowhere = anon
        .post(t.admin("/inbox/0000000000000000000000000000000000000000000000"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(nowhere.status(), 410);
}

/// Memory is a cache, not the store. A message has to be in Postgres the
/// moment it is accepted, or a restart would lose someone's OAuth code and
/// they would see a hang with no explanation.
#[tokio::test]
async fn a_received_message_is_durable_immediately() {
    let t = boot().await;
    let (_, created) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"inbox_create","arguments":{"name":"durable","ttl_seconds":120}}}),
    )
    .await;
    let info: Value =
        serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let url = info["url"].as_str().unwrap().to_string();

    reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "code": "keep-me" }))
        .send()
        .await
        .unwrap();

    // Straight to the database, bypassing the server and its memory entirely.
    let stored: String = sqlx::query_scalar(
        "SELECT m.body FROM inbox_messages m
           JOIN inboxes i ON i.id = m.inbox_id
          WHERE i.name = 'durable'",
    )
    .fetch_one(&t.pool)
    .await
    .expect("the message should be in Postgres, not only in memory");
    assert!(stored.contains("keep-me"), "{stored}");
}

/// The third read path: a deployed function reaching its owner's inbox while
/// it runs. That is what lets one URL receive a webhook and another serve the
/// result, without the agent shuttling it.
#[tokio::test]
async fn a_function_can_read_its_owners_inbox() {
    let t = boot().await;

    let (_, created) = mcp(
        &t,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"inbox_create","arguments":{"name":"from-stripe","ttl_seconds":120}}}),
    )
    .await;
    let info: Value =
        serde_json::from_str(created["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let url = info["url"].as_str().unwrap().to_string();

    reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "amount": 4200 }))
        .send()
        .await
        .unwrap();

    push(
        &t,
        "settle",
        r#"export default async function handler(request, context) {
            const messages = await context.inbox.get("from-stripe");
            const total = messages.reduce((sum, m) => sum + (m.amount ?? 0), 0);
            return context.json({ seen: messages.length, total });
        }"#,
    )
    .await;

    let r = t.client.post(t.data("/f/settle")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["seen"], 1, "{body}");
    assert_eq!(body["total"], 4200, "{body}");
}

/// Scoping comes from the stored owner, never from what the handler asks for.
/// A function must not be able to name its way into another account's inbox.
#[tokio::test]
async fn a_function_cannot_reach_another_accounts_inbox() {
    let t = boot().await;

    // An inbox belonging to somebody else entirely.
    let stranger = rusted_server::testsupport::seed_user(&t.pool).await;
    sqlx::query(
        "INSERT INTO inboxes (user_id, name, address, expires_at)
         VALUES ($1, 'private', 'someone-elses-address', now() + interval '2 minutes')",
    )
    .bind(stranger)
    .execute(&t.pool)
    .await
    .unwrap();

    push(
        &t,
        "peek",
        r#"export default async function handler(request, context) {
            try {
                const messages = await context.inbox.get("private");
                return context.json({ leaked: messages });
            } catch (e) {
                return context.json({ refused: e.message });
            }
        }"#,
    )
    .await;

    let r = t.client.post(t.data("/f/peek")).send().await.unwrap();
    let body: Value = r.json().await.unwrap();
    assert!(
        body["leaked"].is_null(),
        "a handler reached another account's inbox: {body}"
    );
    assert!(
        body["refused"].as_str().unwrap_or("").contains("private"),
        "{body}"
    );
}

// ------------------------------------------------------------------- secrets

/// A fixed key for tests: setting the same value from every test is safe even
/// though the environment is process-global.
fn enable_secret_store() {
    std::env::set_var(
        rusted_server::secrets::KEY_ENV,
        "8e5c1b6f7a2d4e9c0b3a5d7f1e8c2a4b6d9f0e1c3a5b7d9f2e4c6a8b0d1f3e5c",
    );
}

const SECRET_READER: &str = r#"export const config = { secrets: ["API_TOKEN"] };
export default async function handler(request, context) {
    return context.json({ token: context.env.API_TOKEN });
}"#;

#[tokio::test]
async fn a_function_that_requests_secrets_gets_them_decrypted() {
    enable_secret_store();
    let t = boot().await;
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    vault
        .set(t.user_id, "prod", "API_TOKEN", "tok_live_123")
        .await
        .unwrap();
    assert_eq!(push(&t, "envy", SECRET_READER).await.status(), 200);

    let r = t.client.post(t.data("/f/envy")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["token"], "tok_live_123");
}

#[tokio::test]
async fn a_missing_secret_refuses_the_invocation_before_the_handler_runs() {
    enable_secret_store();
    let t = boot().await;
    assert_eq!(push(&t, "needy", SECRET_READER).await.status(), 200);

    let r = t.client.post(t.data("/f/needy")).send().await.unwrap();
    assert_eq!(r.status(), 500);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "missing_secrets");
    // The caller learns nothing specific; the owner's records do.
    assert!(
        !body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("API_TOKEN"),
        "{body}"
    );
}

#[tokio::test]
async fn a_function_that_does_not_ask_sees_no_env_at_all() {
    enable_secret_store();
    let t = boot().await;
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    vault
        .set(t.user_id, "prod", "API_TOKEN", "tok")
        .await
        .unwrap();
    let src = r#"export default async function handler(request, context) {
        return context.json({ env: typeof context.env });
    }"#;
    assert_eq!(push(&t, "plain-env", src).await.status(), 200);

    let r = t.client.post(t.data("/f/plain-env")).send().await.unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["env"], "undefined");
}

#[tokio::test]
async fn updating_a_secret_reaches_the_next_invocation() {
    enable_secret_store();
    let t = boot().await;
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    vault
        .set(t.user_id, "prod", "API_TOKEN", "before")
        .await
        .unwrap();
    assert_eq!(push(&t, "rotating", SECRET_READER).await.status(), 200);

    let r = t.client.post(t.data("/f/rotating")).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["token"], "before");

    // The vault writes through Postgres and NOTIFYs; the server's cache must
    // drop the old value. The listener is async, so poll briefly.
    vault
        .set(t.user_id, "prod", "API_TOKEN", "after")
        .await
        .unwrap();
    let mut latest = String::new();
    for _ in 0..40 {
        let r = t.client.post(t.data("/f/rotating")).send().await.unwrap();
        latest = r.json::<Value>().await.unwrap()["token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if latest == "after" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(latest, "after");
}

#[tokio::test]
async fn secrets_never_reach_the_database_in_the_clear() {
    enable_secret_store();
    let t = boot().await;
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    vault
        .set(t.user_id, "prod", "DB_CHECK", "plaintext-canary")
        .await
        .unwrap();
    let stored: Vec<u8> = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT ciphertext FROM secrets WHERE user_id = $1 AND name = 'DB_CHECK'",
    )
    .bind(t.user_id)
    .fetch_one(&t.pool)
    .await
    .unwrap();
    let haystack = String::from_utf8_lossy(&stored);
    assert!(
        !haystack.contains("plaintext-canary"),
        "the stored blob contains the plaintext"
    );
    // And a deploy declaring a name that is set still round-trips through the
    // console-style listing without exposing the value.
    let listed = vault.list(t.user_id, "prod").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "DB_CHECK");
}

#[tokio::test]
async fn mcp_tools_get_env_the_same_way() {
    enable_secret_store();
    let t = boot().await;
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    vault
        .set(t.user_id, "prod", "SIGNING_KEY", "sk_42")
        .await
        .unwrap();
    let src = r#"export const config = { secrets: ["SIGNING_KEY"] };
export const mcp = { tools: { sign: {
    description: "d", inputSchema: { type: "object" },
    handler(args, context) { return context.env.SIGNING_KEY; } } } };"#;
    assert_eq!(push(&t, "signer", src).await.status(), 200);
    let (s, v) = mcp_fn(
        &t,
        "signer",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"sign","arguments":{}}}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["result"]["content"][0]["text"], "sk_42", "{v}");
    assert_ne!(v["result"]["isError"], json!(true), "{v}");
}

// ------------------------------------------------------- public functions

#[tokio::test]
async fn a_public_function_skips_the_bearer_gate() {
    let database_url = rusted_server::testsupport::create_test_database().await;
    let dir = tempfile::tempdir().unwrap();
    let t = boot_full(dir, 1500, database_url, true).await;
    let callback = r#"export const http = { public: true };
export default async function handler(request, context) {
    return context.json({ state: context.randomBase64Url(32) });
}"#;
    assert_eq!(push(&t, "oauth-cb", callback).await.status(), 200);
    push(&t, "greet", GREET).await;

    // The public function answers keyless callers — that's its point.
    let r = t.client.post(t.data("/f/oauth-cb")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let first = r.json::<Value>().await.unwrap()["state"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(first.len(), 43, "32 bytes of base64url");

    // Fresh entropy per invocation, not a cached value.
    let r = t.client.post(t.data("/f/oauth-cb")).send().await.unwrap();
    assert_ne!(r.json::<Value>().await.unwrap()["state"], json!(first));

    // A function that did not opt in still demands a key.
    let r = t.client.post(t.data("/f/greet")).send().await.unwrap();
    assert_eq!(r.status(), 401);

    // A missing function answers exactly like a private one.
    let r = t.client.post(t.data("/f/nope")).send().await.unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn a_public_mcp_function_passes_the_gate_too() {
    let database_url = rusted_server::testsupport::create_test_database().await;
    let dir = tempfile::tempdir().unwrap();
    let t = boot_full(dir, 1500, database_url, true).await;
    let src = r#"export const mcp = { public: true, tools: { ping: {
        description: "d", inputSchema: { type: "object" },
        handler() { return "pong"; } } } };"#;
    assert_eq!(push(&t, "open-tools", src).await.status(), 200);

    let r = t
        .client
        .post(t.data("/f/open-tools"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["result"]["tools"][0]["name"], "ping", "{v}");
}

// ------------------------------------------------------------ capabilities

const COUNTER: &str = r#"export const config = { state: true };
export default async function handler(request, context) {
    const entry = await context.state.get("hits");
    const wrote = await context.state.compareAndSet(
        "hits",
        entry?.version ?? null,
        (entry?.value ?? 0) + 1,
    );
    if (!wrote.ok) return context.json({ retry: true }, { status: 409 });
    return context.json({ hits: wrote.version });
}"#;

#[tokio::test]
async fn durable_state_counts_across_invocations_and_revisions() {
    let t = boot().await;
    assert_eq!(push(&t, "counter", COUNTER).await.status(), 200);

    for expected in 1..=2 {
        let r = t.client.post(t.data("/f/counter")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.json::<Value>().await.unwrap()["hits"], expected);
    }

    // A new revision keeps counting where the old one left off.
    assert_eq!(
        push(&t, "counter", &format!("{COUNTER}\n// rev2"))
            .await
            .status(),
        200
    );
    let r = t.client.post(t.data("/f/counter")).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["hits"], 3);

    // The explicit purge via the admin API resets it.
    let r = t
        .client
        .delete(t.admin("/api/functions/counter/state"))
        .bearer_auth(&t.key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["purged_keys"], 1);
    let r = t.client.post(t.data("/f/counter")).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["hits"], 1);
}

/// The one endpoint the object e2e tests allowlist. Set identically by every
/// test that needs it, since the environment is process-global.
fn enable_object_endpoint() {
    std::env::set_var(
        rusted_server::objects::KEY_ENV,
        "https://objects-test.example.com",
    );
}

fn binding_module(endpoint: &str) -> String {
    format!(
        r#"export const config = {{ objects: {{ BLOBS: {{
            endpoint: "{endpoint}", bucket: "test-bucket", maxObjectBytes: 1024,
            accessKeyIdSecret: "OBJ_AK", secretAccessKeySecret: "OBJ_SK",
        }} }} }};
export default async function handler(request, context) {{
    const put = await context.objects.BLOBS.presignPut("docs/one.bin", {{
        contentLength: 5,
        sha256: "a".repeat(64),
    }});
    return context.json({{ url: put.url, headers: put.headers }});
}}"#
    )
}

#[tokio::test]
async fn object_bindings_deploy_only_onto_the_allowlist() {
    let t = boot().await;
    // This endpoint is on nobody's allowlist, whatever other tests enabled.
    let r = push(
        &t,
        "sneaky",
        &binding_module("https://not-allowed.example.net"),
    )
    .await;
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "endpoint_not_allowed", "{v}");
}

#[tokio::test]
async fn object_bindings_need_credentials_and_then_presign() {
    enable_object_endpoint();
    enable_secret_store();
    let t = boot().await;
    let module = binding_module("https://objects-test.example.com");
    assert_eq!(push(&t, "blobby", &module).await.status(), 200);

    // Credentials not in the vault yet: refused before the handler runs.
    let r = t.client.post(t.data("/f/blobby")).send().await.unwrap();
    assert_eq!(r.status(), 500);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "capability_unavailable", "{v}");
    // The generic message leaks neither secret names nor endpoints.
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("OBJ_AK") && !message.contains("objects-test"),
        "{message}"
    );

    // With credentials set, the handler gets a signed URL for the exact
    // bytes — string checks only, nothing leaves the process.
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    vault
        .set(t.user_id, "prod", "OBJ_AK", "test-access-key")
        .await
        .unwrap();
    vault
        .set(t.user_id, "prod", "OBJ_SK", "test-secret-key")
        .await
        .unwrap();
    let r = t.client.post(t.data("/f/blobby")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let url = v["url"].as_str().unwrap();
    assert!(
        url.starts_with("https://objects-test.example.com/test-bucket/"),
        "{url}"
    );
    // Namespaced: the caller's key sits under an opaque 32-hex prefix.
    assert!(url.contains("/docs/one.bin?"), "{url}");
    assert!(
        !url.contains("/test-bucket/docs/"),
        "namespace missing: {url}"
    );
    assert_eq!(v["headers"]["content-length"], "5", "{v}");
    assert_eq!(v["headers"]["if-none-match"], "*", "{v}");
    // The credential id appears only as SigV4 requires; the secret never.
    assert!(!url.contains("test-secret-key"), "{url}");
}

#[tokio::test]
async fn seal_round_trips_through_a_vault_keyed_cookie() {
    enable_secret_store();
    let t = boot().await;
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    // The key secret is NOT in config.secrets: the module seals with it
    // without ever being able to read it.
    vault
        .set(t.user_id, "prod", "COOKIE_KEY", "k".repeat(64).as_str())
        .await
        .unwrap();
    let src = r#"export default async function handler(request, context) {
        const input = await request.json();
        if (input.sealed) {
            const opened = await context.open(input.sealed, { keySecret: "COOKIE_KEY", context: "test:v1" });
            const tampered = await context.open(input.sealed.slice(0, -2) + "AA", { keySecret: "COOKIE_KEY", context: "test:v1" });
            const wrongContext = await context.open(input.sealed, { keySecret: "COOKIE_KEY", context: "other" });
            return context.json({ opened, tampered, wrongContext, keyVisible: typeof context.env });
        }
        const sealed = await context.seal({ userId: 42 }, { keySecret: "COOKIE_KEY", context: "test:v1" });
        return context.json({ sealed });
    }"#;
    assert_eq!(push(&t, "sealer", src).await.status(), 200);

    let r = t
        .client
        .post(t.data("/f/sealer"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let sealed = r.json::<Value>().await.unwrap()["sealed"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !sealed.contains("42"),
        "sealed value must not leak the payload"
    );

    let r = t
        .client
        .post(t.data("/f/sealer"))
        .json(&json!({ "sealed": sealed }))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["opened"]["userId"], 42, "{v}");
    assert_eq!(v["tampered"], Value::Null, "{v}");
    assert_eq!(v["wrongContext"], Value::Null, "{v}");
    // No config.secrets declared, so the key never appeared in context.env.
    assert_eq!(v["keyVisible"], "undefined", "{v}");
}

#[tokio::test]
async fn seal_with_a_missing_key_secret_names_the_fix() {
    enable_secret_store();
    let t = boot().await;
    let src = r#"export default async function handler(request, context) {
        try {
            await context.seal({ x: 1 }, { keySecret: "NOT_SET_ANYWHERE" });
            return context.json({ sealed: true });
        } catch (e) {
            return context.json({ failed: e.message });
        }
    }"#;
    assert_eq!(push(&t, "keyless-sealer", src).await.status(), 200);
    let r = t
        .client
        .post(t.data("/f/keyless-sealer"))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert!(
        v["failed"].as_str().unwrap().contains("NOT_SET_ANYWHERE"),
        "{v}"
    );
}

#[tokio::test]
async fn logs_tell_the_truth_about_refusals_and_statuses() {
    let database_url = rusted_server::testsupport::create_test_database().await;
    let dir = tempfile::tempdir().unwrap();
    let t = boot_full(dir, 1500, database_url.clone(), false).await;
    let src = r#"export const http = { methods: ["POST"] };
export default async function handler(request, context) {
    return context.json({ error: { code: "nope" } }, { status: 403 });
}"#;
    assert_eq!(push(&t, "truthful", src).await.status(), 200);

    // A handler-chosen 403 completes fine but is not a caller-facing success.
    let r = t.client.post(t.data("/f/truthful")).send().await.unwrap();
    assert_eq!(r.status(), 403);
    // A refused method never reaches the handler — previously invisible.
    let r = t.client.delete(t.data("/f/truthful")).send().await.unwrap();
    assert_eq!(r.status(), 405);

    let v: Value = t
        .admin_get("/api/functions/truthful")
        .await
        .json()
        .await
        .unwrap();
    let recent = v["recent"].as_array().unwrap();
    assert_eq!(recent[0]["outcome"], "refused", "{v}");
    assert_eq!(recent[0]["status"], 405, "{v}");
    assert!(
        recent[0]["detail"].as_str().unwrap().contains("DELETE"),
        "{v}"
    );
    assert_eq!(recent[1]["outcome"], "success", "{v}");
    assert_eq!(recent[1]["status"], 403, "{v}");

    // A fresh process has an empty ring; its detail endpoint must fall back
    // to analytics rather than claiming there were no invocations. The
    // batcher flushes every 500ms, so poll briefly.
    let dir2 = tempfile::tempdir().unwrap();
    let t2 = boot_full(dir2, 1500, database_url, false).await;
    let mut fallback = Vec::new();
    for _ in 0..30 {
        let r = t2
            .client
            .get(format!(
                "http://{}/api/functions/truthful",
                t2.handle.admin_addr
            ))
            .bearer_auth(&t.key)
            .send()
            .await
            .unwrap();
        let v: Value = r.json().await.unwrap();
        fallback = v["recent"].as_array().cloned().unwrap_or_default();
        if fallback.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(fallback.len() >= 2, "analytics fallback never surfaced");
    let statuses: Vec<i64> = fallback
        .iter()
        .filter_map(|inv| inv["status"].as_i64())
        .collect();
    assert!(
        statuses.contains(&405) && statuses.contains(&403),
        "{statuses:?}"
    );
}

#[tokio::test]
async fn unpublished_functions_answer_like_missing_until_republished() {
    let t = boot().await;
    assert_eq!(push(&t, "toggleable", GREET).await.status(), 200);
    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let cookie = format!("rusted_session={session}");
    let admin = |path: &str| format!("http://{}{path}", t.handle.admin_addr);

    // Serving normally.
    let r = t
        .client
        .post(t.data("/f/toggleable"))
        .body(r#"{"name":"Ada"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Unpublish from the console: callers now get exactly a missing-function
    // answer, and the owner's logs record why.
    let r = t
        .client
        .post(admin("/console/function/toggleable/published"))
        .header("cookie", &cookie)
        .form(&[("published", "false")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("UNPUBLISHED"));
    let r = t.client.post(t.data("/f/toggleable")).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "not_found", "{v}");
    let detail: Value = t
        .admin_get("/api/functions/toggleable")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["published"], false, "{detail}");
    assert_eq!(detail["recent"][0]["outcome"], "refused", "{detail}");
    assert!(detail["recent"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("unpublished"));

    // A redeploy must NOT flip it back on the air.
    assert_eq!(push(&t, "toggleable", GREET).await.status(), 200);
    let r = t.client.post(t.data("/f/toggleable")).send().await.unwrap();
    assert_eq!(r.status(), 404, "push must not republish");

    // Republishing serves again.
    let r = t
        .client
        .post(admin("/console/function/toggleable/published"))
        .header("cookie", &cookie)
        .form(&[("published", "true")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = t
        .client
        .post(t.data("/f/toggleable"))
        .body(r#"{"name":"Ada"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn console_delete_removes_the_function_and_only_for_its_owner() {
    let t = boot().await;
    assert_eq!(push(&t, "condemned", GREET).await.status(), 200);
    let admin = |path: &str| format!("http://{}{path}", t.handle.admin_addr);

    // Somebody else's session cannot delete it — same answer as missing.
    let other = rusted_server::testsupport::seed_user(&t.pool).await;
    let other_session = rusted_server::auth::create_session(&t.pool, other)
        .await
        .unwrap();
    let r = t
        .client
        .delete(admin("/console/function/condemned"))
        .header("cookie", format!("rusted_session={other_session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(
        t.client
            .post(t.data("/f/condemned"))
            .body("{}")
            .send()
            .await
            .unwrap()
            .status(),
        200,
        "a non-owner delete must change nothing"
    );

    // The owner's delete works and htmx is pointed home.
    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let r = t
        .client
        .delete(admin("/console/function/condemned"))
        .header("cookie", format!("rusted_session={session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers().get("hx-redirect").and_then(|v| v.to_str().ok()),
        Some("/console")
    );
    let r = t.client.post(t.data("/f/condemned")).send().await.unwrap();
    assert_eq!(r.status(), 404);
}

// ------------------------------------------------------------- mcp oauth

/// A minimal RFC 8414 + RFC 7662 authorization server on loopback, reachable
/// only because RUSTED_OAUTH_ALLOW_HTTP relaxes the SSRF guard for tests.
/// Counts introspection calls so caching behavior is observable.
async fn mock_authorization_server(
    audience: String,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use axum::routing::{get, post};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app_issuer = issuer.clone();
    let app_hits = hits.clone();
    let app = axum::Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get({
                let issuer = app_issuer.clone();
                move || {
                    let issuer = issuer.clone();
                    async move {
                        axum::Json(json!({
                            "issuer": issuer,
                            "introspection_endpoint": format!("{issuer}/introspect"),
                        }))
                    }
                }
            }),
        )
        .route(
            "/introspect",
            post({
                let issuer = app_issuer.clone();
                let hits = app_hits.clone();
                move |headers: axum::http::HeaderMap, body: String| {
                    let issuer = issuer.clone();
                    let hits = hits.clone();
                    let audience = audience.clone();
                    async move {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // RFC 7662 §2.1: the endpoint authenticates its callers.
                        let authed = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|v| v.starts_with("Basic "));
                        if !authed {
                            return (axum::http::StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                        }
                        let token = body
                            .split('&')
                            .find_map(|pair| pair.strip_prefix("token="))
                            .unwrap_or_default();
                        if token != "good-token" {
                            return (
                                axum::http::StatusCode::OK,
                                axum::Json(json!({ "active": false })),
                            );
                        }
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(json!({
                                "active": true,
                                "iss": issuer,
                                "aud": audience,
                                "exp": rusted_server::state::now_epoch() + 600,
                                "scope": "folders:read",
                                "sub": "github:42",
                                "client_id": "renote-web",
                                "connection_id": "conn-7",
                            })),
                        )
                    }
                }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (issuer, hits)
}

#[tokio::test]
async fn mcp_oauth_flows_end_to_end_from_challenge_to_context_auth() {
    std::env::set_var("RUSTED_OAUTH_ALLOW_HTTP", "1");
    enable_secret_store();
    // require_auth on: proves oauth functions and the well-known metadata
    // both pass the key gate without a rusted API key.
    let database_url = rusted_server::testsupport::create_test_database().await;
    let dir = tempfile::tempdir().unwrap();
    let t = boot_full(dir, 1500, database_url, true).await;

    let audience = t.data("/f/notes");
    let (issuer, hits) = mock_authorization_server(audience.clone()).await;
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    vault
        .set(t.user_id, "prod", "AS_CLIENT_ID", "rusted-rp")
        .await
        .unwrap();
    vault
        .set(t.user_id, "prod", "AS_CLIENT_SECRET", "rp-secret")
        .await
        .unwrap();

    let src = format!(
        r#"export const mcp = {{
  name: "notes",
  auth: {{
    type: "oauth",
    issuer: "{issuer}",
    audience: "{audience}",
    scopes: ["folders:read"],
    introspectionClientIdSecret: "AS_CLIENT_ID",
    introspectionClientSecretSecret: "AS_CLIENT_SECRET",
  }},
  tools: {{ whoami: {{
    description: "Show the verified caller",
    inputSchema: {{ type: "object" }},
    handler(_args, context) {{ return context.auth; }},
  }} }},
}};"#
    );
    let r = push(&t, "notes", &src).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());

    // 1. No token: a 401 challenge pointing at the resource metadata.
    let r = t
        .client
        .post(t.data("/f/notes"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let www = r
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    assert!(www.contains("resource_metadata="), "{www}");
    let metadata_url = www.split('"').nth(1).unwrap().to_string();

    // 2. The advertised metadata is fetchable with no credentials at all —
    //    even though this server otherwise requires API keys.
    let r = t.client.get(&metadata_url).send().await.unwrap();
    assert_eq!(r.status(), 200, "discovery must pass the key gate");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["authorization_servers"][0], json!(issuer), "{v}");
    assert_eq!(v["resource"], json!(audience), "{v}");

    // 3. A bad token: 401, and the issuer was asked exactly once — the
    //    replay hits the negative cache.
    for _ in 0..2 {
        let r = t
            .client
            .post(t.data("/f/notes"))
            .bearer_auth("bad-token")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a replayed bad token must not re-consult the issuer"
    );

    // 4. The good token reaches the tool, which sees only sanitized identity.
    let r = t
        .client
        .post(t.data("/f/notes"))
        .bearer_auth("good-token")
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"whoami","arguments":{}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let identity: Value =
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(identity["subject"], "github:42", "{identity}");
    assert_eq!(identity["clientId"], "renote-web", "{identity}");
    assert_eq!(identity["connectionId"], "conn-7", "{identity}");
    assert_eq!(identity["scopes"], json!(["folders:read"]), "{identity}");
    assert!(identity.get("token").is_none() && !v.to_string().contains("good-token"));

    // 5. The owner's logs saw the rejected token.
    let detail: Value = t
        .admin_get("/api/functions/notes")
        .await
        .json()
        .await
        .unwrap();
    let outcomes: Vec<&str> = detail["recent"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|inv| inv["outcome"].as_str())
        .collect();
    assert!(outcomes.contains(&"refused"), "{outcomes:?}");
}

#[tokio::test]
async fn mcp_oauth_distinguishes_issuer_outage_from_bad_tokens() {
    std::env::set_var("RUSTED_OAUTH_ALLOW_HTTP", "1");
    let t = boot().await;
    // An issuer nothing listens on: consulting it fails, and that must be a
    // 503, not a challenge to re-authorize.
    let src = r#"export const mcp = {
  name: "orphaned",
  auth: { type: "oauth", issuer: "http://127.0.0.1:9", audience: "https://api.example/f/orphaned" },
  tools: { t: { description: "d", inputSchema: { type: "object" }, handler() { return 1; } } },
};"#;
    assert_eq!(push(&t, "orphaned", src).await.status(), 200);
    let r = t
        .client
        .post(t.data("/f/orphaned"))
        .bearer_auth("any-token")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "auth_unavailable", "{v}");
}

// ----------------------------------------------------------- environments

#[tokio::test]
async fn environments_overlay_secrets_state_and_report_current_env() {
    enable_secret_store();
    let t = boot().await;
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    rusted_server::secrets::create_env(&t.pool, t.user_id, "stage")
        .await
        .unwrap();
    vault
        .set(t.user_id, "prod", "APP_ORIGIN", "https://renote.app")
        .await
        .unwrap();
    vault
        .set(t.user_id, "stage", "APP_ORIGIN", "http://localhost:3000")
        .await
        .unwrap();

    let src = r#"export const config = { secrets: ["APP_ORIGIN"], state: true };
export default async function handler(request, context) {
    const entry = await context.state.get("hits");
    const wrote = await context.state.compareAndSet(
        "hits", entry?.version ?? null, (entry?.value ?? 0) + 1);
    return context.json({
        origin: context.env.APP_ORIGIN,
        currentEnv: context.currentEnv,
        hits: wrote.ok ? wrote.version : null,
    });
}"#;
    assert_eq!(push(&t, "enved", src).await.status(), 200);

    // Prod: prod secret, prod state, currentEnv "prod".
    let r = t.client.post(t.data("/f/enved")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["origin"], "https://renote.app", "{v}");
    assert_eq!(v["currentEnv"], "prod", "{v}");
    assert_eq!(v["hits"], 1, "{v}");

    // Stage: its own secret AND its own counter — state does not bleed.
    let r = t
        .client
        .post(t.data("/f/@stage/enved"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["origin"], "http://localhost:3000", "{v}");
    assert_eq!(v["currentEnv"], "stage", "{v}");
    assert_eq!(v["hits"], 1, "stage must have its own counter: {v}");

    // Prod again: unaffected by the stage write.
    let r = t.client.post(t.data("/f/enved")).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["hits"], 2);

    // An env that was never created answers exactly like a missing function.
    let r = t
        .client
        .post(t.data("/f/@ghost/enved"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["code"],
        "not_found"
    );
}

#[tokio::test]
async fn a_secret_missing_in_one_env_refuses_only_that_env() {
    enable_secret_store();
    let t = boot().await;
    let vault = rusted_server::secrets::SecretStore::new(t.pool.clone());
    rusted_server::secrets::create_env(&t.pool, t.user_id, "stage")
        .await
        .unwrap();
    // Set in prod only.
    vault
        .set(t.user_id, "prod", "ONLY_PROD", "value")
        .await
        .unwrap();
    let src = r#"export const config = { secrets: ["ONLY_PROD"] };
export default async function handler(request, context) {
    return context.json({ v: context.env.ONLY_PROD });
}"#;
    assert_eq!(push(&t, "halfset", src).await.status(), 200);

    let r = t.client.post(t.data("/f/halfset")).send().await.unwrap();
    assert_eq!(r.status(), 200, "prod is configured");

    // Stage: refused before the handler, never an empty string, never a
    // silent fallback to prod's value.
    let r = t
        .client
        .post(t.data("/f/@stage/halfset"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 500);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "missing_secrets", "{v}");

    // The owner's log names the env and the secret.
    let detail: Value = t
        .admin_get("/api/functions/halfset")
        .await
        .json()
        .await
        .unwrap();
    let entry = detail["recent"][0]["detail"].as_str().unwrap();
    assert!(
        entry.contains("[@stage]") && entry.contains("ONLY_PROD"),
        "{entry}"
    );
}

#[tokio::test]
async fn environment_lifecycle_is_bounded_and_prod_protected() {
    let t = boot().await;
    let pool = &t.pool;
    assert!(rusted_server::secrets::create_env(pool, t.user_id, "prod")
        .await
        .unwrap_err()
        .contains("always exists"));
    assert!(rusted_server::secrets::create_env(pool, t.user_id, "local")
        .await
        .unwrap_err()
        .contains("reserved"));
    assert!(
        rusted_server::secrets::create_env(pool, t.user_id, "Bad Name")
            .await
            .is_err()
    );
    assert!(rusted_server::secrets::delete_env(pool, t.user_id, "prod")
        .await
        .unwrap_err()
        .contains("cannot be deleted"));
    for i in 0..rusted_server::secrets::MAX_ENVIRONMENTS {
        rusted_server::secrets::create_env(pool, t.user_id, &format!("env-{i}"))
            .await
            .unwrap();
    }
    assert!(
        rusted_server::secrets::create_env(pool, t.user_id, "one-too-many")
            .await
            .is_err()
    );
    let envs = rusted_server::secrets::list_envs(pool, t.user_id).await;
    assert_eq!(envs[0], "prod");
    assert_eq!(
        envs.len() as i64,
        1 + rusted_server::secrets::MAX_ENVIRONMENTS
    );
}

#[tokio::test]
async fn public_functions_stay_public_through_an_env_url() {
    enable_secret_store();
    let database_url = rusted_server::testsupport::create_test_database().await;
    let dir = tempfile::tempdir().unwrap();
    let t = boot_full(dir, 1500, database_url, true).await;
    rusted_server::secrets::create_env(&t.pool, t.user_id, "stage")
        .await
        .unwrap();
    let src = r#"export const http = { public: true };
export default async function handler(request, context) {
    return context.json({ env: context.currentEnv });
}"#;
    assert_eq!(push(&t, "open-env", src).await.status(), 200);
    // Keyless through the env URL: the bearer gate must parse past @stage.
    let r = t
        .client
        .post(t.data("/f/@stage/open-env"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["env"], "stage");
}

#[tokio::test]
async fn oauth_mcp_env_urls_refuse_only_underivable_audiences() {
    std::env::set_var("RUSTED_OAUTH_ALLOW_HTTP", "1");
    let t = boot().await;
    rusted_server::secrets::create_env(&t.pool, t.user_id, "stage")
        .await
        .unwrap();
    // An audience with no /f/ segment: no env audience can be derived.
    let src = r#"export const mcp = {
  name: "envless",
  auth: { type: "oauth", issuer: "http://127.0.0.1:9", audience: "https://api.example/mcp" },
  tools: { t: { description: "d", inputSchema: { type: "object" }, handler() { return 1; } } },
};"#;
    assert_eq!(push(&t, "envless", src).await.status(), 200);
    let r = t
        .client
        .post(t.data("/f/@stage/envless"))
        .bearer_auth("any")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["code"],
        "env_unsupported"
    );
    // Its env discovery metadata is equally absent.
    let r = t
        .client
        .get(t.data("/.well-known/oauth-protected-resource/f/@stage/envless"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn console_oauth_prefers_the_platform_namespaced_variables() {
    // Process-global env, like the other env-dependent tests: set both forms
    // and require the RUSTED_CONSOLE_ one to win, so a tenant-style bare
    // GITHUB_CLIENT_ID in the environment can never masquerade as platform
    // sign-in configuration.
    std::env::set_var("GITHUB_CLIENT_ID", "bare-id");
    std::env::set_var("GITHUB_CLIENT_SECRET", "bare-secret");
    std::env::set_var("RUSTED_CONSOLE_GITHUB_CLIENT_ID", "console-id");
    std::env::set_var("RUSTED_CONSOLE_GITHUB_CLIENT_SECRET", "console-secret");
    let t = boot().await;
    let no_follow = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = no_follow
        .get(format!("http://{}/auth/github", t.handle.admin_addr))
        .send()
        .await
        .unwrap();
    // The redirect to GitHub carries whichever client id won.
    let location = r
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(location.contains("client_id=console-id"), "{location}");
    std::env::remove_var("RUSTED_CONSOLE_GITHUB_CLIENT_ID");
    std::env::remove_var("RUSTED_CONSOLE_GITHUB_CLIENT_SECRET");
    std::env::remove_var("GITHUB_CLIENT_ID");
    std::env::remove_var("GITHUB_CLIENT_SECRET");
}

#[tokio::test]
async fn oauth_discovery_metadata_rides_both_listeners() {
    std::env::set_var("RUSTED_OAUTH_ALLOW_HTTP", "1");
    let t = boot().await;
    let src = r#"export const mcp = {
  name: "discoverable",
  auth: { type: "oauth", issuer: "http://127.0.0.1:9", audience: "https://api.example/f/discoverable" },
  tools: { t: { description: "d", inputSchema: { type: "object" }, handler() { return 1; } } },
};"#;
    assert_eq!(push(&t, "discoverable", src).await.status(), 200);
    // Which side of a reverse proxy's path split /.well-known/ lands on is
    // deployment-specific — so the metadata must answer on both ports.
    for base in [t.handle.data_addr, t.handle.admin_addr] {
        let r = t
            .client
            .get(format!(
                "http://{base}/.well-known/oauth-protected-resource/f/discoverable"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{base} must serve discovery metadata");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["resource"], "https://api.example/f/discoverable", "{v}");
    }
}

/// The cross-env OAuth matrix: per-env audiences, per-env discovery, and a
/// negative cache that cannot let a wrong-env attempt poison the right env.
#[tokio::test]
async fn oauth_mcp_serves_environments_with_derived_audiences() {
    use axum::routing::{get, post};
    std::env::set_var("RUSTED_OAUTH_ALLOW_HTTP", "1");
    enable_secret_store();
    let t = boot().await;
    rusted_server::secrets::create_env(&t.pool, t.user_id, "stage")
        .await
        .unwrap();

    let prod_aud = t.data("/f/multi");
    let stage_aud = t.data("/f/@stage/multi");
    // A mock AS minting audience-specific tokens: tok-prod → prod aud,
    // tok-stage → stage aud. No client auth, keeping the focus on audiences.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let app = {
        let issuer = issuer.clone();
        let (prod_aud, stage_aud) = (prod_aud.clone(), stage_aud.clone());
        axum::Router::new()
            .route(
                "/.well-known/oauth-authorization-server",
                get({
                    let issuer = issuer.clone();
                    move || {
                        let issuer = issuer.clone();
                        async move {
                            axum::Json(json!({
                                "issuer": issuer,
                                "introspection_endpoint": format!("{issuer}/introspect"),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/introspect",
                post(move |body: String| {
                    let issuer = issuer.clone();
                    let (prod_aud, stage_aud) = (prod_aud.clone(), stage_aud.clone());
                    async move {
                        let token = body
                            .split('&')
                            .find_map(|pair| pair.strip_prefix("token="))
                            .unwrap_or_default();
                        let aud = match token {
                            "tok-prod" => prod_aud,
                            "tok-stage" => stage_aud,
                            _ => return axum::Json(json!({ "active": false })),
                        };
                        axum::Json(json!({
                            "active": true,
                            "iss": issuer,
                            "aud": aud,
                            "exp": rusted_server::state::now_epoch() + 600,
                            "scope": "folders:read",
                            "sub": "github:42",
                            "client_id": "renote-web",
                        }))
                    }
                }),
            )
    };
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let src = format!(
        r#"export const mcp = {{
  name: "multi",
  auth: {{ type: "oauth", issuer: "{issuer}", audience: "{prod_aud}", scopes: ["folders:read"] }},
  tools: {{ whoami: {{
    description: "d", inputSchema: {{ type: "object" }},
    handler(_args, context) {{ return {{ subject: context.auth.subject, env: context.currentEnv }}; }},
  }} }},
}};"#
    );
    assert_eq!(push(&t, "multi", &src).await.status(), 200);

    // Discovery: each env advertises its own resource identity.
    let r = t
        .client
        .get(t.data("/.well-known/oauth-protected-resource/f/multi"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.json::<Value>().await.unwrap()["resource"],
        json!(prod_aud)
    );
    let r = t
        .client
        .get(t.data("/.well-known/oauth-protected-resource/f/@stage/multi"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["resource"],
        json!(stage_aud)
    );
    // Unknown env: metadata as absent as the function would be.
    let r = t
        .client
        .get(t.data("/.well-known/oauth-protected-resource/f/@ghost/multi"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // The stage challenge points at the stage metadata.
    let r = t
        .client
        .post(t.data("/f/@stage/multi"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let www = r
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert!(www.contains("/f/@stage/multi"), "{www}");

    let call = |url: String, token: &'static str| {
        let client = t.client.clone();
        async move {
            client
                .post(url)
                .bearer_auth(token)
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"whoami","arguments":{}}}))
                .send()
                .await
                .unwrap()
        }
    };

    // Wrong-env first, on purpose: tok-prod against stage fails (audience
    // mismatch) — and must NOT poison the cache for the right env.
    let r = call(t.data("/f/@stage/multi"), "tok-prod").await;
    assert_eq!(r.status(), 401, "prod token must not open stage");
    let r = call(t.data("/f/multi"), "tok-prod").await;
    assert_eq!(r.status(), 200, "the same token must still open prod");
    let v: Value = r.json().await.unwrap();
    let out: Value =
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(out["env"], "prod", "{out}");

    // And the stage token opens stage, running with stage context.
    let r = call(t.data("/f/@stage/multi"), "tok-stage").await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let out: Value =
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(out["subject"], "github:42", "{out}");
    assert_eq!(out["env"], "stage", "{out}");
    // …and not prod.
    let r = call(t.data("/f/multi"), "tok-stage").await;
    assert_eq!(r.status(), 401, "stage token must not open prod");
}

#[tokio::test]
async fn function_page_shows_env_urls_deploy_time_and_redirects_old_path() {
    let t = boot().await;
    rusted_server::secrets::create_env(&t.pool, t.user_id, "stage")
        .await
        .unwrap();
    push(&t, "greet", GREET).await;
    let session = rusted_server::auth::create_session(&t.pool, t.user_id)
        .await
        .unwrap();
    let cookie = format!("rusted_session={session}");
    let admin = |path: &str| format!("http://{}{path}", t.handle.admin_addr);

    let r = t
        .client
        .get(admin("/console/function/greet"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    // One URL row per environment, prod first.
    assert!(body.contains("@prod"), "prod badge missing");
    assert!(body.contains("@stage"), "stage badge missing");
    assert!(body.contains("/f/@stage/greet"), "stage URL missing");
    // The deploy line: absolute UTC time plus a relative age.
    assert!(
        body.contains("deployed 20") && body.contains("UTC"),
        "deploy time missing"
    );
    assert!(
        body.contains("just now") || body.contains("ago"),
        "deploy age missing"
    );

    // The old spelling still lands on the page (via permanent redirect).
    let r = t
        .client
        .get(admin("/console/lambda/greet"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.url().path().ends_with("/console/function/greet"),
        "{}",
        r.url()
    );
}

// ------------------------------------------------------------- telemetry

#[tokio::test]
async fn stats_api_reports_otel_counts_p95_and_error_rate() {
    let t = boot().await;
    push(&t, "steady", GREET).await;
    push(
        &t,
        "faulty",
        r#"export default async function handler() { throw new Error("boom"); }"#,
    )
    .await;
    for _ in 0..4 {
        let r = t
            .client
            .post(t.data("/f/steady"))
            .body(r#"{"name":"Ada"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    let r = t.client.post(t.data("/f/faulty")).send().await.unwrap();
    assert_eq!(r.status(), 500);
    // A refused method counts as refused, not as handler failure.
    let r = t.client.delete(t.data("/f/steady")).send().await.unwrap();
    assert_eq!(r.status(), 405);

    // Protected: no key, no stats.
    let bare = t.client.get(t.admin("/api/stats")).send().await.unwrap();
    assert_eq!(bare.status(), 401);

    let v: Value = t.admin_get("/api/stats").await.json().await.unwrap();
    assert_eq!(v["source"], "opentelemetry", "{v}");
    let functions = v["functions"].as_array().unwrap();
    let steady = functions
        .iter()
        .find(|f| f["function"] == "steady")
        .unwrap_or_else(|| panic!("steady missing: {v}"));
    assert_eq!(steady["invocations"], 5, "{steady}");
    assert_eq!(steady["success"], 4, "{steady}");
    assert_eq!(steady["refused"], 1, "{steady}");
    assert_eq!(steady["error_rate"], 0.0, "{steady}");
    assert!(steady["p95_exec_ms"].as_f64().unwrap() > 0.0, "{steady}");
    let faulty = functions
        .iter()
        .find(|f| f["function"] == "faulty")
        .unwrap();
    assert_eq!(faulty["error"], 1, "{faulty}");
    assert_eq!(faulty["error_rate"], 1.0, "{faulty}");

    // Another account sees none of it.
    let other = rusted_server::testsupport::seed_user(&t.pool).await;
    let (_, other_key) = rusted_server::auth::create_key(&t.pool, other, "other")
        .await
        .unwrap();
    let r = t
        .client
        .get(t.admin("/api/stats"))
        .bearer_auth(&other_key)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["functions"].as_array().unwrap().len(), 0, "{v}");
}

#[tokio::test]
async fn telemetry_totals_survive_a_server_restart() {
    std::env::set_var("RUSTED_TELEMETRY_PERSIST_SECS", "1");
    let database_url = rusted_server::testsupport::create_test_database().await;
    let dir = tempfile::tempdir().unwrap();
    let t = boot_full(dir, 1500, database_url.clone(), false).await;
    push(&t, "durable-stats", GREET).await;
    for _ in 0..3 {
        t.client
            .post(t.data("/f/durable-stats"))
            .body(r#"{"name":"Ada"}"#)
            .send()
            .await
            .unwrap();
    }
    // Let the persist loop fold the totals into Postgres.
    let mut persisted = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let n: i64 = sqlx::query_scalar(
            "SELECT coalesce(sum(invocations), 0)::bigint FROM telemetry_counters
             WHERE function = 'durable-stats'",
        )
        .fetch_one(&t.pool)
        .await
        .unwrap();
        if n >= 3 {
            persisted = true;
            break;
        }
    }
    assert!(persisted, "totals never reached the snapshot table");

    // A fresh process starts from the baseline, not from zero.
    let dir2 = tempfile::tempdir().unwrap();
    let t2 = boot_full(dir2, 1500, database_url, false).await;
    let r = t2
        .client
        .get(format!("http://{}/api/stats", t2.handle.admin_addr))
        .bearer_auth(&t.key)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    let durable = v["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["function"] == "durable-stats")
        .unwrap_or_else(|| panic!("baseline missing after restart: {v}"))
        .clone();
    assert!(durable["invocations"].as_u64().unwrap() >= 3, "{durable}");
    assert!(durable["p95_exec_ms"].as_f64().unwrap() > 0.0, "{durable}");
}

#[tokio::test]
async fn docs_are_public_complete_and_unauthenticated() {
    let t = boot().await;
    let admin = |path: &str| format!("http://{}{path}", t.handle.admin_addr);

    // The index redirects to getting-started.
    let r = t.client.get(admin("/docs")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.url().path().ends_with("/docs/getting-started"));

    // Every page renders without any cookie or key, and carries its content.
    for (page, marker) in [
        ("getting-started", "rusted login"),
        ("cli", "RUSTED_ADMIN"),
        ("mcp", "tools/call"),
        ("security", "AES-256-GCM"),
        ("inbox", "write address and the read handle"),
        ("ai-agents", "write their own tools"),
    ] {
        let r = t
            .client
            .get(admin(&format!("/docs/{page}")))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "/docs/{page}");
        let body = r.text().await.unwrap();
        assert!(body.contains(marker), "/docs/{page} missing {marker:?}");
        // The sidebar links every page from every page.
        assert!(body.contains("/docs/ai-agents"), "/docs/{page} sidebar");
    }

    // An unknown page lands on getting-started rather than 404ing a typo.
    let r = t
        .client
        .get(admin("/docs/getting-strated"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.url().path().ends_with("/docs/getting-started"));

    // llms.txt links every docs page; llms-full.txt carries their content
    // as plain text with no HTML left in it.
    let r = t.client.get(admin("/llms.txt")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.starts_with("# rusted"), "{body}");
    for page in [
        "getting-started",
        "cli",
        "mcp",
        "security",
        "inbox",
        "ai-agents",
    ] {
        assert!(
            body.contains(&format!("/docs/{page}")),
            "llms.txt missing {page}"
        );
    }
    assert!(body.contains("/llms-full.txt"));

    let r = t.client.get(admin("/llms-full.txt")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"].to_str().unwrap(),
        "text/plain; charset=utf-8"
    );
    let body = r.text().await.unwrap();
    assert!(body.contains("## The sandbox"), "{}", &body[..500]);
    assert!(body.contains("rusted inbox new"));
    assert!(
        !body.contains("<p>") && !body.contains("</code>"),
        "html leaked"
    );
}

/// The whole point of the identities table: sign in with GitHub today,
/// Google tomorrow, land in the same account — but only a verified email
/// links, and a provider we've seen before never re-links.
#[tokio::test]
async fn oauth_identities_link_by_verified_email() {
    let t = boot().await;
    let github = rusted_server::auth::OauthProfile {
        provider: "github",
        subject: "9001".into(),
        login: "ada",
        name: Some("Ada"),
        avatar_url: None,
        email: Some("ada@example.com"),
    };
    let ada = rusted_server::auth::resolve_oauth_user(&t.pool, github)
        .await
        .unwrap();

    // Same verified email over Google → the same account, now dual-identity.
    let google = rusted_server::auth::OauthProfile {
        provider: "google",
        subject: "g-sub-1".into(),
        login: "ada.g",
        name: None,
        avatar_url: Some("https://lh3.example/pic"),
        email: Some("ada@example.com"),
    };
    let linked = rusted_server::auth::resolve_oauth_user(&t.pool, google)
        .await
        .unwrap();
    assert_eq!(ada, linked, "verified email must join the accounts");

    // A repeat Google sign-in resolves by identity even with no email at all.
    let returning = rusted_server::auth::OauthProfile {
        provider: "google",
        subject: "g-sub-1".into(),
        login: "ignored",
        name: None,
        avatar_url: None,
        email: None,
    };
    let again = rusted_server::auth::resolve_oauth_user(&t.pool, returning)
        .await
        .unwrap();
    assert_eq!(ada, again);

    // No verified email → no linking, even if the human is the same.
    let unverified = rusted_server::auth::OauthProfile {
        provider: "google",
        subject: "g-sub-2".into(),
        login: "stranger",
        name: None,
        avatar_url: None,
        email: None,
    };
    let stranger = rusted_server::auth::resolve_oauth_user(&t.pool, unverified)
        .await
        .unwrap();
    assert_ne!(ada, stranger, "no email must never join accounts");

    // Profile fields fill gaps and never erase: Ada's name survived Google
    // omitting it, and Google's avatar filled the gap GitHub left.
    let row = sqlx::query("SELECT name, avatar_url, email FROM users WHERE id = $1")
        .bind(ada)
        .fetch_one(&t.pool)
        .await
        .unwrap();
    use sqlx::Row;
    assert_eq!(row.get::<Option<String>, _>("name").as_deref(), Some("Ada"));
    assert_eq!(
        row.get::<Option<String>, _>("avatar_url").as_deref(),
        Some("https://lh3.example/pic")
    );
    assert_eq!(
        row.get::<Option<String>, _>("email").as_deref(),
        Some("ada@example.com")
    );
}
