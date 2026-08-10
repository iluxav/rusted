use std::collections::BTreeMap;

use rusted_engine::{Executor, HttpRequest, Limits, Outcome, QuickJsExecutor, Surface};

fn exec() -> QuickJsExecutor {
    QuickJsExecutor::new()
}

fn success(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Success(s) => s,
        o => panic!("expected Success, got {o:?}"),
    }
}

#[test]
fn echo_handler_reads_json_body() {
    let src = r#"export default async function handler(request, context) {
        const input = await request.json();
        return context.json({ message: `Hello, ${input.name}` });
    }"#;
    let r = exec().execute(
        src,
        &HttpRequest::post_json(r#"{"name":"Ada"}"#),
        &Limits::default(),
    );
    assert_eq!(success(&r.outcome), r#"{"message":"Hello, Ada"}"#);
    assert!(r.wall.as_nanos() > 0);
}

#[test]
fn request_method_headers_query_are_exposed() {
    let src = r#"export default async function handler(request, context) {
        return context.json({
            m: request.method,
            h: request.headers["x-test"],
            q: request.query.id,
        });
    }"#;
    let req = HttpRequest {
        method: "PUT".into(),
        headers: BTreeMap::from([("x-test".into(), "yes".into())]),
        query: BTreeMap::from([("id".into(), "42".into())]),
        params: BTreeMap::new(),
        body: String::new(),
    };
    let r = exec().execute(src, &req, &Limits::default());
    assert_eq!(success(&r.outcome), r#"{"m":"PUT","h":"yes","q":"42"}"#);
}

#[test]
fn console_logs_are_captured_in_order() {
    let src = r#"export default async function handler() {
        console.log("a", 1);
        console.warn("w");
        console.error("e", { deep: true });
        return "done";
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert_eq!(success(&r.outcome), "done");
    let logs: Vec<(&str, &str)> = r
        .logs
        .iter()
        .map(|l| (l.level.as_str(), l.message.as_str()))
        .collect();
    assert_eq!(
        logs,
        vec![
            ("log", "a 1"),
            ("warn", "w"),
            ("error", r#"e {"deep":true}"#),
        ]
    );
}

#[test]
fn logs_survive_handler_errors() {
    let src = r#"export default async function handler() {
        console.log("before the crash");
        throw new Error("boom");
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    match &r.outcome {
        Outcome::Error(msg) => assert!(msg.contains("boom"), "lost message: {msg}"),
        o => panic!("expected Error, got {o:?}"),
    }
    assert_eq!(r.logs.len(), 1);
    assert_eq!(r.logs[0].message, "before the crash");
}

#[test]
fn invocations_are_isolated() {
    let src = r#"export default async function handler() {
        globalThis.counter = (globalThis.counter || 0) + 1;
        return String(globalThis.counter);
    }"#;
    let e = exec();
    let req = HttpRequest::post_json("{}");
    let limits = Limits::default();
    assert_eq!(success(&e.execute(src, &req, &limits).outcome), "1");
    assert_eq!(success(&e.execute(src, &req, &limits).outcome), "1");
}

#[test]
fn hostile_loop_is_terminated_by_wall_deadline() {
    let src = r#"export default async function handler() { while (true) {} }"#;
    let limits = Limits {
        wall_ms: 100,
        ..Limits::default()
    };
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &limits);
    match &r.outcome {
        Outcome::Terminated(_) => {}
        o => panic!("expected Terminated, got {o:?}"),
    }
}

#[test]
fn oversized_response_is_terminated() {
    let src = r#"export default async function handler() { return "x".repeat(300 * 1024); }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    match &r.outcome {
        Outcome::Terminated(reason) => assert!(reason.contains("output"), "reason: {reason}"),
        o => panic!("expected Terminated, got {o:?}"),
    }
}

#[test]
fn verify_accepts_valid_handler() {
    let src = r#"export default async function handler(request, context) { return "ok"; }"#;
    exec().verify(src).expect("valid source should verify");
}

#[test]
fn verify_rejects_syntax_errors() {
    let err = exec()
        .verify("export default function handler( {")
        .expect_err("syntax error should fail verify");
    assert!(!err.is_empty());
}

#[test]
fn verify_rejects_missing_default_export() {
    let err = exec()
        .verify("export const x = 1;")
        .expect_err("missing default export should fail verify");
    assert!(
        err.contains("default"),
        "error should mention default export: {err}"
    );
}

#[test]
fn top_level_console_logs_are_captured() {
    let src = r#"console.log("boot");
export default async function handler() {
    console.log("handling");
    return "ok";
}"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert_eq!(success(&r.outcome), "ok");
    let messages: Vec<&str> = r.logs.iter().map(|l| l.message.as_str()).collect();
    assert_eq!(messages, vec!["boot", "handling"]);
}

#[test]
fn verify_allows_top_level_console() {
    let src = r#"console.log("boot");
export default async function handler() { return "ok"; }"#;
    exec()
        .verify(src)
        .expect("top-level console.log must not fail verify");
}

#[test]
fn memory_exhaustion_is_terminated_not_error() {
    let src = r#"export default async function handler() {
        const a = [];
        while (true) { a.push("x".repeat(1024 * 1024)); }
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    match &r.outcome {
        Outcome::Terminated(reason) => assert!(reason.contains("memory"), "reason: {reason}"),
        o => panic!("expected Terminated, got {o:?}"),
    }
}

#[test]
fn stack_overflow_is_terminated_not_error() {
    let src = r#"function f() { return f() + 1; }
export default async function handler() { return String(f()); }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    match &r.outcome {
        Outcome::Terminated(reason) => assert!(reason.contains("stack"), "reason: {reason}"),
        o => panic!("expected Terminated, got {o:?}"),
    }
}

#[test]
fn context_text_returns_plain_text_with_explicit_content_type() {
    let src = r#"export default async function handler(request, context) {
        return context.text(123);
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert_eq!(success(&r.outcome), "123");
    assert_eq!(r.content_type.as_deref(), Some("text/plain; charset=utf-8"));
}

#[test]
fn context_json_carries_json_content_type() {
    let src = r#"export default async function handler(request, context) {
        return context.json({ a: 1 });
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert_eq!(success(&r.outcome), r#"{"a":1}"#);
    assert_eq!(r.content_type.as_deref(), Some("application/json"));
}

#[test]
fn bare_string_return_has_no_explicit_content_type() {
    let src = r#"export default async function handler() { return "plain"; }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert_eq!(success(&r.outcome), "plain");
    assert_eq!(r.content_type, None);
}

#[test]
fn pure_execution_time_is_measured_separately_from_setup() {
    let src = r#"export default async function handler() {
        let x = 0;
        for (let i = 0; i < 100000; i++) { x += i; }
        return String(x);
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert!(matches!(r.outcome, Outcome::Success(_)));
    assert!(r.exec_wall.as_nanos() > 0, "exec time not measured");
    assert!(
        r.exec_wall <= r.wall,
        "pure execution ({:?}) cannot exceed total wall ({:?})",
        r.exec_wall,
        r.wall
    );
}

#[test]
fn path_params_are_exposed_to_the_handler() {
    let src = r#"export default async function handler(request, context) {
        return context.json({ id: request.params.id, method: request.method });
    }"#;
    let req = HttpRequest {
        method: "GET".into(),
        headers: BTreeMap::new(),
        query: BTreeMap::new(),
        params: BTreeMap::from([("id".into(), "42".into())]),
        body: String::new(),
    };
    let r = exec().execute(src, &req, &Limits::default());
    assert_eq!(success(&r.outcome), r#"{"id":"42","method":"GET"}"#);
}

#[test]
fn inspect_reads_http_export() {
    let src = r#"export const http = { name: "greet", methods: ["GET", "POST"], path: "/user/greet" };
export default async function handler() { return "ok"; }"#;
    let cfg = match exec().inspect(src).expect("valid source inspects").surface {
        Surface::Http(cfg) => cfg,
        s => panic!("expected http surface, got {s:?}"),
    };
    assert_eq!(cfg.name.as_deref(), Some("greet"));
    assert_eq!(
        cfg.methods,
        Some(vec!["GET".to_string(), "POST".to_string()])
    );
    assert_eq!(cfg.path.as_deref(), Some("/user/greet"));
}

#[test]
fn inspect_without_http_export_returns_empty_config() {
    let src = r#"export default async function handler() { return "ok"; }"#;
    let cfg = exec().inspect(src).expect("valid source inspects");
    assert_eq!(
        cfg.surface,
        Surface::Http(rusted_engine::HttpConfig::default())
    );
    assert!(cfg.config.secrets.is_empty());
}

#[test]
fn inspect_rejects_http_typos_loudly() {
    let src = r#"export const http = { metods: ["GET"] };
export default async function handler() { return "ok"; }"#;
    let err = exec().inspect(src).expect_err("unknown key must fail");
    assert!(
        err.contains("metods"),
        "error should name the bad key: {err}"
    );
}

#[test]
fn inspect_still_requires_default_export() {
    let err = exec()
        .inspect("export const http = { name: \"x\" };")
        .expect_err("missing handler must fail");
    assert!(err.contains("default"), "error: {err}");
}

const MCP_MODULE: &str = r#"
export const mcp = {
  name: "my-mcp",
  tools: {
    slugify: {
      description: "Turn a title into a URL slug",
      inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
      async handler({ text }) { return text.toLowerCase(); },
    },
  },
};
"#;

#[test]
fn inspect_reads_mcp_surface() {
    let ex = QuickJsExecutor::new();
    match ex.inspect(MCP_MODULE).unwrap().surface {
        Surface::Mcp(m) => {
            assert_eq!(m.name.as_deref(), Some("my-mcp"));
            assert!(!m.public);
            let tool = &m.tools["slugify"];
            assert_eq!(tool.description, "Turn a title into a URL slug");
            assert_eq!(tool.input_schema["type"], "object");
        }
        s => panic!("expected mcp surface, got {s:?}"),
    }
}

#[test]
fn inspect_rejects_a_module_with_both_surfaces() {
    let src = format!("{MCP_MODULE}\nexport default async function h() {{}}");
    // mcp export + default handler is ambiguous
    let err = QuickJsExecutor::new().inspect(&src).unwrap_err();
    assert!(err.contains("default export"), "{err}");
}

#[test]
fn inspect_rejects_http_and_mcp_together() {
    let src = format!("export const http = {{ name: \"x\" }};\n{MCP_MODULE}");
    let err = QuickJsExecutor::new().inspect(&src).unwrap_err();
    assert!(err.contains("one surface"), "{err}");
}

#[test]
fn inspect_rejects_a_tool_without_a_handler() {
    let src = r#"export const mcp = { tools: { broken: {
        description: "no handler", inputSchema: { type: "object" } } } };"#;
    let err = QuickJsExecutor::new().inspect(src).unwrap_err();
    assert!(err.contains("broken") && err.contains("handler"), "{err}");
}

#[test]
fn inspect_rejects_an_invalid_input_schema() {
    let src = r#"export const mcp = { tools: { bad: {
        description: "d", inputSchema: { type: 42 },
        handler() {} } } };"#;
    let err = QuickJsExecutor::new().inspect(src).unwrap_err();
    assert!(err.contains("inputSchema"), "{err}");
}

#[test]
fn inspect_rejects_mcp_typos_loudly() {
    let src = r#"export const mcp = { tols: {} };"#;
    let err = QuickJsExecutor::new().inspect(src).unwrap_err();
    assert!(err.contains("tols"), "{err}");
}

#[test]
fn inspect_still_returns_http_for_plain_handlers() {
    let src = "export default async function h() { return 1; }";
    assert!(matches!(
        QuickJsExecutor::new().inspect(src).unwrap().surface,
        Surface::Http(_)
    ));
}

#[test]
fn inspect_rejects_a_module_with_no_tools() {
    let src = r#"export const mcp = { name: "empty" };"#;
    let err = QuickJsExecutor::new().inspect(src).unwrap_err();
    assert!(err.contains("tool"), "{err}");
}

#[test]
fn inspect_rejects_a_non_object_input_schema() {
    let src = r#"export const mcp = { tools: { loose: {
        description: "d", inputSchema: true,
        handler() {} } } };"#;
    let err = QuickJsExecutor::new().inspect(src).unwrap_err();
    assert!(
        err.contains("loose") && err.contains("inputSchema"),
        "{err}"
    );
}

#[test]
fn inspect_accepts_exactly_the_tool_limit() {
    let tools: Vec<String> = (0..32)
        .map(|i| {
            format!(
                r#"t{i}: {{ description: "d", inputSchema: {{ type: "object" }}, handler() {{}} }}"#
            )
        })
        .collect();
    let src = format!(
        "export const mcp = {{ tools: {{ {} }} }};",
        tools.join(", ")
    );
    match QuickJsExecutor::new().inspect(&src).unwrap().surface {
        Surface::Mcp(m) => assert_eq!(m.tools.len(), 32),
        s => panic!("expected mcp surface, got {s:?}"),
    }
}

/// A hostile `tools` getter that answers differently per read must not be able
/// to smuggle unchecked tools past inspect: the handler check and the stored
/// metadata must come from the same single read.
#[test]
fn inspect_snapshots_tools_in_one_read() {
    let src = r#"
        const checked = { good: { description: "d", inputSchema: { type: "object" }, handler() {} } };
        const ghost = { ghost: { description: "never handler-checked", inputSchema: { type: "object" } } };
        let reads = 0;
        export const mcp = { name: "shifty", get tools() { return reads++ === 0 ? checked : ghost; } };
    "#;
    match QuickJsExecutor::new().inspect(src).map(|i| i.surface) {
        // The snapshot that was handler-checked is the one stored.
        Ok(Surface::Mcp(m)) => {
            assert_eq!(m.tools.keys().collect::<Vec<_>>(), vec!["good"]);
        }
        Ok(s) => panic!("expected mcp surface, got {s:?}"),
        // Refusing the module outright is also sound.
        Err(err) => assert!(!err.is_empty()),
    }
}

#[test]
fn inspect_rejects_too_many_tools() {
    let tools: Vec<String> = (0..33)
        .map(|i| {
            format!(
                r#"t{i}: {{ description: "d", inputSchema: {{ type: "object" }}, handler() {{}} }}"#
            )
        })
        .collect();
    let src = format!(
        "export const mcp = {{ tools: {{ {} }} }};",
        tools.join(", ")
    );
    let err = QuickJsExecutor::new().inspect(&src).unwrap_err();
    assert!(err.contains("too many tools"), "{err}");
}

#[test]
fn inspect_rejects_a_bad_tool_name() {
    let src = r#"export const mcp = { tools: { "Bad Name!": {
        description: "d", inputSchema: { type: "object" },
        handler() {} } } };"#;
    let err = QuickJsExecutor::new().inspect(src).unwrap_err();
    assert!(err.contains("Bad Name!"), "{err}");
}

#[test]
fn inspect_reads_config_secrets() {
    let src = r#"export const config = { secrets: ["GITHUB_CLIENT_SECRET", "OAUTH_COOKIE_KEY_CURRENT"] };
export default async function handler() { return "ok"; }"#;
    let inspection = exec().inspect(src).expect("valid source inspects");
    assert_eq!(
        inspection.config.secrets,
        vec!["GITHUB_CLIENT_SECRET", "OAUTH_COOKIE_KEY_CURRENT"]
    );
}

#[test]
fn inspect_reads_config_on_mcp_modules_too() {
    let src = format!("export const config = {{ secrets: [\"API_KEY\"] }};\n{MCP_MODULE}");
    let inspection = QuickJsExecutor::new().inspect(&src).unwrap();
    assert_eq!(inspection.config.secrets, vec!["API_KEY"]);
    assert!(matches!(inspection.surface, Surface::Mcp(_)));
}

#[test]
fn inspect_rejects_bad_secret_declarations() {
    let handler = "export default async function handler() { return \"ok\"; }";
    for (config, expected) in [
        // Env-style names only, so the console and the module always agree.
        (r#"{ secrets: ["lowercase"] }"#, "lowercase"),
        (
            r#"{ secrets: ["9STARTS_WITH_DIGIT"] }"#,
            "9STARTS_WITH_DIGIT",
        ),
        (r#"{ secrets: ["TWIN", "TWIN"] }"#, "twice"),
        // Unknown keys fail at verify time instead of silently deploying.
        (r#"{ secrit: ["A"] }"#, "secrit"),
    ] {
        let src = format!("export const config = {config};\n{handler}");
        let err = exec().inspect(&src).expect_err("must be refused");
        assert!(err.contains(expected), "{config}: {err}");
    }
}

#[test]
fn fetch_is_unavailable_when_the_plan_allows_none() {
    let src = r#"export default async function handler(request, context) {
        try { await fetch("https://example.com"); return "reached"; }
        catch (e) { return "blocked: " + e.message; }
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    let body = success(&r.outcome);
    assert!(body.starts_with("blocked:"), "got {body}");
    assert!(body.contains("not available"), "got {body}");
}

#[test]
fn fetch_refuses_private_addresses() {
    let src = r#"export default async function handler(request, context) {
        try { await fetch("http://127.0.0.1:9/secret"); return "reached"; }
        catch (e) { return "blocked: " + e.message; }
    }"#;
    let limits = Limits {
        outbound: rusted_engine::OutboundPolicy {
            max_requests: 5,
            ..Default::default()
        },
        ..Limits::default()
    };
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &limits);
    let body = success(&r.outcome);
    assert!(body.contains("private address"), "got {body}");
}

#[test]
fn fetch_refuses_non_http_schemes() {
    let src = r#"export default async function handler(request, context) {
        try { await fetch("file:///etc/passwd"); return "reached"; }
        catch (e) { return "blocked: " + e.message; }
    }"#;
    let limits = Limits {
        outbound: rusted_engine::OutboundPolicy {
            max_requests: 5,
            ..Default::default()
        },
        ..Limits::default()
    };
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &limits);
    assert!(
        success(&r.outcome).contains("http and https"),
        "{:?}",
        r.outcome
    );
}

#[test]
fn fetch_enforces_the_per_execution_quota() {
    // Two allowed; the third is refused before any network work happens.
    let src = r#"export default async function handler(request, context) {
        const results = [];
        for (let i = 0; i < 3; i++) {
            try { await fetch("http://10.0.0.1/never"); results.push("attempted"); }
            catch (e) { results.push(e.message.includes("limit reached") ? "quota" : "guard"); }
        }
        return results.join(",");
    }"#;
    let limits = Limits {
        outbound: rusted_engine::OutboundPolicy {
            max_requests: 2,
            ..Default::default()
        },
        ..Limits::default()
    };
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &limits);
    assert_eq!(success(&r.outcome), "guard,guard,quota");
}

#[test]
fn repeated_executions_reuse_compiled_bytecode() {
    let src = r#"export const http = { name: "cached" };
export default async function handler(request, context) {
    globalThis.seen = (globalThis.seen || 0) + 1;
    return context.text("run:" + globalThis.seen);
}"#;
    let e = exec();
    let req = HttpRequest::post_json("{}");
    let limits = Limits::default();
    // Isolation survives caching: each invocation still gets a fresh context,
    // so the global counter never carries over.
    for _ in 0..3 {
        assert_eq!(success(&e.execute(src, &req, &limits).outcome), "run:1");
    }
}

#[test]
fn compile_errors_still_surface_with_the_cache_in_play() {
    let e = exec();
    let r = e.execute(
        "export default function handler( {",
        &HttpRequest::post_json("{}"),
        &Limits::default(),
    );
    match &r.outcome {
        Outcome::Error(msg) => assert!(!msg.is_empty()),
        o => panic!("expected Error, got {o:?}"),
    }
}

#[test]
fn a_timeout_after_an_await_reads_as_a_limit_not_a_crash() {
    // The await suspends the handler, so the interrupt lands inside a promise
    // job and leaves it unsettled. That used to surface as rquickjs's internal
    // "dead lock" wording under a generic function error.
    let src = r#"export default async function handler(request, context) {
        await request.json();
        const until = Date.now() + 5000;
        while (Date.now() < until) {}
        return context.text("never");
    }"#;
    let limits = Limits {
        wall_ms: 100,
        ..Limits::default()
    };
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &limits);
    match &r.outcome {
        Outcome::Terminated(reason) => {
            assert!(reason.contains("wall deadline"), "reason: {reason}");
            assert!(
                !reason.contains("dead lock"),
                "internal wording leaked: {reason}"
            );
        }
        o => panic!("expected Terminated, got {o:?}"),
    }
}

#[test]
fn a_promise_that_never_settles_reads_as_a_timeout() {
    let src = r#"export default async function handler() {
        await new Promise(() => {});
        return "never";
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    match &r.outcome {
        Outcome::Terminated(reason) => {
            assert!(reason.contains("never settles"), "reason: {reason}");
        }
        o => panic!("expected Terminated, got {o:?}"),
    }
}

#[test]
fn a_handler_can_choose_its_status_and_headers() {
    let src = r#"export default async function handler(request, context) {
        return context.json({ queued: true }, {
            status: 202,
            headers: { "mcp-session-id": "abc123", "Cache-Control": "no-store" },
        });
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert_eq!(success(&r.outcome), r#"{"queued":true}"#);
    assert_eq!(r.status, Some(202));
    // Names are normalised, so casing in the handler doesn't matter.
    assert_eq!(r.headers.get("mcp-session-id").unwrap(), "abc123");
    assert_eq!(r.headers.get("cache-control").unwrap(), "no-store");
}

#[test]
fn framing_headers_cannot_be_overridden() {
    for header in ["content-length", "Transfer-Encoding", "connection"] {
        let src = format!(
            r#"export default async function handler(request, context) {{
                return context.text("x", {{ headers: {{ "{header}": "0" }} }});
            }}"#
        );
        let r = exec().execute(&src, &HttpRequest::post_json("{}"), &Limits::default());
        match &r.outcome {
            Outcome::Error(message) => {
                assert!(message.contains("cannot be overridden"), "{message}")
            }
            o => panic!("{header} should be refused, got {o:?}"),
        }
    }
}

#[test]
fn a_header_value_cannot_smuggle_a_second_header() {
    let src = r#"export default async function handler(request, context) {
        return context.text("x", { headers: { "x-note": "ok\r\nx-injected: yes" } });
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    match &r.outcome {
        Outcome::Error(message) => assert!(message.contains("line break"), "{message}"),
        o => panic!("expected refusal, got {o:?}"),
    }
}

#[test]
fn an_impossible_status_is_reported_rather_than_ignored() {
    let src = r#"export default async function handler(request, context) {
        return context.text("x", { status: 999 });
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    match &r.outcome {
        Outcome::Error(message) => assert!(message.contains("999"), "{message}"),
        o => panic!("expected an error naming the status, got {o:?}"),
    }
}

#[test]
fn responses_without_an_init_are_unchanged() {
    let src = r#"export default async function handler(request, context) {
        return context.json({ ok: true });
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert_eq!(r.status, None, "no status means the platform's default");
    assert!(r.headers.is_empty());
}

/// Exhausting the heap with many small objects, rather than one big string,
/// leaves QuickJS unable to allocate even the Error it wants to throw — the
/// caught value comes back as `null`. Reporting that verbatim tells a
/// developer nothing about what went wrong.
#[test]
fn memory_exhaustion_by_small_objects_says_it_was_memory() {
    let src = r#"export default async function handler() {
        const a = [];
        for (let i = 0; i < 5000000; i++) a.push({ id: i, name: "n" + i, ok: i % 2 === 0 });
        return String(a.length);
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    match &r.outcome {
        Outcome::Terminated(reason) => {
            assert!(
                reason.to_lowercase().contains("memory"),
                "should name memory as the cause, got: {reason}"
            );
            assert!(
                !reason.contains("null"),
                "should not surface a bare null, got: {reason}"
            );
        }
        o => panic!("expected Terminated for an out-of-memory run, got {o:?}"),
    }
}

/// An agent sending ad-hoc code will reach for `import` out of habit. Saying
/// only that the module could not be loaded tells it nothing to do differently,
/// so it tries another package. The message has to teach the constraint.
#[test]
fn an_unresolvable_import_explains_the_constraint() {
    let src = r#"import { z } from "zod";
export default async function handler(request, context) { return context.json({}); }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    let message = match &r.outcome {
        Outcome::Error(m) => m.clone(),
        o => panic!("expected an error, got {o:?}"),
    };
    assert!(message.contains("zod"), "should name the module: {message}");
    assert!(
        message.contains("self-contained") || message.contains("inline"),
        "should say what to do instead: {message}"
    );
}

#[test]
fn random_bytes_come_from_the_host_and_never_repeat() {
    let src = r#"export default async function handler(request, context) {
        const a = context.randomBytes(32);
        const b = context.randomBytes(32);
        const s = context.randomBase64Url(32);
        return context.json({
            len: a.length,
            typed: a instanceof Uint8Array,
            distinct: a.join(",") !== b.join(","),
            b64len: s.length,
            urlSafe: /^[A-Za-z0-9_-]+$/.test(s),
        });
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert_eq!(
        success(&r.outcome),
        r#"{"len":32,"typed":true,"distinct":true,"b64len":43,"urlSafe":true}"#
    );
}

#[test]
fn random_length_is_bounded_and_integral() {
    let src = r#"export default async function handler(request, context) {
        const refused = [];
        for (const n of [0, -1, 1025, 3.5]) {
            try { context.randomBytes(n); refused.push("accepted " + n); }
            catch (e) { refused.push("refused"); }
        }
        return refused.join(",");
    }"#;
    let r = exec().execute(src, &HttpRequest::post_json("{}"), &Limits::default());
    assert_eq!(success(&r.outcome), "refused,refused,refused,refused");
}
