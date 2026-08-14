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

/// Secrets one account may hold per environment. A bound, not a plan
/// feature: credentials are small and few, and an unbounded write path is
/// what this prevents.
pub const MAX_SECRETS_PER_USER: i64 = 64;

/// The default environment: always valid, never stored as a row, never
/// deletable. Existing behavior is exactly "everything is prod".
pub const PROD_ENV: &str = "prod";
/// What `rusted run` reports as `context.currentEnv`; reserved so a deployed
/// environment can never masquerade as local development.
pub const LOCAL_ENV: &str = "local";
/// Additional environments one account may create.
pub const MAX_ENVIRONMENTS: i64 = 8;

/// Environment names ride in URLs after `@`, so they share the function-name
/// charset, shorter.
pub fn valid_env_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Whether `env` is valid for this account: prod always, otherwise a row.
pub async fn env_exists(pool: &PgPool, user_id: Uuid, env: &str) -> bool {
    if env == PROD_ENV {
        return true;
    }
    sqlx::query("SELECT 1 FROM environments WHERE user_id = $1 AND name = $2")
        .bind(user_id)
        .bind(env)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .is_some()
}

/// Every environment this account can resolve, prod first.
pub async fn list_envs(pool: &PgPool, user_id: Uuid) -> Vec<String> {
    let mut envs = vec![PROD_ENV.to_string()];
    if let Ok(rows) = sqlx::query("SELECT name FROM environments WHERE user_id = $1 ORDER BY name")
        .bind(user_id)
        .fetch_all(pool)
        .await
    {
        use sqlx::Row as _;
        envs.extend(rows.iter().map(|row| row.get::<String, _>("name")));
    }
    envs
}

pub async fn create_env(pool: &PgPool, user_id: Uuid, name: &str) -> Result<(), String> {
    if name == PROD_ENV {
        return Err("prod always exists".to_string());
    }
    if name == LOCAL_ENV {
        return Err("'local' is reserved for rusted run".to_string());
    }
    if !valid_env_name(name) {
        return Err("environment names are 1-32 chars of a-z, 0-9, '-', '_'".to_string());
    }
    let held: i64 = sqlx::query("SELECT count(*) AS n FROM environments WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())
        .map(|row| {
            use sqlx::Row as _;
            row.get("n")
        })?;
    if held >= MAX_ENVIRONMENTS {
        return Err(format!(
            "this account already has {MAX_ENVIRONMENTS} environments besides prod"
        ));
    }
    sqlx::query("INSERT INTO environments (user_id, name) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(user_id)
        .bind(name)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Removes the environment and every secret stored under it. Durable state
/// keyed to it stays, like function state generally — purge is explicit.
pub async fn delete_env(pool: &PgPool, user_id: Uuid, name: &str) -> Result<bool, String> {
    if name == PROD_ENV {
        return Err("prod cannot be deleted".to_string());
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let removed = sqlx::query("DELETE FROM environments WHERE user_id = $1 AND name = $2")
        .bind(user_id)
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();
    if removed == 0 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM secrets WHERE user_id = $1 AND env = $2")
        .bind(user_id)
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(INVALIDATION_CHANNEL)
        .bind(format!("secret:{user_id}:{name}"))
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(true)
}

/// A secret is a credential, not a document. Large enough for a PEM key.
pub const MAX_VALUE_BYTES: usize = 8 * 1024;

/// AES-GCM standard nonce length; each sealed blob is nonce || ciphertext+tag.
const NONCE_LEN: usize = 12;

/// One environment's decrypted secrets, shared by reference from the cache.
type DecryptedEnv = Arc<BTreeMap<String, String>>;

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
    /// (user, env) → decrypted name→value map; the invocation-path read.
    /// Decrypted values in process memory are acceptable — they are handed to
    /// handlers anyway — while the database only ever sees ciphertext.
    cache: Mutex<HashMap<(Uuid, String), DecryptedEnv>>,
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

    pub fn invalidate(&self, user_id: Uuid, env: &str) {
        self.cache
            .lock()
            .unwrap()
            .remove(&(user_id, env.to_string()));
    }

    pub fn invalidate_all(&self) {
        self.cache.lock().unwrap().clear();
    }

    /// Names and timestamps only — a stored value never travels back out
    /// except into `context.env`.
    pub async fn list(&self, user_id: Uuid, env: &str) -> sqlx::Result<Vec<SecretMeta>> {
        let rows = sqlx::query(
            "SELECT name,
                    extract(epoch FROM created_at)::bigint AS created,
                    extract(epoch FROM updated_at)::bigint AS updated
             FROM secrets WHERE user_id = $1 AND env = $2 ORDER BY name",
        )
        .bind(user_id)
        .bind(env)
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
    pub async fn set(
        &self,
        user_id: Uuid,
        env: &str,
        name: &str,
        value: &str,
    ) -> Result<(), String> {
        let Some(cipher) = &self.cipher else {
            return Err(DISABLED.to_string());
        };
        if !env_exists(&self.pool, user_id, env).await {
            return Err(format!("no environment named {env}"));
        }
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
        let held: i64 = sqlx::query(
            "SELECT count(*) AS n FROM secrets WHERE user_id = $1 AND env = $2 AND name <> $3",
        )
        .bind(user_id)
        .bind(env)
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
            "INSERT INTO secrets (user_id, env, name, ciphertext) VALUES ($1, $2, $3, $4)
             ON CONFLICT (user_id, env, name) DO UPDATE
                 SET ciphertext = $4, updated_at = now()",
        )
        .bind(user_id)
        .bind(env)
        .bind(name)
        .bind(cipher.seal(value))
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        self.notify(user_id, env).await;
        Ok(())
    }

    pub async fn delete(&self, user_id: Uuid, env: &str, name: &str) -> Result<bool, String> {
        let result =
            sqlx::query("DELETE FROM secrets WHERE user_id = $1 AND env = $2 AND name = $3")
                .bind(user_id)
                .bind(env)
                .bind(name)
                .execute(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        self.notify(user_id, env).await;
        Ok(true)
    }

    /// Evicts every server's cache for this (user, env), ours included.
    async fn notify(&self, user_id: Uuid, env: &str) {
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(INVALIDATION_CHANNEL)
            .bind(format!("secret:{user_id}:{env}"))
            .execute(&self.pool)
            .await;
        self.invalidate(user_id, env);
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
        env: &str,
        names: &[String],
    ) -> Result<BTreeMap<String, String>, String> {
        if self.cipher.is_none() {
            return Err(DISABLED.to_string());
        }
        let all = self.all_for(user_id, env).await?;
        let mut resolved = BTreeMap::new();
        let mut missing = Vec::new();
        for name in names {
            match all.get(name) {
                Some(value) => {
                    resolved.insert(name.clone(), value.clone());
                }
                None => missing.push(name.as_str()),
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "secrets not set in the {env} environment: {} — add them in the console under Secrets",
                missing.join(", ")
            ));
        }
        Ok(resolved)
    }

    /// Every secret the user holds in one environment, decrypted, cached.
    async fn all_for(
        &self,
        user_id: Uuid,
        env: &str,
    ) -> Result<Arc<BTreeMap<String, String>>, String> {
        if let Some(hit) = self.cache.lock().unwrap().get(&(user_id, env.to_string())) {
            return Ok(hit.clone());
        }
        let cipher = self.cipher.as_ref().expect("checked by callers");
        let rows =
            sqlx::query("SELECT name, ciphertext FROM secrets WHERE user_id = $1 AND env = $2")
                .bind(user_id)
                .bind(env)
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
        self.cache
            .lock()
            .unwrap()
            .insert((user_id, env.to_string()), all.clone());
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

// ------------------------------------------------------------ seal / open
//
// `context.seal` / `context.open`: authenticated encryption performed host-
// side, keyed by one of the owner's vault secrets. The key material never
// enters JavaScript — a strict improvement over handlers decrypting cookies
// with a key read from `context.env`, and native speed instead of an
// interpreted cipher on every request.

/// Serialized payloads above this are refused — a sealed value is a cookie or
/// token, not a document.
pub const MAX_SEAL_BYTES: usize = 16 * 1024;

/// Envelope version, first byte of every sealed blob. Bump on any format
/// change so old seals fail closed instead of decrypting wrongly.
const SEAL_VERSION: u8 = 1;

/// One `context.seal`/`context.open` request off the wire. `context` is the
/// associated-data string: a seal opens only under the same one, so a value
/// sealed for one purpose cannot be replayed into another.
#[derive(serde::Deserialize)]
#[serde(tag = "op")]
pub enum SealOp {
    #[serde(rename = "seal", rename_all = "camelCase")]
    Seal {
        payload: serde_json::Value,
        key_secret: String,
        #[serde(default)]
        context: String,
    },
    #[serde(rename = "open", rename_all = "camelCase")]
    Open {
        sealed: String,
        key_secret: String,
        #[serde(default)]
        context: String,
    },
}

impl SealOp {
    pub fn parse(op_json: &str) -> Result<SealOp, String> {
        let op: SealOp = serde_json::from_str(op_json).map_err(|e| format!("malformed op: {e}"))?;
        let key = match &op {
            SealOp::Seal { key_secret, .. } | SealOp::Open { key_secret, .. } => key_secret,
        };
        if !rusted_engine::valid_secret_name(key) {
            return Err("keySecret must name a vault secret".to_string());
        }
        Ok(op)
    }

    pub fn key_secret(&self) -> &str {
        match self {
            SealOp::Seal { key_secret, .. } | SealOp::Open { key_secret, .. } => key_secret,
        }
    }

    /// Runs the op with the resolved key material. Deriving the AES key as
    /// SHA-256 of the material means any strong vault secret works — no
    /// format requirement on the stored value.
    pub fn perform(&self, key_material: &str) -> Result<String, String> {
        use aes_gcm::aead::Payload;
        use base64::Engine as _;
        use sha2::Digest as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let key = sha2::Sha256::digest(key_material.as_bytes());
        let cipher = Aes256Gcm::new_from_slice(&key).expect("32 bytes is a valid AES-256 key");
        match self {
            SealOp::Seal {
                payload, context, ..
            } => {
                let plaintext = serde_json::to_string(payload).map_err(|e| e.to_string())?;
                if plaintext.len() > MAX_SEAL_BYTES {
                    return Err(format!(
                        "payload is {} bytes serialized; seal caps at {MAX_SEAL_BYTES}",
                        plaintext.len()
                    ));
                }
                let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
                let sealed = cipher
                    .encrypt(
                        &nonce,
                        Payload {
                            msg: plaintext.as_bytes(),
                            aad: context.as_bytes(),
                        },
                    )
                    .expect("aes-gcm encryption is infallible for in-memory data");
                let mut blob = vec![SEAL_VERSION];
                blob.extend(nonce);
                blob.extend(sealed);
                Ok(serde_json::json!({ "sealed": b64.encode(blob) }).to_string())
            }
            SealOp::Open {
                sealed, context, ..
            } => {
                // Every failure mode — bad encoding, wrong version, wrong key,
                // wrong context, tampering — is the same null answer: a forger
                // learns nothing about which check refused them.
                let invalid = serde_json::json!({ "valid": false, "payload": null }).to_string();
                let Ok(blob) = b64.decode(sealed.as_bytes()) else {
                    return Ok(invalid);
                };
                if blob.len() <= 1 + 12 || blob[0] != SEAL_VERSION {
                    return Ok(invalid);
                }
                let (nonce, ciphertext) = blob[1..].split_at(12);
                let Ok(plaintext) = cipher.decrypt(
                    Nonce::from_slice(nonce),
                    Payload {
                        msg: ciphertext,
                        aad: context.as_bytes(),
                    },
                ) else {
                    return Ok(invalid);
                };
                let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&plaintext) else {
                    return Ok(invalid);
                };
                Ok(serde_json::json!({ "valid": true, "payload": payload }).to_string())
            }
        }
    }
}

#[cfg(test)]
mod seal_tests {
    use super::*;
    use serde_json::json;

    fn run(op: serde_json::Value, material: &str) -> serde_json::Value {
        let parsed = SealOp::parse(&op.to_string()).unwrap();
        serde_json::from_str(&parsed.perform(material).unwrap()).unwrap()
    }

    #[test]
    fn seal_round_trips_and_fails_closed() {
        let sealed = run(
            json!({"op":"seal","payload":{"userId":7},"keySecret":"COOKIE_KEY","context":"auth:v1"}),
            "material",
        )["sealed"]
            .as_str()
            .unwrap()
            .to_string();
        // URL/cookie-safe, and never the plaintext.
        assert!(sealed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(!sealed.contains("userId"));

        let opened = run(
            json!({"op":"open","sealed":sealed,"keySecret":"COOKIE_KEY","context":"auth:v1"}),
            "material",
        );
        assert_eq!(opened["valid"], true);
        assert_eq!(opened["payload"]["userId"], 7);

        // Wrong key, wrong context, tampering: all the same null, no detail.
        for (material, context, tamper) in [
            ("other-material", "auth:v1", false),
            ("material", "other-context", false),
            ("material", "auth:v1", true),
        ] {
            let mut value = sealed.clone();
            if tamper {
                let flipped = if value.ends_with('A') { 'B' } else { 'A' };
                value.pop();
                value.push(flipped);
            }
            let refused = run(
                json!({"op":"open","sealed":value,"keySecret":"COOKIE_KEY","context":context}),
                material,
            );
            assert_eq!(refused["valid"], false, "{material} {context} {tamper}");
        }
    }

    #[test]
    fn seal_refuses_oversize_and_bad_key_names() {
        let big = "x".repeat(MAX_SEAL_BYTES + 1);
        let parsed =
            SealOp::parse(&json!({"op":"seal","payload":big,"keySecret":"K"}).to_string()).unwrap();
        assert!(parsed.perform("m").unwrap_err().contains("caps"));
        assert!(
            SealOp::parse(&json!({"op":"seal","payload":1,"keySecret":"lower"}).to_string())
                .is_err()
        );
    }
}
