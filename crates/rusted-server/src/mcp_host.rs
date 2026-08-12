//! Serves deployed MCP functions over Streamable HTTP.
//!
//! A function whose module declares `export const mcp` is not an HTTP endpoint
//! but an MCP server: every POST to `/f/{name}` carries JSON-RPC, answered
//! entirely from the deploy-time metadata (`tools/list`, `initialize`) except
//! for `tools/call`, which validates the arguments against the stored schema
//! and runs the named tool through the same admission path — plan limits, rate
//! limit, concurrency, analytics — as an http invocation.
//!
//! [`handle_message`] is transport- and host-free on purpose: the server wires
//! plans and analytics into its `call` closure here, and `rusted run` drives
//! the same dispatch with a bare executor.

use std::future::Future;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use rusted_engine::Outcome;
use serde_json::{json, Value};

use crate::api::{execute_serialized, grant_for_function, plan_for_owner, record_refusal, Job};
use crate::mcp_wire;
use crate::state::AppState;
use crate::store::Fetched;

/// Answers one POST to a deployed mcp function. `fetched.kind` is already
/// known to be `"mcp"`; the route and method checks happened in the caller.
pub async fn serve(
    state: Arc<AppState>,
    fetched: Arc<Fetched>,
    name: String,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let meta = fetched.mcp.clone().unwrap_or_else(|| json!({}));

    // Auth comes before any parsing: a caller without the owner's key learns
    // nothing about the body's validity, the tools, or the protocol.
    let public = meta
        .get("public")
        .and_then(|p| p.as_bool())
        .unwrap_or(false);
    if !public {
        let presented = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let caller = match presented {
            Some(token) => crate::auth::user_for_key(&state, token).await,
            None => None,
        };
        // Missing, invalid, and someone else's key all get the same answer: a
        // wrong key must not learn whose function this is.
        let authorized = matches!((caller, fetched.owner), (Some(c), Some(o)) if c == o);
        if !authorized {
            return (
                StatusCode::UNAUTHORIZED,
                [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
                Json(json!({ "error": {
                    "code": "unauthorized",
                    "message": "this mcp function requires its owner's API key (Authorization: Bearer rk_live_…)"
                }})),
            )
                .into_response();
        }
    }

    let state = &state;
    let fetched = &fetched;
    let meta = &meta;
    let name = name.as_str();
    mcp_wire::respond(&body, &headers, move |msg| async move {
        handle_message(meta, name, fetched.rev, msg, move |tool, args| async move {
            call_tool(state, fetched, name, tool, args).await
        })
        .await
    })
    .await
}

/// Answers one JSON-RPC message from stored mcp metadata. `None` means it was
/// a notification and wants no reply.
///
/// Transport-free: everything but `tools/call` is served from `meta` alone,
/// and a call reaches `call` only with a tool that exists and arguments its
/// schema accepts. `call` returns a tool result (see [`mcp_wire::tool_result`])
/// — the server's closure adds rate limiting and analytics, a local runner
/// needs neither.
pub async fn handle_message<F, Fut>(
    meta: &Value,
    name: &str,
    rev: u64,
    msg: Value,
    call: F,
) -> Option<Value>
where
    F: Fn(String, Value) -> Fut,
    Fut: Future<Output = Value>,
{
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let ok = |result: Value| Some(mcp_wire::ok(id.clone(), result));

    match method {
        "initialize" => ok(json!({
            "protocolVersion": params.get("protocolVersion")
                .and_then(|v| v.as_str()).unwrap_or(mcp_wire::MCP_PROTOCOL),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": name, "version": format!("rev-{rev}") },
        })),
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({ "tools": tool_listing(meta) })),
        "tools/call" => {
            let tool = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let Some(spec) = meta.get("tools").and_then(|t| t.get(tool)) else {
                return ok(mcp_wire::tool_result(format!("unknown tool: {tool}"), true));
            };
            // Refused before spending anything: a schema violation is the
            // model's mistake, named precisely so it can correct itself.
            if let Some(violation) = schema_violation(&spec["inputSchema"], &args) {
                return ok(mcp_wire::tool_result(
                    format!("invalid arguments: {violation}"),
                    true,
                ));
            }
            ok(call(tool.to_string(), args).await)
        }
        other => Some(mcp_wire::err(
            id.clone(),
            -32601,
            &format!("method not found: {other}"),
        )),
    }
}

/// The tools array for `tools/list`, straight from the stored metadata.
fn tool_listing(meta: &Value) -> Vec<Value> {
    meta.get("tools")
        .and_then(|t| t.as_object())
        .map(|tools| {
            tools
                .iter()
                .map(|(name, spec)| {
                    json!({
                        "name": name,
                        "description": spec.get("description").cloned().unwrap_or_else(|| json!("")),
                        "inputSchema": spec.get("inputSchema").cloned()
                            .unwrap_or_else(|| json!({ "type": "object" })),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The first way `args` violates `schema`, with its instance path — or `None`
/// when the arguments are acceptable. The schema was validated at deploy, so a
/// schema that fails to compile here validates nothing rather than refusing
/// every call.
fn schema_violation(schema: &Value, args: &Value) -> Option<String> {
    let validator = jsonschema::validator_for(schema).ok()?;
    let error = validator.validate(args).err()?;
    let path = error.instance_path.to_string();
    Some(if path.is_empty() {
        error.to_string()
    } else {
        format!("{error} (at {path})")
    })
}

/// Runs one validated tool call under the owner's plan and maps the outcome
/// to a tool result. This is the only message that spends the rate limit —
/// `initialize` and `tools/list` are metadata reads.
async fn call_tool(
    state: &Arc<AppState>,
    fetched: &Arc<Fetched>,
    name: &str,
    tool: String,
    args: Value,
) -> Value {
    let (plan, limits) = plan_for_owner(state, fetched.owner).await;
    if let Err(retry_after) = state.rate_limiter.check(name, plan.limits.rate_per_min) {
        return mcp_wire::tool_result(
            format!("rate limit exceeded, retry after {retry_after}s"),
            true,
        );
    }
    // Model-visible on purpose: the secret and binding names are deploy
    // metadata, and naming what is missing is how the owner (or their agent)
    // fixes it.
    let grant = match grant_for_function(state, name, fetched, &plan).await {
        Ok(grant) => grant,
        Err((_code, detail)) => {
            record_refusal(state, name, fetched.owner, detail.clone());
            return mcp_wire::tool_result(detail, true);
        }
    };
    let result = match execute_serialized(
        state,
        name,
        fetched.source.clone(),
        Job::Tool { name: tool, args },
        limits,
        fetched.owner,
        plan.limits.concurrency,
        grant,
    )
    .await
    {
        Ok(result) => result,
        // Admission refusals (queue-wait busy, memory pressure) come back as
        // an HTTP response; here the caller is a model, so it gets a tool
        // result it can act on instead.
        Err(refused) => {
            let text = if refused.status() == StatusCode::SERVICE_UNAVAILABLE {
                "the server is at capacity; retry in a moment"
            } else {
                "busy: concurrency limit reached, retry"
            };
            return mcp_wire::tool_result(text, true);
        }
    };
    invocation_tool_result(result)
}

/// Maps a finished tool invocation to the result the model sees. Shared by
/// the deployed server and `rusted run`, so local semantics match production
/// exactly: a thrown error is model-visible, a terminated run says only that
/// a limit was exceeded.
pub fn invocation_tool_result(result: rusted_engine::InvocationResult) -> Value {
    match result.outcome {
        Outcome::Success(body) => {
            let mut out = json!({ "content": [{ "type": "text", "text": body }] });
            // A tool that returned a value (not a string) came back as JSON;
            // carry it as structuredContent too so clients need not re-parse.
            // The spec types structuredContent as an object, so scalars and
            // arrays travel only as text.
            if result.content_type.as_deref() == Some("application/json") {
                if let Ok(parsed) = serde_json::from_str::<Value>(&body) {
                    if parsed.is_object() {
                        out["structuredContent"] = parsed;
                    }
                }
            }
            out
        }
        // Model-visible by design: a thrown error is how the model learns what
        // to fix. The owner's logs keep the stack and console output.
        Outcome::Error(message) => mcp_wire::tool_result(message, true),
        // The detail (which limit, at what value) stays in logs/analytics —
        // callers of a served function don't learn the owner's plan.
        Outcome::Terminated(_) => mcp_wire::tool_result("execution limit exceeded", true),
    }
}
