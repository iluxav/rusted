//! JSON-RPC wire handling for MCP over Streamable HTTP, shared by the
//! platform `/mcp` endpoint and deployed MCP functions.
//!
//! This module is transport only: it parses the request body, frames the
//! reply (single vs batch, 202 for notification-only posts, `mcp-session-id`
//! echo) and shapes JSON-RPC envelopes. What a message *means* — method
//! dispatch — stays with each caller, supplied as a per-message closure.

use std::future::Future;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

pub const MCP_PROTOCOL: &str = "2025-06-18";

/// A JSON-RPC success envelope.
pub fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC error envelope.
pub fn err(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Tool failures come back as results, not JSON-RPC errors: that is how the
/// model sees what broke and fixes its own code.
pub fn tool_result(text: impl Into<String>, is_error: bool) -> Value {
    let mut result = json!({ "content": [{ "type": "text", "text": text.into() }] });
    if is_error {
        result["isError"] = json!(true);
    }
    result
}

/// MCP over Streamable HTTP in JSON response mode — the spec allows answering
/// a POST with one `application/json` body instead of an SSE stream, so no
/// session or streaming machinery is needed.
///
/// `handle` answers one JSON-RPC message; `None` means it was a notification
/// and wants no reply.
pub async fn respond<F, Fut>(body: &[u8], headers: &HeaderMap, handle: F) -> Response
where
    F: Fn(Value) -> Fut,
    Fut: Future<Output = Option<Value>>,
{
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "jsonrpc": "2.0", "id": null,
                         "error": { "code": -32700, "message": "parse error" } })),
        )
            .into_response();
    };

    let is_batch = message.is_array();
    let replies: Vec<Value> = match message {
        Value::Array(batch) => {
            let mut out = Vec::new();
            for m in batch {
                if let Some(reply) = handle(m).await {
                    out.push(reply);
                }
            }
            out
        }
        single => handle(single).await.into_iter().collect(),
    };

    // Nothing asked for an answer — say so the way the spec does.
    if replies.is_empty() {
        return (StatusCode::ACCEPTED, "").into_response();
    }

    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("stateless")
        .to_string();
    let payload = if is_batch {
        Value::Array(replies)
    } else {
        replies.into_iter().next().expect("one reply")
    };
    ([("mcp-session-id", session)], Json(payload)).into_response()
}
