// One-off experiment: can the wall deadline interrupt a blocking fetch?
use std::time::{Duration, Instant};

use rusted_engine::{Executor, HttpRequest, Limits, OutboundPolicy, QuickJsExecutor};

#[test]
#[ignore = "hits the network; run explicitly"]
fn wall_deadline_versus_a_slow_fetch() {
    // Deliberately decoupled, which today's config cannot express: a short
    // execution budget with a long outbound timeout.
    let limits = Limits {
        wall_ms: 500,
        memory_bytes: 32 * 1024 * 1024,
        max_output_bytes: 256 * 1024,
        outbound: OutboundPolicy {
            max_requests: 5,
            max_response_bytes: 256 * 1024,
            timeout: Duration::from_secs(2),
        },
    };
    let src = r#"export default async function handler(request, context) {
        // Three hanging fetches. If timeouts are per-request they accumulate,
        // and one invocation holds its slot for their sum.
        for (let i = 0; i < 3; i++) {
          try { await fetch("http://198.51.100." + (1 + i) + "/hang"); } catch (e) {}
        }
        return context.json({ done: true });
    }"#;

    let start = Instant::now();
    let r = QuickJsExecutor::new().execute(src, &HttpRequest::post_json("{}"), &limits);
    let elapsed = start.elapsed();
    eprintln!(
        "WALL_VS_FETCH wall_ms=500, three 2s fetches -> took {:?}, outcome {:?}",
        elapsed, r.outcome
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "exec_ms must bound total wall time including fetches, took {elapsed:?}"
    );
}
