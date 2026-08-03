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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

    // Reusing a name mints a new address, so any copy of the old one must go.
    announce(state, &address).await;
    if let Some(stale) = state.inboxes.cached_address_for(user_id, name) {
        announce(state, &stale).await;
    }

    Ok(Created {
        name: name.to_string(),
        address,
        store,
        drain,
        expires_in_seconds: ttl_seconds,
    })
}

// -------------------------------------------------------------- memory layer

/// An inbox and its messages, as held in memory.
#[derive(Clone)]
struct Cached {
    id: Uuid,
    user_id: Uuid,
    name: String,
    store: Store,
    drain: bool,
    writes: i32,
    messages: Vec<String>,
}

/// Write-through cache in front of Postgres.
///
/// Reads and writes hit memory; Postgres is written on the same call, so a
/// restart loses nothing and a cold instance fills itself on first use. That is
/// the difference between a cache and a store: an inbox holds someone's OAuth
/// code, and losing it to a deploy would be a hang with no explanation.
///
/// Other instances are told to drop their copy through the same LISTEN/NOTIFY
/// channel everything else uses. The payload is not sent — `NOTIFY` caps at
/// 8000 bytes and a webhook body routinely exceeds it — so the notification
/// carries the address and the reader reloads.
#[derive(Default)]
pub struct InboxCache {
    by_address: Mutex<HashMap<String, Cached>>,
    /// So a read by (owner, name) does not have to scan.
    address_of: Mutex<HashMap<(Uuid, String), String>>,
}

impl InboxCache {
    pub fn invalidate(&self, address: &str) {
        if let Some(entry) = self.by_address.lock().unwrap().remove(address) {
            self.address_of
                .lock()
                .unwrap()
                .remove(&(entry.user_id, entry.name));
        }
    }

    pub fn clear(&self) {
        self.by_address.lock().unwrap().clear();
        self.address_of.lock().unwrap().clear();
    }

    fn get(&self, address: &str) -> Option<Cached> {
        self.by_address.lock().unwrap().get(address).cloned()
    }

    fn cached_address_for(&self, user_id: Uuid, name: &str) -> Option<String> {
        self.address_of
            .lock()
            .unwrap()
            .get(&(user_id, name.to_string()))
            .cloned()
    }

    fn put(&self, address: &str, entry: Cached) {
        self.address_of
            .lock()
            .unwrap()
            .insert((entry.user_id, entry.name.clone()), address.to_string());
        self.by_address
            .lock()
            .unwrap()
            .insert(address.to_string(), entry);
    }
}

/// Tells every instance, including this one, to drop its copy.
async fn announce(state: &AppState, address: &str) {
    state.inboxes.invalidate(address);
    let _ = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(crate::store::INVALIDATION_CHANNEL)
        .bind(format!("inbox:{address}"))
        .execute(&state.pool)
        .await;
}

/// Loads an inbox and its messages from Postgres into memory.
///
/// Expiry is decided by the database rather than a timestamp held in memory:
/// one clock, and no stale copy outliving its TTL.
async fn load(state: &AppState, address: &str) -> Option<Cached> {
    let row = sqlx::query(
        "SELECT id, user_id, name, store, drain, writes
           FROM inboxes WHERE address = $1 AND expires_at > now()",
    )
    .bind(address)
    .fetch_optional(&state.pool)
    .await
    .ok()??;

    let id: Uuid = row.get("id");
    let messages =
        sqlx::query("SELECT body FROM inbox_messages WHERE inbox_id = $1 ORDER BY received_at, id")
            .bind(id)
            .fetch_all(&state.pool)
            .await
            .ok()?
            .iter()
            .map(|r| r.get::<String, _>("body"))
            .collect();

    let entry = Cached {
        id,
        user_id: row.get("user_id"),
        name: row.get("name"),
        store: Store::parse(&row.get::<String, _>("store")).unwrap_or(Store::Append),
        drain: row.get("drain"),
        writes: row.get("writes"),
        messages,
    };
    state.inboxes.put(address, entry.clone());
    Some(entry)
}

/// Memory first, Postgres on a miss.
async fn resolve(state: &AppState, address: &str) -> Option<Cached> {
    match state.inboxes.get(address) {
        Some(entry) => Some(entry),
        None => load(state, address).await,
    }
}

/// The address behind a name, which needs a lookup on a cold instance.
async fn address_for(state: &AppState, user_id: Uuid, name: &str) -> Option<String> {
    if let Some(address) = state.inboxes.cached_address_for(user_id, name) {
        return Some(address);
    }
    let address: String = sqlx::query(
        "SELECT address FROM inboxes WHERE user_id = $1 AND name = $2 AND expires_at > now()",
    )
    .bind(user_id)
    .bind(name)
    .fetch_optional(&state.pool)
    .await
    .ok()??
    .get("address");
    load(state, &address).await;
    Some(address)
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
    let Some(address) = address_for(state, user_id, name).await else {
        return Ok(Reading::Gone);
    };
    let Some(entry) = resolve(state, &address).await else {
        return Ok(Reading::Gone);
    };
    // The name index is per-owner, but a cold lookup could still land on
    // someone else's row if a name were reused; check rather than assume.
    if entry.user_id != user_id {
        return Ok(Reading::Gone);
    }

    let messages: Vec<Value> = entry
        .messages
        .iter()
        .map(|body| {
            // Most senders post JSON; anything else comes back as the string it
            // was, rather than being forced into a shape it does not have.
            serde_json::from_str(body).unwrap_or(Value::String(body.clone()))
        })
        .collect();

    // Only a read that found something drains. Draining an empty inbox would
    // destroy it while the agent was still waiting for the first message.
    let drained = entry.drain && !messages.is_empty();
    if drained {
        let _ = sqlx::query("DELETE FROM inboxes WHERE id = $1")
            .bind(entry.id)
            .execute(&state.pool)
            .await;
        announce(state, &address).await;
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
    if let Some(address) = address_for(state, user_id, name).await {
        announce(state, &address).await;
    }
    let done = sqlx::query("DELETE FROM inboxes WHERE user_id = $1 AND name = $2")
        .bind(user_id)
        .bind(name)
        .execute(&state.pool)
        .await
        .map_err(|e| format!("could not delete the inbox: {e}"))?;
    Ok(done.rows_affected() > 0)
}

/// Deletes what has expired, on a loop.
///
/// Not cosmetic. Reads already filter on `expires_at`, so an unswept inbox is
/// invisible — but the messages are still there, and they hold whatever a
/// webhook sent: card metadata, an authorization code, someone's email. The TTL
/// is a promise that the payload stops existing, not that it stops being
/// listed, and only this keeps it.
pub async fn sweeper(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        sweep_once(&state).await;
    }
}

/// One pass. Messages go with the inbox through the foreign key cascade.
pub async fn sweep_once(state: &AppState) -> u64 {
    sqlx::query("DELETE FROM inboxes WHERE expires_at < now()")
        .execute(&state.pool)
        .await
        .map(|done| done.rows_affected())
        .unwrap_or(0)
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

    let Some(entry) = resolve(&state, &address).await else {
        return gone();
    };
    if entry.writes >= MAX_WRITES {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": { "code": "inbox_full",
                "message": format!("this inbox has accepted its limit of {MAX_WRITES} writes") } })),
        )
            .into_response();
    }
    if entry.store == Store::Append && entry.messages.len() as i64 >= MAX_MESSAGES {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": { "code": "inbox_full",
                "message": format!("this inbox already holds {MAX_MESSAGES} messages") } })),
        )
            .into_response();
    }

    // Written through: Postgres first, so a crash between the two loses a
    // message rather than inventing one, then memory so the next read is warm.
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(_) => return gone(),
    };
    if entry.store == Store::Upsert {
        let _ = sqlx::query("DELETE FROM inbox_messages WHERE inbox_id = $1")
            .bind(entry.id)
            .execute(&mut *tx)
            .await;
    }
    if sqlx::query("INSERT INTO inbox_messages (inbox_id, body) VALUES ($1, $2)")
        .bind(entry.id)
        .bind(&body)
        .execute(&mut *tx)
        .await
        .is_err()
    {
        return gone();
    }
    let _ = sqlx::query("UPDATE inboxes SET writes = writes + 1 WHERE id = $1")
        .bind(entry.id)
        .execute(&mut *tx)
        .await;
    if tx.commit().await.is_err() {
        return gone();
    }

    let mut updated = entry;
    if updated.store == Store::Upsert {
        updated.messages.clear();
    }
    updated.messages.push(body);
    updated.writes += 1;
    state.inboxes.put(&address, updated);
    // Other instances hold a stale copy now. The body is too big for a NOTIFY
    // payload, so they are told to reload rather than sent the message.
    let _ = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(crate::store::INVALIDATION_CHANNEL)
        .bind(format!("inbox:{address}"))
        .execute(&state.pool)
        .await;

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

// --------------------------------------------------- what a handler may see

/// Lends `context.inbox` to a running handler, fixed to one owner.
///
/// The user id is captured when this is built, from the function's stored
/// owner — never from anything the handler says. So a handler can ask for a
/// name, and only ever gets an inbox belonging to whoever deployed it.
pub struct OwnerScopedInbox {
    state: Arc<AppState>,
    user_id: Uuid,
}

impl OwnerScopedInbox {
    pub fn new(state: Arc<AppState>, user_id: Uuid) -> Self {
        Self { state, user_id }
    }
}

impl rusted_engine::HostServices for OwnerScopedInbox {
    fn inbox_get(
        &self,
        name: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(async move {
            match read(&self.state, self.user_id, &name).await {
                Ok(Reading::Alive { messages, .. }) => {
                    Ok(serde_json::to_string(&messages).unwrap_or_else(|_| "[]".into()))
                }
                // A handler gets the same answer a person does: gone is gone,
                // and it cannot tell that apart from never having existed.
                Ok(Reading::Gone) => Err(format!(
                    "inbox '{name}' has expired, been drained, or never existed"
                )),
                Err(e) => Err(e),
            }
        })
    }
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
