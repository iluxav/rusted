//! Throwaway addresses an agent can receive things at.
//!
//! A browser or cloud agent can call out but cannot be reached, which rules out
//! OAuth callbacks, webhooks and anything another party initiates. An inbox is
//! a URL that accepts a POST from anyone and holds it until the owner reads it
//! or it expires.
//!
//! Writing and reading are separate capabilities on purpose. The address is
//! unguessable and grants only writing; reading is by name and needs the
//! owner's key. Handing the URL to Stripe never hands over what Stripe sent.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::state::AppState;

/// Longest an inbox may live. Beyond a few minutes this stops being a place to
/// catch a callback and starts being storage, which is a different product.
pub const MAX_TTL_SECONDS: i64 = 24 * 60 * 60;
pub const DEFAULT_TTL_SECONDS: i64 = 300;

/// Caps on what a public, unauthenticated write endpoint may consume.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_MESSAGES: i64 = 100;
/// Writes accepted over an inbox's whole life. `upsert` overwrites in place, so
/// the message cap alone would leave load unbounded.
pub const MAX_WRITES: i32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Store {
    /// Keep every message.
    Append,
    /// Keep only the most recent.
    Upsert,
}

impl Store {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "append" => Ok(Store::Append),
            "upsert" => Ok(Store::Upsert),
            other => Err(format!(
                "unknown store '{other}': use 'append' to keep every message, or 'upsert' to keep only the latest"
            )),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Store::Append => "append",
            Store::Upsert => "upsert",
        }
    }
}

/// A name has to be safe to put in a URL and easy to type back.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub struct Created {
    pub name: String,
    pub address: String,
    pub store: Store,
    pub drain: bool,
    pub expires_in_seconds: i64,
}

/// Creates an inbox, replacing any earlier one of the same name.
///
/// Replacing rather than refusing: a name is a handle the owner reuses, and an
/// agent retrying a flow should not have to invent `stripe-data-2`.
pub async fn create(
    state: &AppState,
    user_id: Uuid,
    name: &str,
    ttl_seconds: i64,
    store: Store,
    drain: bool,
) -> Result<Created, String> {
    if !valid_name(name) {
        return Err("names are 1-64 chars of a-z, 0-9, '-', '_'".to_string());
    }
    if ttl_seconds <= 0 || ttl_seconds > MAX_TTL_SECONDS {
        return Err(format!(
            "ttl must be between 1 second and {} hours",
            MAX_TTL_SECONDS / 3600
        ));
    }

    let address = crate::auth::random_token(24);
    sqlx::query(
        "INSERT INTO inboxes (user_id, name, address, store, drain, expires_at)
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))
         ON CONFLICT (user_id, name) DO UPDATE SET
           address    = EXCLUDED.address,
           store      = EXCLUDED.store,
           drain      = EXCLUDED.drain,
           expires_at = EXCLUDED.expires_at,
           writes     = 0,
           created_at = now()",
    )
    .bind(user_id)
    .bind(name)
    .bind(&address)
    .bind(store.as_str())
    .bind(drain)
    .bind(ttl_seconds as f64)
    .execute(&state.pool)
    .await
    .map_err(|e| format!("could not create the inbox: {e}"))?;

    Ok(Created {
        name: name.to_string(),
        address,
        store,
        drain,
        expires_in_seconds: ttl_seconds,
    })
}

/// What a read found.
pub enum Reading {
    /// Alive, with whatever has arrived — possibly nothing.
    Alive { messages: Vec<Value>, drained: bool },
    /// Expired, drained, or never existed. Deliberately one case: telling them
    /// apart would say whether a name was ever real.
    Gone,
}

/// Reads by name, on behalf of the owner.
pub async fn read(state: &AppState, user_id: Uuid, name: &str) -> Result<Reading, String> {
    let row = sqlx::query(
        "SELECT id, store, drain, expires_at < now() AS expired
           FROM inboxes WHERE user_id = $1 AND name = $2",
    )
    .bind(user_id)
    .bind(name)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| format!("could not read the inbox: {e}"))?;

    let Some(row) = row else {
        return Ok(Reading::Gone);
    };
    if row.get::<bool, _>("expired") {
        return Ok(Reading::Gone);
    }

    let inbox_id: Uuid = row.get("id");
    let drain: bool = row.get("drain");

    let rows =
        sqlx::query("SELECT body FROM inbox_messages WHERE inbox_id = $1 ORDER BY received_at, id")
            .bind(inbox_id)
            .fetch_all(&state.pool)
            .await
            .map_err(|e| format!("could not read messages: {e}"))?;

    let messages: Vec<Value> = rows
        .iter()
        .map(|r| {
            let body: String = r.get("body");
            // Most senders post JSON; anything else comes back as the string it
            // was, rather than being forced into a shape it does not have.
            serde_json::from_str(&body).unwrap_or(Value::String(body))
        })
        .collect();

    // Only a read that found something drains. Draining an empty inbox would
    // destroy it while the agent was still waiting for the first message.
    let drained = drain && !messages.is_empty();
    if drained {
        let _ = sqlx::query("DELETE FROM inboxes WHERE id = $1")
            .bind(inbox_id)
            .execute(&state.pool)
            .await;
    }

    Ok(Reading::Alive { messages, drained })
}

pub async fn list(state: &AppState, user_id: Uuid) -> Result<Vec<Value>, String> {
    let rows = sqlx::query(
        "SELECT i.name, i.address, i.store, i.drain, i.writes,
                GREATEST(0, EXTRACT(EPOCH FROM (i.expires_at - now()))::bigint) AS expires_in,
                (SELECT count(*) FROM inbox_messages m WHERE m.inbox_id = i.id) AS held
           FROM inboxes i
          WHERE i.user_id = $1 AND i.expires_at > now()
          ORDER BY i.created_at DESC",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| format!("could not list inboxes: {e}"))?;

    Ok(rows
        .iter()
        .map(|r| {
            json!({
                "name": r.get::<String, _>("name"),
                "url": state.console_url(&format!("/inbox/{}", r.get::<String, _>("address"))),
                "store": r.get::<String, _>("store"),
                "drain": r.get::<bool, _>("drain"),
                "held": r.get::<i64, _>("held"),
                "writes": r.get::<i32, _>("writes"),
                "expires_in_seconds": r.get::<i64, _>("expires_in"),
            })
        })
        .collect())
}

pub async fn delete(state: &AppState, user_id: Uuid, name: &str) -> Result<bool, String> {
    let done = sqlx::query("DELETE FROM inboxes WHERE user_id = $1 AND name = $2")
        .bind(user_id)
        .bind(name)
        .execute(&state.pool)
        .await
        .map_err(|e| format!("could not delete the inbox: {e}"))?;
    Ok(done.rows_affected() > 0)
}

/// Removes what has expired. Called on the same sweep as everything else.
pub async fn sweep(state: &AppState) {
    let _ = sqlx::query("DELETE FROM inboxes WHERE expires_at < now()")
        .execute(&state.pool)
        .await;
}

// ------------------------------------------------------------- public write

/// Accepts a POST from anyone holding the address.
///
/// Gone is `410`, not `404`: it is terminal to a well-behaved webhook sender,
/// which stops retrying rather than escalating to a disabled endpoint. It is
/// also the same answer for expired, drained and never-existed, so probing
/// addresses reveals nothing.
async fn receive(
    State(state): State<Arc<AppState>>,
    Path(address): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let gone = || {
        (
            StatusCode::GONE,
            Json(json!({ "error": { "code": "gone", "message": "this inbox has expired or does not exist" } })),
        )
            .into_response()
    };

    if body.len() > MAX_MESSAGE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": { "code": "too_large",
                "message": format!("messages are limited to {MAX_MESSAGE_BYTES} bytes") } })),
        )
            .into_response();
    }
    // The same rule as function bodies: refuse rather than store mangled text.
    let Ok(body) = String::from_utf8(body.to_vec()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "code": "invalid_body",
                "message": "the body is not valid UTF-8; inboxes hold text, so send JSON or base64" } })),
        )
            .into_response();
    };

    let row = sqlx::query(
        "SELECT id, store, writes, expires_at < now() AS expired
           FROM inboxes WHERE address = $1",
    )
    .bind(&address)
    .fetch_optional(&state.pool)
    .await;

    let Ok(Some(row)) = row else {
        return gone();
    };
    if row.get::<bool, _>("expired") {
        return gone();
    }
    if row.get::<i32, _>("writes") >= MAX_WRITES {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": { "code": "inbox_full",
                "message": format!("this inbox has accepted its limit of {MAX_WRITES} writes") } })),
        )
            .into_response();
    }

    let inbox_id: Uuid = row.get("id");
    let store: String = row.get("store");

    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(_) => return gone(),
    };
    // Upsert keeps only the latest, so the previous message goes first.
    if store == "upsert" {
        let _ = sqlx::query("DELETE FROM inbox_messages WHERE inbox_id = $1")
            .bind(inbox_id)
            .execute(&mut *tx)
            .await;
    } else {
        let held: i64 = sqlx::query("SELECT count(*) AS n FROM inbox_messages WHERE inbox_id = $1")
            .bind(inbox_id)
            .fetch_one(&mut *tx)
            .await
            .map(|r| r.get::<i64, _>("n"))
            .unwrap_or(0);
        if held >= MAX_MESSAGES {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": { "code": "inbox_full",
                    "message": format!("this inbox already holds {MAX_MESSAGES} messages") } })),
            )
                .into_response();
        }
    }

    if sqlx::query("INSERT INTO inbox_messages (inbox_id, body) VALUES ($1, $2)")
        .bind(inbox_id)
        .bind(&body)
        .execute(&mut *tx)
        .await
        .is_err()
    {
        return gone();
    }
    let _ = sqlx::query("UPDATE inboxes SET writes = writes + 1 WHERE id = $1")
        .bind(inbox_id)
        .execute(&mut *tx)
        .await;
    if tx.commit().await.is_err() {
        return gone();
    }

    (StatusCode::ACCEPTED, Json(json!({ "received": true }))).into_response()
}

// -------------------------------------------------------------- owner's API

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    #[serde(default)]
    ttl_seconds: Option<i64>,
    #[serde(default)]
    store: Option<String>,
    #[serde(default)]
    drain: Option<bool>,
}

fn api_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message.into() } })),
    )
        .into_response()
}

pub async fn create_endpoint(
    state: &Arc<AppState>,
    user_id: Uuid,
    raw: Value,
) -> Result<Value, Response> {
    let body: CreateBody = serde_json::from_value(raw)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, "bad_request", e.to_string()))?;
    let store = match body.store.as_deref() {
        Some(raw) => Store::parse(raw)
            .map_err(|e| api_error(StatusCode::UNPROCESSABLE_ENTITY, "bad_store", e))?,
        None => Store::Append,
    };
    let created = create(
        state,
        user_id,
        &body.name,
        body.ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS),
        store,
        body.drain.unwrap_or(false),
    )
    .await
    .map_err(|e| api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_inbox", e))?;

    Ok(json!({
        "name": created.name,
        "url": state.console_url(&format!("/inbox/{}", created.address)),
        "store": created.store.as_str(),
        "drain": created.drain,
        "expires_in_seconds": created.expires_in_seconds,
        "note": "anyone holding the URL can POST to it; reading needs your key",
    }))
}

/// The public write route. Everything else is owned by the admin API.
pub fn public_router(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/inbox/{address}", axum::routing::post(receive))
        .layer(axum::extract::DefaultBodyLimit::max(
            MAX_MESSAGE_BYTES + 4096,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_url_safe() {
        assert!(valid_name("stripe-data"));
        assert!(valid_name("oauth_cb2"));
        assert!(!valid_name(""));
        assert!(!valid_name("Has Capitals"));
        assert!(!valid_name("has/slash"));
        assert!(!valid_name(&"x".repeat(65)));
    }

    #[test]
    fn store_modes_parse_and_explain_themselves() {
        assert_eq!(Store::parse("append").unwrap(), Store::Append);
        assert_eq!(Store::parse("upsert").unwrap(), Store::Upsert);
        let err = Store::parse("queue").unwrap_err();
        assert!(err.contains("append") && err.contains("upsert"), "{err}");
    }
}
