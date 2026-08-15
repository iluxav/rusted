//! Sessions and API keys: hashed at rest, verified through in-memory caches.
//! Steady-state traffic never touches Postgres — NOTIFY events (see
//! [`crate::store::INVALIDATION_CHANNEL`]) evict entries the moment a key is
//! revoked or a session ends; TTLs are only the fallback.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

use crate::state::AppState;
use crate::store::INVALIDATION_CHANNEL;

const POSITIVE_TTL: Duration = Duration::from_secs(300);
const NEGATIVE_TTL: Duration = Duration::from_secs(10);
const LAST_USED_FLUSH: Duration = Duration::from_secs(60);
pub const SESSION_COOKIE: &str = "rusted_session";
const SESSION_DAYS: i64 = 30;

pub fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

pub fn random_token(bytes: usize) -> String {
    let buf: Vec<u8> = (0..bytes).map(|_| rand::random::<u8>()).collect();
    hex::encode(buf)
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: Uuid,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub email: Option<String>,
}

struct KeyEntry {
    /// Hash of the key's secret; None caches a miss (unknown or revoked).
    secret_hash: Option<String>,
    user_id: Option<Uuid>,
    at: Instant,
    last_used_flush: Instant,
}

struct SessionEntry {
    user: Option<User>,
    at: Instant,
}

#[derive(Default)]
pub struct AuthCaches {
    keys: Mutex<HashMap<String, KeyEntry>>,
    sessions: Mutex<HashMap<String, SessionEntry>>,
}

impl AuthCaches {
    pub fn invalidate_key(&self, lookup: &str) {
        self.keys.lock().unwrap().remove(lookup);
    }

    pub fn invalidate_session(&self, token_hash: &str) {
        self.sessions.lock().unwrap().remove(token_hash);
    }

    pub fn clear(&self) {
        self.keys.lock().unwrap().clear();
        self.sessions.lock().unwrap().clear();
    }
}

// ------------------------------------------------------------------ api keys

/// Verifies `rk_live_<lookup>_<secret>` against the cache, falling back to the
/// DB. The lookup is a random handle, so tokens never expose row identities.
pub async fn verify_key(state: &AppState, token: &str) -> bool {
    let Some((lookup, presented_hash)) = parse_key(token) else {
        return false;
    };
    let now = Instant::now();
    if let Some(entry) = state.auth.keys.lock().unwrap().get_mut(&lookup) {
        let ttl = if entry.secret_hash.is_some() {
            POSITIVE_TTL
        } else {
            NEGATIVE_TTL
        };
        if now.duration_since(entry.at) < ttl {
            let ok = entry.secret_hash.as_deref() == Some(presented_hash.as_str());
            if ok && now.duration_since(entry.last_used_flush) > LAST_USED_FLUSH {
                entry.last_used_flush = now;
                flush_last_used(state.pool.clone(), lookup.clone());
            }
            return ok;
        }
    }
    let row = sqlx::query(
        "SELECT secret_hash, user_id FROM api_keys WHERE lookup = $1 AND revoked_at IS NULL",
    )
    .bind(&lookup)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let stored: Option<String> = row.as_ref().map(|row| row.get("secret_hash"));
    let owner: Option<Uuid> = row.as_ref().map(|row| row.get("user_id"));
    let ok = stored.as_deref() == Some(presented_hash.as_str());
    if ok {
        flush_last_used(state.pool.clone(), lookup.clone());
    }
    state.auth.keys.lock().unwrap().insert(
        lookup,
        KeyEntry {
            secret_hash: stored,
            user_id: owner,
            at: now,
            last_used_flush: now,
        },
    );
    ok
}

/// Resolves an API key to its owner — the admin API's authentication.
pub async fn user_for_key(state: &AppState, token: &str) -> Option<Uuid> {
    let (lookup, _) = parse_key(token)?;
    if !verify_key(state, token).await {
        return None;
    }
    state
        .auth
        .keys
        .lock()
        .unwrap()
        .get(&lookup)
        .and_then(|entry| entry.user_id)
}

fn parse_key(token: &str) -> Option<(String, String)> {
    let rest = token.strip_prefix("rk_live_")?;
    let (lookup, secret) = rest.split_once('_')?;
    if lookup.is_empty() || secret.is_empty() {
        return None;
    }
    Some((lookup.to_string(), sha256_hex(secret)))
}

fn flush_last_used(pool: PgPool, lookup: String) {
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE lookup = $1")
            .bind(lookup)
            .execute(&pool)
            .await;
    });
}

pub async fn create_key(pool: &PgPool, user_id: Uuid, name: &str) -> sqlx::Result<(Uuid, String)> {
    let secret = random_token(20);
    let lookup = random_token(6);
    let id: Uuid = sqlx::query(
        "INSERT INTO api_keys (user_id, name, secret_hash, prefix, lookup)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(user_id)
    .bind(name)
    .bind(sha256_hex(&secret))
    .bind(&secret[..8])
    .bind(&lookup)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok((id, format!("rk_live_{lookup}_{secret}")))
}

pub async fn revoke_key(
    pool: &PgPool,
    caches: &AuthCaches,
    user_id: Uuid,
    id: Uuid,
) -> sqlx::Result<bool> {
    let lookup: Option<String> = sqlx::query(
        "UPDATE api_keys SET revoked_at = now()
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL
         RETURNING lookup",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .map(|row| row.get("lookup"));
    let Some(lookup) = lookup else {
        return Ok(false);
    };
    caches.invalidate_key(&lookup);
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(INVALIDATION_CHANNEL)
        .bind(format!("key:{lookup}"))
        .execute(pool)
        .await?;
    Ok(true)
}

// ------------------------------------------------------------------ sessions

pub async fn create_session(pool: &PgPool, user_id: Uuid) -> sqlx::Result<String> {
    let token = random_token(32);
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, expires_at)
         VALUES ($1, $2, now() + make_interval(days => $3))",
    )
    .bind(sha256_hex(&token))
    .bind(user_id)
    .bind(SESSION_DAYS as i32)
    .execute(pool)
    .await?;
    Ok(token)
}

pub async fn resolve_session(state: &AppState, token: &str) -> Option<User> {
    let token_hash = sha256_hex(token);
    let now = Instant::now();
    if let Some(entry) = state.auth.sessions.lock().unwrap().get(&token_hash) {
        let ttl = if entry.user.is_some() {
            POSITIVE_TTL
        } else {
            NEGATIVE_TTL
        };
        if now.duration_since(entry.at) < ttl {
            return entry.user.clone();
        }
    }
    let user = sqlx::query(
        "SELECT u.id, u.login, u.name, u.avatar_url, u.email
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.expires_at > now()",
    )
    .bind(&token_hash)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()
    .map(|row| User {
        id: row.get("id"),
        login: row.get("login"),
        name: row.get("name"),
        avatar_url: row.get("avatar_url"),
        email: row.get("email"),
    });
    state.auth.sessions.lock().unwrap().insert(
        token_hash,
        SessionEntry {
            user: user.clone(),
            at: now,
        },
    );
    user
}

pub async fn destroy_session(pool: &PgPool, caches: &AuthCaches, token: &str) {
    let token_hash = sha256_hex(token);
    let _ = sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(&token_hash)
        .execute(pool)
        .await;
    caches.invalidate_session(&token_hash);
    let _ = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(INVALIDATION_CHANNEL)
        .bind(format!("session:{token_hash}"))
        .execute(pool)
        .await;
}

pub async fn upsert_github_user(
    pool: &PgPool,
    github_id: i64,
    login: &str,
    name: Option<&str>,
    avatar_url: Option<&str>,
    email: Option<&str>,
) -> sqlx::Result<Uuid> {
    // COALESCE: a login where the email fetch failed must not erase an
    // address we already know.
    let id: Uuid = sqlx::query(
        "INSERT INTO users (github_id, login, name, avatar_url, email) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (github_id) DO UPDATE
             SET login = $2, name = $3, avatar_url = $4,
                 email = COALESCE($5, users.email)
         RETURNING id",
    )
    .bind(github_id)
    .bind(login)
    .bind(name)
    .bind(avatar_url)
    .bind(email)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

// -------------------------------------------------------- invalidation feed

/// Holds a LISTEN connection and evicts cache entries as NOTIFY events arrive.
/// On any (re)connect every cache is flushed — events may have been missed,
/// and correctness beats warmth.
pub async fn invalidation_listener(database_url: String, state: Arc<AppState>) {
    loop {
        if let Ok(mut listener) = sqlx::postgres::PgListener::connect(&database_url).await {
            if listener.listen(INVALIDATION_CHANNEL).await.is_ok() {
                state.store.invalidate_all();
                state.auth.clear();
                state.plan_cache.clear();
                state.inboxes.clear();
                state.secrets.invalidate_all();
                state.fnstate.invalidate_all();
                while let Ok(event) = listener.recv().await {
                    dispatch(&state, event.payload());
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn dispatch(state: &AppState, payload: &str) {
    match payload.split_once(':') {
        Some(("function", name)) => state.store.invalidate(name),
        Some(("inbox", address)) => state.inboxes.invalidate(address),
        Some(("key", lookup)) => state.auth.invalidate_key(lookup),
        Some(("session", token_hash)) => state.auth.invalidate_session(token_hash),
        Some(("subscription", user_id)) => {
            if let Ok(user_id) = user_id.parse() {
                state.plan_cache.invalidate(user_id);
            }
        }
        Some(("secret", rest)) => {
            if let Some((user_id, env)) = rest.split_once(':') {
                if let Ok(user_id) = user_id.parse() {
                    state.secrets.invalidate(user_id, env);
                }
            }
        }
        Some(("fnstate", rest)) => {
            let mut parts = rest.splitn(3, ':');
            if let (Some(user_id), Some(function_name), Some(env)) =
                (parts.next(), parts.next(), parts.next())
            {
                if let Ok(user_id) = user_id.parse() {
                    if env == "*" {
                        state.fnstate.invalidate_all();
                    } else {
                        state.fnstate.invalidate(user_id, function_name, env);
                    }
                }
            }
        }
        _ => {}
    }
}
