// The async path must enforce exactly what the blocking one does. Anything
// that only holds on one of them is a hole.
use std::time::{Duration, Instant};

use rusted_engine::outbound::{FetchRequest, OutboundBudget, OutboundPolicy};

fn policy(max: u32) -> OutboundPolicy {
    OutboundPolicy {
        max_requests: max,
        max_response_bytes: 64 * 1024,
        timeout: Duration::from_secs(10),
    }
}

fn req(url: &str) -> FetchRequest {
    FetchRequest {
        url: url.into(),
        method: None,
        headers: Default::default(),
        body: None,
    }
}

#[tokio::test]
async fn refuses_private_addresses() {
    let b = OutboundBudget::new(policy(4));
    for url in [
        "http://127.0.0.1:5432/",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.1/",
        "http://[::1]/",
    ] {
        let r = b.perform_async(req(url)).await;
        assert!(
            r.error.as_deref().unwrap_or("").contains("private")
                || r.error.as_deref().unwrap_or("").contains("resolve"),
            "{url} was not refused: {:?}",
            r.error
        );
    }
}

#[tokio::test]
async fn refuses_non_http_schemes() {
    let b = OutboundBudget::new(policy(4));
    for url in ["file:///etc/passwd", "ftp://example.com/x", "gopher://x/"] {
        let r = b.perform_async(req(url)).await;
        assert!(
            r.error.as_deref().unwrap_or("").contains("http"),
            "{url} was not refused: {:?}",
            r.error
        );
    }
}

#[tokio::test]
async fn enforces_the_per_invocation_quota() {
    let b = OutboundBudget::new(policy(2));
    for _ in 0..2 {
        b.perform_async(req("http://127.0.0.1/")).await; // refused, but counted
    }
    let r = b.perform_async(req("http://127.0.0.1/")).await;
    assert!(
        r.error.as_deref().unwrap_or("").contains("limit reached"),
        "quota not enforced: {:?}",
        r.error
    );
}

#[tokio::test]
async fn a_fetch_after_the_deadline_is_refused_without_connecting() {
    let past = Instant::now() - Duration::from_millis(10);
    let b = OutboundBudget::with_deadline(policy(4), past);
    let started = Instant::now();
    let r = b.perform_async(req("http://198.51.100.1/never")).await;
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "it connected"
    );
    assert!(
        r.error.as_deref().unwrap_or("").contains("deadline"),
        "{:?}",
        r.error
    );
}

#[tokio::test]
async fn fetch_disabled_when_the_plan_allows_none() {
    let b = OutboundBudget::new(policy(0));
    let r = b.perform_async(req("https://example.com")).await;
    assert!(
        r.error.as_deref().unwrap_or("").contains("not available"),
        "{:?}",
        r.error
    );
}

#[tokio::test]
#[ignore = "hits the network; run explicitly"]
async fn a_real_request_succeeds_and_binary_is_reported() {
    let b = OutboundBudget::new(policy(6));
    let ok = b
        .perform_async(req("https://www.google.com/generate_204"))
        .await;
    assert_eq!(ok.status, 204, "{:?}", ok.error);
    assert!(ok.error.is_none());

    let binary = b
        .perform_async(req("https://www.google.com/favicon.ico"))
        .await;
    assert!(
        binary
            .error
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("utf-8"),
        "binary should be reported, got {:?}",
        binary.error
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits the network; run explicitly"]
async fn concurrent_fetches_overlap() {
    let url = "https://www.google.com/generate_204";
    // Warm the pool and DNS.
    OutboundBudget::new(policy(2)).perform_async(req(url)).await;

    let started = Instant::now();
    OutboundBudget::new(policy(2)).perform_async(req(url)).await;
    let one = started.elapsed();

    let started = Instant::now();
    let mut hs = Vec::new();
    for _ in 0..8 {
        hs.push(tokio::spawn(async move {
            OutboundBudget::new(policy(2)).perform_async(req(url)).await
        }));
    }
    for h in hs {
        let r = h.await.unwrap();
        assert!(r.error.is_none(), "{:?}", r.error);
    }
    let eight = started.elapsed();
    eprintln!(
        "FETCH-OVERLAP one={:.0}ms eight={:.0}ms (serial would be ~{:.0}ms)",
        one.as_secs_f64() * 1000.0,
        eight.as_secs_f64() * 1000.0,
        one.as_secs_f64() * 8000.0
    );
}
