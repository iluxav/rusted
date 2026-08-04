// The async executor must be indistinguishable from the blocking one except in
// how it waits. Anything that holds for only one of them is a hole.
use std::time::{Duration, Instant};

use rusted_engine::{
    Executor, HttpRequest, InvocationResult, Limits, OutboundPolicy, Outcome, QuickJsExecutor,
};

fn limits(wall_ms: u64, outbound: u32) -> Limits {
    Limits {
        wall_ms,
        memory_bytes: 32 * 1024 * 1024,
        max_output_bytes: 256 * 1024,
        outbound: OutboundPolicy {
            max_requests: outbound,
            max_response_bytes: 64 * 1024,
            timeout: Duration::from_secs(10),
        },
    }
}

/// Describes an outcome in a form that can be compared between the two paths.
fn shape(o: &Outcome) -> String {
    match o {
        Outcome::Success(s) => format!("success:{s}"),
        Outcome::Terminated(r) => format!("terminated:{}", r.split(':').next().unwrap_or("")),
        Outcome::Error(e) => format!("error:{}", e.chars().take(40).collect::<String>()),
    }
}

async fn both(src: &str, body: &str, l: &Limits) -> (String, String) {
    let ex = QuickJsExecutor::new();
    let req = HttpRequest::post_json(body.to_string());
    let sync = ex.execute(src, &req, l);
    let asy = ex.execute_async(src, &req, l).await;
    (shape(&sync.outcome), shape(&asy.outcome))
}

#[tokio::test]
async fn the_two_executors_agree() {
    let l = limits(2000, 2);
    let cases: Vec<(&str, &str, &str)> = vec![
        (
            "plain success",
            r#"export default async function handler(request, context) {
                const { name } = await request.json();
                return context.json({ hello: name });
            }"#,
            r#"{"name":"Ada"}"#,
        ),
        (
            "throws",
            r#"export default async function handler() { throw new Error("boom"); }"#,
            "{}",
        ),
        (
            "reference error",
            r#"export default async function handler(r, c) { return c.json({ v: nope.x }); }"#,
            "{}",
        ),
        ("no handler", r#"export const http = { name: "x" };"#, "{}"),
        (
            "returns a bare string",
            r#"export default async function handler() { return "plain"; }"#,
            "{}",
        ),
        (
            "console output",
            r#"export default async function handler(r, c) { console.log("hi"); return c.json({ ok: 1 }); }"#,
            "{}",
        ),
        (
            "refused framing header",
            r#"export default async function handler(r, c) {
                return c.json({}, { headers: { "content-length": "5" } });
            }"#,
            "{}",
        ),
        (
            "private address refused",
            r#"export default async function handler(r, c) {
                const x = await fetch("http://127.0.0.1:5432/");
                return c.json({ leaked: await x.text() });
            }"#,
            "{}",
        ),
    ];
    for (label, src, body) in cases {
        let (s, a) = both(src, body, &l).await;
        assert_eq!(s, a, "'{label}' differs: blocking={s} async={a}");
    }
}

#[tokio::test]
async fn containment_holds_in_the_async_executor() {
    let ex = QuickJsExecutor::new();
    let req = HttpRequest::post_json("{}".to_string());

    // Runaway loop.
    let l = limits(400, 0);
    let started = Instant::now();
    let r = ex
        .execute_async(
            "export default async function h(){ while(true){} }",
            &req,
            &l,
        )
        .await;
    assert!(
        matches!(r.outcome, Outcome::Terminated(_)),
        "infinite loop not stopped: {:?}",
        r.outcome
    );
    assert!(started.elapsed() < Duration::from_millis(2000));

    // Catastrophic regex — one operation, no loop to interrupt between.
    let started = Instant::now();
    let r = ex
        .execute_async(
            r#"export default async function h(r,c){ return c.json({ x: /^(a+)+$/.test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!") }); }"#,
            &req,
            &l,
        )
        .await;
    assert!(
        matches!(r.outcome, Outcome::Terminated(_)),
        "ReDoS not stopped: {:?}",
        r.outcome
    );
    assert!(started.elapsed() < Duration::from_millis(2000));

    // Heap.
    let r = ex
        .execute_async(
            r#"export default async function h(){ const a=[]; for(;;) a.push("x".repeat(1024*1024)); }"#,
            &req,
            &limits(5000, 0),
        )
        .await;
    assert!(
        matches!(r.outcome, Outcome::Terminated(_)),
        "heap cap not enforced: {:?}",
        r.outcome
    );

    // No host escapes.
    let r = ex
        .execute_async(
            r#"export default async function h(r,c){ return c.json({ require: typeof require, process: typeof process }); }"#,
            &req,
            &limits(2000, 0),
        )
        .await;
    match r.outcome {
        Outcome::Success(s) => {
            assert!(s.contains("\"require\":\"undefined\""), "{s}");
            assert!(s.contains("\"process\":\"undefined\""), "{s}");
        }
        o => panic!("expected success, got {o:?}"),
    }
}

/// A promise that never settles runs no bytecode, so the QuickJS interrupt
/// cannot fire; the host must enforce the wall deadline itself, and report it
/// the same way the blocking executor does.
#[tokio::test]
async fn a_never_settling_handler_is_terminated_like_the_blocking_path() {
    let l = limits(300, 0);
    let src = "export default async function h() { return new Promise(() => {}); }";
    let started = Instant::now();
    let (s, a) = both(src, "{}", &l).await;
    assert_eq!(s, a, "blocking={s} async={a}");
    assert_eq!(a, "terminated:the handler never finished");
    assert!(
        started.elapsed() < Duration::from_millis(2000),
        "took {:?}",
        started.elapsed()
    );
}

/// The reason for the whole change: awaits from separate invocations overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "hits the network; run explicitly"]
async fn concurrent_invocations_overlap_while_awaiting() {
    let src = r#"export default async function handler(request, context) {
        const r = await fetch("https://httpbingo.org/delay/1");
        return context.json({ status: r.status });
    }"#;
    let l = limits(10_000, 2);

    // Warm the connection pool so the comparison is about waiting, not TLS.
    let ex = QuickJsExecutor::new();
    ex.execute_async(src, &HttpRequest::post_json("{}".to_string()), &l)
        .await;

    let one = {
        let started = Instant::now();
        ex.execute_async(src, &HttpRequest::post_json("{}".to_string()), &l)
            .await;
        started.elapsed()
    };

    let started = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let l = l.clone();
        handles.push(tokio::spawn(async move {
            QuickJsExecutor::new()
                .execute_async(src, &HttpRequest::post_json("{}".to_string()), &l)
                .await
        }));
    }
    for h in handles {
        let r = h.await.unwrap();
        assert!(matches!(r.outcome, Outcome::Success(_)), "{:?}", r.outcome);
    }
    let eight = started.elapsed();

    eprintln!(
        "OVERLAP one={:.0}ms  eight_concurrent={:.0}ms  (serial would be ~{:.0}ms)",
        one.as_secs_f64() * 1000.0,
        eight.as_secs_f64() * 1000.0,
        one.as_secs_f64() * 8000.0
    );
    // A 1s upstream makes this unambiguous: overlapping means eight finish in
    // about the time of one, serial would be eight times that. A fast endpoint
    // cannot show this — eight concurrent requests to one get throttled
    // upstream, which is what a first version of this test actually measured.
    assert!(
        eight < one * 3,
        "eight concurrent awaits took {eight:?} against {one:?} for one — they did not overlap"
    );
}

// --- One-shot tool execution -------------------------------------------------

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

async fn run_tool(source: &str, tool: &str, args: serde_json::Value) -> InvocationResult {
    QuickJsExecutor::new()
        .execute_tool_with_services(source, tool, &args, &Limits::default(), None)
        .await
}

#[tokio::test]
async fn a_tool_returning_a_string_is_text() {
    let r = run_tool(
        MCP_MODULE,
        "slugify",
        serde_json::json!({"text": "Hello World"}),
    )
    .await;
    assert_eq!(r.outcome, Outcome::Success("hello world".into()));
    assert_eq!(r.content_type.as_deref(), Some("text/plain"));
}

#[tokio::test]
async fn a_tool_returning_a_value_is_json() {
    let src = r#"export const mcp = { tools: { count: {
        description: "d", inputSchema: { type: "object" },
        handler({ text }) { return { words: 2 }; } } } };"#;
    let r = run_tool(src, "count", serde_json::json!({"text": "a b"})).await;
    assert_eq!(r.outcome, Outcome::Success(r#"{"words":2}"#.into()));
    assert_eq!(r.content_type.as_deref(), Some("application/json"));
}

#[tokio::test]
async fn a_throwing_tool_is_an_error_with_its_message() {
    let src = r#"export const mcp = { tools: { boom: {
        description: "d", inputSchema: { type: "object" },
        handler() { throw new Error("kaput"); } } } };"#;
    let r = run_tool(src, "boom", serde_json::json!({})).await;
    assert!(matches!(&r.outcome, Outcome::Error(m) if m.contains("kaput")));
}

#[tokio::test]
async fn a_spinning_tool_is_terminated() {
    let src = r#"export const mcp = { tools: { spin: {
        description: "d", inputSchema: { type: "object" },
        handler() { while (true) {} } } } };"#;
    let r = run_tool(src, "spin", serde_json::json!({})).await;
    assert!(matches!(r.outcome, Outcome::Terminated(_)));
}

#[tokio::test]
async fn an_unknown_tool_is_an_error_naming_it() {
    let r = run_tool(MCP_MODULE, "nope", serde_json::json!({})).await;
    assert!(
        matches!(&r.outcome, Outcome::Error(m) if m.contains("unknown tool: nope")),
        "{:?}",
        r.outcome
    );
}

#[tokio::test]
async fn a_tool_returning_undefined_is_json_null() {
    let src = r#"export const mcp = { tools: { quiet: {
        description: "d", inputSchema: { type: "object" },
        handler() {} } } };"#;
    let r = run_tool(src, "quiet", serde_json::json!({})).await;
    assert_eq!(r.outcome, Outcome::Success("null".into()));
    assert_eq!(r.content_type.as_deref(), Some("application/json"));
}

#[tokio::test]
async fn tool_console_output_is_captured() {
    let src = r#"export const mcp = { tools: { noisy: {
        description: "d", inputSchema: { type: "object" },
        handler() { console.log("tool speaking"); return "done"; } } } };"#;
    let r = run_tool(src, "noisy", serde_json::json!({})).await;
    assert_eq!(r.outcome, Outcome::Success("done".into()));
    assert!(
        r.logs.iter().any(|l| l.message.contains("tool speaking")),
        "console output not captured: {:?}",
        r.logs
    );
}

#[tokio::test]
async fn an_async_tool_can_fetch() {
    // The outbound policy refuses private addresses; seeing that refusal as the
    // tool's error proves fetch is wired through the policy, not absent.
    let src = r#"export const mcp = { tools: { leak: {
        description: "d", inputSchema: { type: "object" },
        async handler() {
            const r = await fetch("http://127.0.0.1:5432/");
            return r.text();
        } } } };"#;
    let l = limits(2000, 2);
    let r = QuickJsExecutor::new()
        .execute_tool_with_services(src, "leak", &serde_json::json!({}), &l, None)
        .await;
    match &r.outcome {
        Outcome::Error(m) => assert!(m.contains("private address"), "{m}"),
        o => panic!("expected the outbound policy's refusal, got {o:?}"),
    }
}
