//! Encrypted per-account secrets, decrypted into `context.env` for functions
//! that request them via `export const config = { secrets: [...] }`.
//!
//! Values are sealed with AES-256-GCM under a key from the server environment
//! (`RUSTED_SECRETS_KEY`, 64 hex chars) before they touch Postgres, so the
//! database never holds a plaintext credential and a dump of the table alone
//! reveals nothing. Without the key the store is disabled: setting secrets is
//! refused with instructions, and invoking a function that requests them fails
//! saying so rather than running with an empty environment.
//!
//! Reads go through a per-user in-memory cache of decrypted values —
//! the invocation path must not query Postgres — invalidated over the same
//! LISTEN/NOTIFY channel the other caches use (`secret:<user_id>`).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

use crate::store::INVALIDATION_CHANNEL;

/// Server environment variable holding the 32-byte master key, hex-encoded.
/// Generate one with `openssl rand -hex 32`.
pub const KEY_ENV: &str = "RUSTED_SECRETS_KEY";

/// Secrets one account may hold. A bound, not a plan feature: credentials are
/// small and few, and an unbounded write path is what this prevents.
pub const MAX_SECRETS_PER_USER: i64 = 64;

/// A secret is a credential, not a document. Large enough for a PEM key.
pub const MAX_VALUE_BYTES: usize = 8 * 1024;

/// AES-GCM standard nonce length; each sealed blob is nonce || ciphertext+tag.
const NONCE_LEN: usize = 12;

/// What the console lists: everything about a secret except its value.
pub struct SecretMeta {
    pub name: String,
    pub created_at: u64,
    pub updated_at: u64,
}

struct Cipher(Aes256Gcm);

impl Cipher {
    fn seal(&self, plaintext: &str) -> Vec<u8> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let sealed = self
            .0
            .encrypt(&nonce, plaintext.as_bytes())
            .expect("aes-gcm encryption is infallible for in-memory data");
        let mut blob = nonce.to_vec();
        blob.extend(sealed);
        blob
    }

    /// Fails on a truncated blob or a key that is not the one that sealed it —
    /// which is what rotating `RUSTED_SECRETS_KEY` without re-entering the
    /// secrets looks like.
    fn open(&self, blob: &[u8]) -> Result<String, String> {
        if blob.len() <= NONCE_LEN {
            return Err("stored secret is truncated".to_string());
        }
        let (nonce, sealed) = blob.split_at(NONCE_LEN);
        let plaintext = self
            .0
            .decrypt(Nonce::from_slice(nonce), sealed)
            .map_err(|_| {
                "stored secret cannot be decrypted — was RUSTED_SECRETS_KEY changed? \
                 Re-enter the secret in the console"
                    .to_string()
            })?;
        String::from_utf8(plaintext).map_err(|_| "stored secret is not UTF-8".to_string())
    }
}

pub struct SecretStore {
    pool: PgPool,
    /// `None` when the server has no master key: the store exists but refuses.
    cipher: Option<Cipher>,
    /// user → decrypted name→value map; the invocation-path read. Decrypted
    /// values in process memory are acceptable — they are handed to handlers
    /// anyway — while the database only ever sees ciphertext.
    cache: Mutex<HashMap<Uuid, Arc<BTreeMap<String, String>>>>,
}

/// The one message every disabled-store path shows.
const DISABLED: &str = "this server has no secret store: set RUSTED_SECRETS_KEY in the server \
                        environment (64 hex chars — `openssl rand -hex 32`) and restart";

impl SecretStore {
    /// Reads the master key from the environment. A missing key disables the
    /// store; a malformed one is a configuration bug worth stopping on, not
    /// silently running without encryption for.
    pub fn new(pool: PgPool) -> SecretStore {
        let cipher = std::env::var(KEY_ENV).ok().map(|raw| {
            let bytes = hex::decode(raw.trim())
                .ok()
                .filter(|b| b.len() == 32)
                .unwrap_or_else(|| panic!("{KEY_ENV} must be 64 hex chars (openssl rand -hex 32)"));
            Cipher(Aes256Gcm::new_from_slice(&bytes).expect("32 bytes is a valid AES-256 key"))
        });
        SecretStore {
            pool,
            cipher,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cipher.is_some()
    }

    pub fn invalidate(&self, user_id: Uuid) {
        self.cache.lock().unwrap().remove(&user_id);
    }

    pub fn invalidate_all(&self) {
        self.cache.lock().unwrap().clear();
    }

    /// Names and timestamps only — a stored value never travels back out
    /// except into `context.env`.
    pub async fn list(&self, user_id: Uuid) -> sqlx::Result<Vec<SecretMeta>> {
        let rows = sqlx::query(
            "SELECT name,
                    extract(epoch FROM created_at)::bigint AS created,
                    extract(epoch FROM updated_at)::bigint AS updated
             FROM secrets WHERE user_id = $1 ORDER BY name",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| SecretMeta {
                name: row.get("name"),
                created_at: row.get::<i64, _>("created").max(0) as u64,
                updated_at: row.get::<i64, _>("updated").max(0) as u64,
            })
            .collect())
    }

    /// Creates or replaces one secret. The value is sealed before the query is
    /// built, so plaintext never leaves this process.
    pub async fn set(&self, user_id: Uuid, name: &str, value: &str) -> Result<(), String> {
        let Some(cipher) = &self.cipher else {
            return Err(DISABLED.to_string());
        };
        if !rusted_engine::valid_secret_name(name) {
            return Err(
                "secret names are 1-64 chars of A-Z, 0-9, '_', not starting with a digit \
                 (e.g. GITHUB_CLIENT_SECRET)"
                    .to_string(),
            );
        }
        if value.is_empty() {
            return Err("the secret value is empty".to_string());
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(format!(
                "secret value is {} bytes; the limit is {MAX_VALUE_BYTES}",
                value.len()
            ));
        }
        let held: i64 =
            sqlx::query("SELECT count(*) AS n FROM secrets WHERE user_id = $1 AND name <> $2")
                .bind(user_id)
                .bind(name)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| e.to_string())?
                .get("n");
        if held >= MAX_SECRETS_PER_USER {
            return Err(format!(
                "this account already holds {MAX_SECRETS_PER_USER} secrets — delete one first"
            ));
        }
        sqlx::query(
            "INSERT INTO secrets (user_id, name, ciphertext) VALUES ($1, $2, $3)
             ON CONFLICT (user_id, name) DO UPDATE
                 SET ciphertext = $3, updated_at = now()",
        )
        .bind(user_id)
        .bind(name)
        .bind(cipher.seal(value))
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        self.notify(user_id).await;
        Ok(())
    }

    pub async fn delete(&self, user_id: Uuid, name: &str) -> Result<bool, String> {
        let result = sqlx::query("DELETE FROM secrets WHERE user_id = $1 AND name = $2")
            .bind(user_id)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        self.notify(user_id).await;
        Ok(true)
    }

    /// Evicts every server's cache for this user, ours included.
    async fn notify(&self, user_id: Uuid) {
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(INVALIDATION_CHANNEL)
            .bind(format!("secret:{user_id}"))
            .execute(&self.pool)
            .await;
        self.invalidate(user_id);
    }

    /// The environment a function asked for: exactly `names`, decrypted.
    ///
    /// All-or-nothing on purpose: a handler exchanging an OAuth code with half
    /// its credentials produces a confusing upstream error, while "GITHUB_
    /// CLIENT_SECRET is not set" names the fix. Served through the per-user
    /// cache, so a warm invocation never touches Postgres.
    pub async fn env_for(
        &self,
        user_id: Uuid,
        names: &[String],
    ) -> Result<BTreeMap<String, String>, String> {
        if self.cipher.is_none() {
            return Err(DISABLED.to_string());
        }
        let all = self.all_for(user_id).await?;
        let mut env = BTreeMap::new();
        let mut missing = Vec::new();
        for name in names {
            match all.get(name) {
                Some(value) => {
                    env.insert(name.clone(), value.clone());
                }
                None => missing.push(name.as_str()),
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "secrets not set for this account: {} — add them in the console under Secrets",
                missing.join(", ")
            ));
        }
        Ok(env)
    }

    /// Every secret the user holds, decrypted, through the cache.
    async fn all_for(&self, user_id: Uuid) -> Result<Arc<BTreeMap<String, String>>, String> {
        if let Some(hit) = self.cache.lock().unwrap().get(&user_id) {
            return Ok(hit.clone());
        }
        let cipher = self.cipher.as_ref().expect("checked by callers");
        let rows = sqlx::query("SELECT name, ciphertext FROM secrets WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let mut all = BTreeMap::new();
        for row in &rows {
            let name: String = row.get("name");
            let value = cipher
                .open(&row.get::<Vec<u8>, _>("ciphertext"))
                .map_err(|e| format!("secret {name}: {e}"))?;
            all.insert(name, value);
        }
        let all = Arc::new(all);
        self.cache.lock().unwrap().insert(user_id, all.clone());
        Ok(all)
    }
}

#[cfg(test)]
mod cipher_tests {
    use super::*;

    fn cipher() -> Cipher {
        Cipher(Aes256Gcm::new_from_slice(&[7u8; 32]).unwrap())
    }

    #[test]
    fn seal_and_open_round_trip() {
        let c = cipher();
        let blob = c.seal("hunter2");
        assert_ne!(blob, b"hunter2", "value must not be stored in the clear");
        assert_eq!(c.open(&blob).unwrap(), "hunter2");
        // A fresh nonce per seal: identical plaintexts must not produce
        // identical ciphertexts, or the table leaks equality.
        assert_ne!(c.seal("hunter2"), blob);
    }

    #[test]
    fn open_refuses_wrong_key_and_garbage() {
        let blob = cipher().seal("hunter2");
        let other = Cipher(Aes256Gcm::new_from_slice(&[8u8; 32]).unwrap());
        assert!(other.open(&blob).is_err(), "wrong key must not decrypt");
        assert!(cipher().open(&blob[..5]).is_err(), "truncated blob");
        let mut tampered = blob.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(cipher().open(&tampered).is_err(), "tampered blob");
    }
}
