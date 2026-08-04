// The async executor must be indistinguishable from the blocking one except in
// how it waits. Anything that holds for only one of them is a hole.
use std::time::{Duration, Instant};

use rusted_engine::{Executor, HttpRequest, Limits, OutboundPolicy, Outcome, QuickJsExecutor};

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
        (
            "no handler",
            r#"export const http = { name: "x" };"#,
            "{}",
        ),
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
