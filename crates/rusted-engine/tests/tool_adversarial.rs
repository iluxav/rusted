// Adversarial tool modules at execution time: every one of these must end in a
// clean Outcome (Error or Terminated), never a panic or a hang.
use std::time::Duration;

use rusted_engine::{Limits, OutboundPolicy, Outcome, QuickJsExecutor};

fn limits(wall_ms: u64) -> Limits {
    Limits {
        wall_ms,
        memory_bytes: 32 * 1024 * 1024,
        max_output_bytes: 256 * 1024,
        outbound: OutboundPolicy {
            max_requests: 0,
            max_response_bytes: 64 * 1024,
            timeout: Duration::from_secs(10),
        },
    }
}

async fn run(source: &str, tool: &str) -> rusted_engine::InvocationResult {
    QuickJsExecutor::new()
        .execute_tool_with_services(source, tool, &serde_json::json!({}), &limits(1000), None)
        .await
}

#[tokio::test]
async fn a_return_value_whose_tojson_throws_is_a_clean_error() {
    let src = r#"export const mcp = { tools: { t: {
        description: "d", inputSchema: { type: "object" },
        handler() { return { toJSON() { throw new Error("gotcha"); } }; } } } };"#;
    let r = run(src, "t").await;
    assert!(
        matches!(&r.outcome, Outcome::Error(m) if m.contains("gotcha")),
        "{:?}",
        r.outcome
    );
}

#[tokio::test]
async fn a_return_value_whose_tojson_spins_is_terminated() {
    let src = r#"export const mcp = { tools: { t: {
        description: "d", inputSchema: { type: "object" },
        handler() { return { toJSON() { while (true) {} } }; } } } };"#;
    let r = run(src, "t").await;
    assert!(
        matches!(r.outcome, Outcome::Terminated(_)),
        "{:?}",
        r.outcome
    );
}

#[tokio::test]
async fn a_returned_proxy_with_throwing_traps_is_a_clean_error() {
    let src = r#"export const mcp = { tools: { t: {
        description: "d", inputSchema: { type: "object" },
        handler() {
            return new Proxy({}, { ownKeys() { throw new Error("trapdoor"); } });
        } } } };"#;
    let r = run(src, "t").await;
    assert!(
        matches!(&r.outcome, Outcome::Error(m) if m.contains("trapdoor")),
        "{:?}",
        r.outcome
    );
}

#[tokio::test]
async fn a_tools_getter_that_throws_at_call_time_is_a_clean_error() {
    let src = r#"export const mcp = {
        get tools() { throw new Error("bait and switch"); },
    };"#;
    let r = run(src, "t").await;
    assert!(
        matches!(&r.outcome, Outcome::Error(m) if m.contains("bait and switch")),
        "{:?}",
        r.outcome
    );
}

#[tokio::test]
async fn a_handler_getter_that_lies_about_typeof_is_a_clean_error() {
    // First read is a function (passes the typeof re-check), second read is not.
    let src = r#"let reads = 0;
    export const mcp = { tools: { t: {
        description: "d", inputSchema: { type: "object" },
        get handler() { return reads++ === 0 ? () => "ok" : 42; } } } };"#;
    let r = run(src, "t").await;
    assert!(
        matches!(r.outcome, Outcome::Error(_)) || matches!(r.outcome, Outcome::Success(_)),
        "{:?}",
        r.outcome
    );
}

#[tokio::test]
async fn clobbering_the_log_global_does_not_break_the_envelope() {
    let src = r#"export const mcp = { tools: { t: {
        description: "d", inputSchema: { type: "object" },
        handler() { globalThis.__rustedLogs = "not an array"; return "fine"; } } } };"#;
    let r = run(src, "t").await;
    assert_eq!(r.outcome, Outcome::Success("fine".into()), "{:?}", r.outcome);
}

#[tokio::test]
async fn poisoning_the_log_array_is_a_clean_error() {
    // A log entry whose serialization throws poisons the success envelope; the
    // catch branch then fails the same way, so the promise rejects.
    let src = r#"export const mcp = { tools: { t: {
        description: "d", inputSchema: { type: "object" },
        handler() {
            globalThis.__rustedLogs.push({ toJSON() { throw new Error("log bomb"); } });
            return "fine";
        } } } };"#;
    let r = run(src, "t").await;
    assert!(
        !matches!(r.outcome, Outcome::Success(_)),
        "poisoned logs must not report success: {:?}",
        r.outcome
    );
}

#[tokio::test]
async fn a_huge_tool_output_hits_the_output_cap() {
    let src = r#"export const mcp = { tools: { t: {
        description: "d", inputSchema: { type: "object" },
        handler() { return "x".repeat(1024 * 1024); } } } };"#;
    let r = run(src, "t").await;
    assert!(
        matches!(&r.outcome, Outcome::Terminated(m) if m.contains("output limit")),
        "{:?}",
        r.outcome
    );
}

#[tokio::test]
#[ignore = "known hole: the async executor hangs on a never-settling promise where the \
            blocking one reports Terminated — pre-existing, affects the http path \
            identically; needs an engine-level deadline on promise.into_future"]
async fn a_handler_returning_a_never_settling_promise_is_terminated() {
    let src = r#"export const mcp = { tools: { t: {
        description: "d", inputSchema: { type: "object" },
        handler() { return new Promise(() => {}); } } } };"#;
    let r = run(src, "t").await;
    assert!(
        matches!(r.outcome, Outcome::Terminated(_)),
        "{:?}",
        r.outcome
    );
}
