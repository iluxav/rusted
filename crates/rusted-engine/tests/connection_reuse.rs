// A fetch-heavy handler pays a TLS handshake per request unless the agent's
// connection pool survives between calls. On a 1-vCPU server serving 151 req/s
// of fetch-bound work, 36% of CPU was system time — handshakes and syscalls.
use std::time::{Duration, Instant};

use rusted_engine::outbound::{FetchRequest, OutboundBudget, OutboundPolicy};

fn policy(n: u32) -> OutboundPolicy {
    OutboundPolicy {
        max_requests: n,
        max_response_bytes: 64 * 1024,
        timeout: Duration::from_secs(10),
    }
}

fn get(budget: &OutboundBudget, url: &str) -> Duration {
    let t = Instant::now();
    let r = budget.perform(FetchRequest {
        url: url.into(),
        method: None,
        headers: Default::default(),
        body: None,
        body_base64: None,
    });
    assert!(r.error.is_none(), "fetch failed: {:?}", r.error);
    t.elapsed()
}

/// Pooling must not cost the deadline: a fetch starting past it is still
/// refused, and one inside it still cannot outlive the time remaining.
#[test]
#[ignore = "hits the network; run explicitly"]
fn pooling_does_not_weaken_the_deadline() {
    let url = "https://www.google.com/generate_204";
    let budget =
        OutboundBudget::with_deadline(policy(10), Instant::now() + Duration::from_millis(400));
    get(&budget, url); // inside the deadline, fine

    let expired =
        OutboundBudget::with_deadline(policy(10), Instant::now() - Duration::from_millis(1));
    let t = Instant::now();
    let r = expired.perform(FetchRequest {
        url: url.into(),
        method: None,
        headers: Default::default(),
        body: None,
        body_base64: None,
    });
    assert!(
        t.elapsed() < Duration::from_millis(50),
        "it tried to connect"
    );
    assert!(
        r.error.as_deref().unwrap_or("").contains("deadline"),
        "expected a deadline refusal, got {:?}",
        r.error
    );
}

/// The case that actually matters in production: each invocation builds its own
/// budget, so unless the pool outlives a single budget, a handler doing one
/// fetch pays a fresh TLS handshake on every request. On the 1-vCPU server that
/// showed up as 36% system time while serving fetch-bound traffic.
#[test]
#[ignore = "hits the network; run explicitly"]
fn the_pool_survives_between_invocations() {
    let url = "https://www.google.com/generate_204";

    // Warm the pool the way a first invocation would.
    let warm = OutboundBudget::new(policy(4));
    get(&warm, url);
    let within_invocation = get(&warm, url).as_secs_f64() * 1000.0;

    // A later invocation: a brand-new budget, as the server creates per call.
    let mut across = Vec::new();
    for _ in 0..4 {
        let later = OutboundBudget::new(policy(2));
        across.push(get(&later, url).as_secs_f64() * 1000.0);
    }
    let best = across.iter().cloned().fold(f64::INFINITY, f64::min);

    eprintln!(
        "CROSS within_invocation={within_invocation:.1}ms  new_budget_best={best:.1}ms  \
         (all: {})",
        across
            .iter()
            .map(|d| format!("{d:.1}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(
        best < within_invocation * 1.6,
        "a new budget should reuse the pooled connection ({best:.1}ms vs \
         {within_invocation:.1}ms within one invocation) — the pool dies with the budget"
    );
}

/// A binary response used to be swallowed: `read_to_string().unwrap_or_default()`
/// turns invalid UTF-8 into an empty string, so a handler fetching an image saw
/// `""` and had no way to tell that from a genuinely empty body.
#[test]
#[ignore = "hits the network; run explicitly"]
fn a_binary_response_body_is_available_without_text_coercion() {
    let budget = OutboundBudget::new(policy(4));
    let r = budget.perform(FetchRequest {
        url: "https://www.google.com/favicon.ico".into(),
        method: None,
        headers: Default::default(),
        body: None,
        body_base64: None,
    });
    eprintln!(
        "BINARY status={} body_len={} error={:?}",
        r.status,
        r.body.as_ref().map_or(0, String::len),
        r.error
    );
    assert!(r.error.is_none(), "binary fetch failed: {:?}", r.error);
    assert!(
        r.body.is_none(),
        "binary bytes must not be coerced into text"
    );
    assert!(
        !r.body_base64.is_empty(),
        "arrayBuffer needs the encoded bytes"
    );
}
