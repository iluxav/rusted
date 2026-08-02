//! The OAuth device flow (RFC 8628), so a CLI can get a credential without a
//! browser and without anyone pasting a long-lived key into a file.
//!
//! The two endpoints here are deliberately unauthenticated — a client starting
//! this flow has no credential yet, which is the whole point — so they are
//! rate limited and every code expires quickly.

use std::time::Duration;

use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

use crate::auth::{random_token, sha256_hex};

/// Long enough that guessing is hopeless, short enough to read aloud.
const USER_CODE_LEN: usize = 8;
/// No 0/O/1/I/L/U: people transcribe these, sometimes over a phone.
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
pub const EXPIRY: Duration = Duration::from_secs(600);
/// How often the client should poll, in seconds.
pub const POLL_INTERVAL: u64 = 2;

pub struct Pending {
    pub device_code: String,
    pub user_code: String,
}

fn user_code() -> String {
    let raw: String = (0..USER_CODE_LEN)
        .map(|_| ALPHABET[rand::random::<u8>() as usize % ALPHABET.len()] as char)
        .collect();
    format!("{}-{}", &raw[..4], &raw[4..])
}

/// Opens a device authorization request. The device code is returned once and
/// stored only as a hash.
pub async fn start(pool: &PgPool, label: &str) -> sqlx::Result<Pending> {
    let device_code = random_token(32);
    // A collision on the user code is possible but vanishingly rare; retry
    // rather than fail the request.
    for _ in 0..5 {
        let code = user_code();
        let inserted = sqlx::query(
            "INSERT INTO device_codes (device_code_hash, user_code, label, expires_at)
             VALUES ($1, $2, $3, now() + make_interval(secs => $4))
             ON CONFLICT (user_code) DO NOTHING
             RETURNING id",
        )
        .bind(sha256_hex(&device_code))
        .bind(&code)
        .bind(label)
        .bind(EXPIRY.as_secs() as f64)
        .fetch_optional(pool)
        .await?;
        if inserted.is_some() {
            return Ok(Pending {
                device_code,
                user_code: code,
            });
        }
    }
    Err(sqlx::Error::Protocol(
        "could not allocate a user code".into(),
    ))
}

pub enum Poll {
    Pending,
    Denied,
    Expired,
    /// Approved: the key is returned exactly once.
    Approved(String),
}

/// Exchanges a device code for an API key once a human has approved it.
pub async fn poll(pool: &PgPool, device_code: &str) -> sqlx::Result<Poll> {
    // Ask SQL the yes/no questions directly rather than reading timestamps
    // only to test them for presence.
    let Some(row) = sqlx::query(
        "SELECT id, label, user_id,
                approved_at IS NOT NULL AS approved,
                denied_at   IS NOT NULL AS denied,
                redeemed_at IS NOT NULL AS redeemed,
                expires_at < now()      AS expired
         FROM device_codes WHERE device_code_hash = $1",
    )
    .bind(sha256_hex(device_code))
    .fetch_optional(pool)
    .await?
    else {
        return Ok(Poll::Expired);
    };
    if row.get::<bool, _>("denied") {
        return Ok(Poll::Denied);
    }
    // A code is good for one key; a second poll after collection looks expired.
    if row.get::<bool, _>("redeemed") || row.get::<bool, _>("expired") {
        return Ok(Poll::Expired);
    }
    let Some(user_id) = row
        .get::<Option<Uuid>, _>("user_id")
        .filter(|_| row.get("approved"))
    else {
        return Ok(Poll::Pending);
    };

    let label: String = row.get("label");
    let id: Uuid = row.get("id");
    let (_, key) = crate::auth::create_key(pool, user_id, &label).await?;
    sqlx::query("UPDATE device_codes SET redeemed_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(Poll::Approved(key))
}

/// What the console shows a human before they approve.
pub struct Request {
    pub id: Uuid,
    pub label: String,
}

pub async fn lookup(pool: &PgPool, user_code: &str) -> sqlx::Result<Option<Request>> {
    Ok(sqlx::query(
        "SELECT id, label FROM device_codes
         WHERE user_code = $1 AND expires_at > now()
           AND approved_at IS NULL AND denied_at IS NULL AND redeemed_at IS NULL",
    )
    .bind(user_code.trim().to_uppercase())
    .fetch_optional(pool)
    .await?
    .map(|row| Request {
        id: row.get("id"),
        label: row.get("label"),
    }))
}

pub async fn decide(
    pool: &PgPool,
    user_code: &str,
    user_id: Uuid,
    approve: bool,
) -> sqlx::Result<bool> {
    // Two literal statements rather than a formatted column name: nothing
    // about a decision should be able to reach the query text.
    let result = if approve {
        sqlx::query(
            "UPDATE device_codes SET approved_at = now(), user_id = $2
             WHERE user_code = $1 AND expires_at > now()
               AND approved_at IS NULL AND denied_at IS NULL",
        )
    } else {
        sqlx::query(
            "UPDATE device_codes SET denied_at = now(), user_id = $2
             WHERE user_code = $1 AND expires_at > now()
               AND approved_at IS NULL AND denied_at IS NULL",
        )
    }
    .bind(user_code.trim().to_uppercase())
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}
