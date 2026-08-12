//! Durable JSON state for functions (`context.state`).
//!
//! Scoped by (owner, stable function name); persists across revisions and
//! delete/redeploy. Single-key compare-and-set is the transaction boundary:
//! CAS and delete are single SQL statements whose version check happens in the
//! database, never read-then-write in application code. A per-function
//! advisory lock serializes writes so the plan's key/byte accounting — checked
//! inside the same statement — cannot be raced past.
//!
//! Reads go through a small in-memory cache invalidated over the shared
//! LISTEN/NOTIFY channel (`fnstate:<user_id>:<function_name>`). Correctness
//! never depends on the cache: every write's version check runs in Postgres
//! against the real row.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

use crate::store::INVALIDATION_CHANNEL;

pub const MAX_KEY_BYTES: usize = 512;
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_LIST_LIMIT: i64 = 100;

/// Bound on cached entries across all functions; beyond it the cache is
/// dropped wholesale rather than growing without limit.
const CACHE_ENTRY_CAP: usize = 4096;

/// Per-function allowances, handed in by the caller from the owner's plan —
/// this layer stores no product-tier numbers of its own. The default is zero
/// allowance: an invocation that was never granted state can write none.
#[derive(Debug, Default, Clone, Copy)]
pub struct StateAllowance {
    pub max_keys: i64,
    pub max_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub key: String,
    pub value: serde_json::Value,
    pub version: i64,
}

/// A compare-and-set that did not apply, and why.
#[derive(Debug, PartialEq)]
pub enum CasOutcome {
    Applied {
        version: i64,
    },
    /// The stored version was not what the caller expected (`None`: no row).
    Conflict {
        current_version: Option<i64>,
    },
}

pub struct StateStore {
    pool: PgPool,
    /// (owner, function) → key → entry; a read cache only.
    cache: Mutex<HashMap<(Uuid, String), HashMap<String, Entry>>>,
    /// Total cached entries, so the bound is O(1) to check.
    cached_entries: Mutex<usize>,
}

fn vet_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(format!("state keys are 1-{MAX_KEY_BYTES} bytes of UTF-8"));
    }
    Ok(())
}

impl StateStore {
    pub fn new(pool: PgPool) -> StateStore {
        StateStore {
            pool,
            cache: Mutex::new(HashMap::new()),
            cached_entries: Mutex::new(0),
        }
    }

    pub fn invalidate(&self, user_id: Uuid, function_name: &str) {
        let mut cache = self.cache.lock().unwrap();
        if let Some(entries) = cache.remove(&(user_id, function_name.to_string())) {
            let mut count = self.cached_entries.lock().unwrap();
            *count = count.saturating_sub(entries.len());
        }
    }

    pub fn invalidate_all(&self) {
        self.cache.lock().unwrap().clear();
        *self.cached_entries.lock().unwrap() = 0;
    }

    pub async fn get(
        &self,
        user_id: Uuid,
        function_name: &str,
        key: &str,
    ) -> Result<Option<Entry>, String> {
        vet_key(key)?;
        {
            let cache = self.cache.lock().unwrap();
            if let Some(entries) = cache.get(&(user_id, function_name.to_string())) {
                if let Some(entry) = entries.get(key) {
                    return Ok(Some(entry.clone()));
                }
            }
        }
        let row = sqlx::query(
            "SELECT value, version FROM function_state
             WHERE user_id = $1 AND function_name = $2 AND key = $3",
        )
        .bind(user_id)
        .bind(function_name)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let Some(row) = row else { return Ok(None) };
        let entry = Entry {
            key: key.to_string(),
            value: row.get("value"),
            version: row.get("version"),
        };
        // Negative results are not cached: absence is cheap to re-check and
        // caching it would make create-after-miss look racy for no gain.
        let mut cache = self.cache.lock().unwrap();
        let mut count = self.cached_entries.lock().unwrap();
        if *count >= CACHE_ENTRY_CAP {
            cache.clear();
            *count = 0;
        }
        cache
            .entry((user_id, function_name.to_string()))
            .or_default()
            .insert(key.to_string(), entry.clone());
        *count += 1;
        Ok(Some(entry))
    }

    /// Applies `value` only if the stored version is exactly `expected`
    /// (`None`: create, failing if the key exists). The version check and the
    /// plan accounting both run inside the SQL statement, under a
    /// per-function advisory lock, so a refused write changes nothing.
    pub async fn compare_and_set(
        &self,
        user_id: Uuid,
        function_name: &str,
        key: &str,
        expected: Option<i64>,
        value: &serde_json::Value,
        allowance: StateAllowance,
    ) -> Result<CasOutcome, String> {
        vet_key(key)?;
        let serialized = serde_json::to_string(value).map_err(|e| e.to_string())?;
        if serialized.len() > MAX_VALUE_BYTES {
            return Err(format!(
                "state value is {} bytes serialized; the limit is {MAX_VALUE_BYTES}",
                serialized.len()
            ));
        }
        let bytes = serialized.len() as i32;
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        lock_function(&mut tx, user_id, function_name).await?;

        let applied: Option<i64> = match expected {
            None => sqlx::query(
                "INSERT INTO function_state (user_id, function_name, key, value, bytes)
                 SELECT $1, $2, $3, $4, $5
                 WHERE NOT EXISTS (
                         SELECT 1 FROM function_state
                         WHERE user_id = $1 AND function_name = $2 AND key = $3)
                   AND (SELECT count(*) FROM function_state
                        WHERE user_id = $1 AND function_name = $2) < $6
                   AND (SELECT coalesce(sum(bytes), 0) FROM function_state
                        WHERE user_id = $1 AND function_name = $2) + $5 <= $7
                 RETURNING version",
            )
            .bind(user_id)
            .bind(function_name)
            .bind(key)
            .bind(value)
            .bind(bytes)
            .bind(allowance.max_keys)
            .bind(allowance.max_bytes)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            .map(|row| row.get("version")),
            Some(expected) => sqlx::query(
                "UPDATE function_state
                 SET value = $4, bytes = $5, version = version + 1, updated_at = now()
                 WHERE user_id = $1 AND function_name = $2 AND key = $3 AND version = $6
                   AND (SELECT coalesce(sum(bytes), 0) FROM function_state
                        WHERE user_id = $1 AND function_name = $2 AND key <> $3) + $5 <= $7
                 RETURNING version",
            )
            .bind(user_id)
            .bind(function_name)
            .bind(key)
            .bind(value)
            .bind(bytes)
            .bind(expected)
            .bind(allowance.max_bytes)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            .map(|row| row.get("version")),
        };

        if let Some(version) = applied {
            notify(&mut tx, user_id, function_name).await?;
            tx.commit().await.map_err(|e| e.to_string())?;
            self.invalidate(user_id, function_name);
            return Ok(CasOutcome::Applied { version });
        }

        // Nothing was written. Name the reason: a version conflict is an
        // outcome the caller handles, a plan limit is an error that names the
        // fix. Read under the same lock, so the answer is the one that held.
        let current: Option<i64> = sqlx::query(
            "SELECT version FROM function_state
             WHERE user_id = $1 AND function_name = $2 AND key = $3",
        )
        .bind(user_id)
        .bind(function_name)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .map(|row| row.get("version"));
        let conflicted = match (expected, current) {
            (None, Some(_)) => true,
            (Some(expected), current) => current != Some(expected),
            (None, None) => false,
        };
        if conflicted {
            tx.commit().await.map_err(|e| e.to_string())?;
            return Ok(CasOutcome::Conflict {
                current_version: current,
            });
        }
        // The version matched (or the key was creatable), so a limit refused
        // it. Say which.
        let (keys, total_bytes): (i64, i64) = {
            let row = sqlx::query(
                "SELECT count(*) AS keys, coalesce(sum(bytes), 0) AS bytes
                 FROM function_state WHERE user_id = $1 AND function_name = $2",
            )
            .bind(user_id)
            .bind(function_name)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
            (row.get("keys"), row.get("bytes"))
        };
        drop(tx);
        if expected.is_none() && keys >= allowance.max_keys {
            return Err(format!(
                "state limit: this function already holds {keys} keys (plan allows {})",
                allowance.max_keys
            ));
        }
        Err(format!(
            "state limit: writing this value would exceed the plan's {} bytes of state \
             (currently {total_bytes})",
            allowance.max_bytes
        ))
    }

    /// Deletes only if the stored version is exactly `expected` — the same
    /// single-statement discipline as CAS.
    pub async fn delete(
        &self,
        user_id: Uuid,
        function_name: &str,
        key: &str,
        expected: i64,
    ) -> Result<CasOutcome, String> {
        vet_key(key)?;
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        lock_function(&mut tx, user_id, function_name).await?;
        let deleted = sqlx::query(
            "DELETE FROM function_state
             WHERE user_id = $1 AND function_name = $2 AND key = $3 AND version = $4",
        )
        .bind(user_id)
        .bind(function_name)
        .bind(key)
        .bind(expected)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();
        if deleted > 0 {
            notify(&mut tx, user_id, function_name).await?;
            tx.commit().await.map_err(|e| e.to_string())?;
            self.invalidate(user_id, function_name);
            return Ok(CasOutcome::Applied { version: expected });
        }
        let current: Option<i64> = sqlx::query(
            "SELECT version FROM function_state
             WHERE user_id = $1 AND function_name = $2 AND key = $3",
        )
        .bind(user_id)
        .bind(function_name)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .map(|row| row.get("version"));
        Ok(CasOutcome::Conflict {
            current_version: current,
        })
    }

    /// Lexicographically ordered page of entries. `cursor` is the last key of
    /// the previous page; the returned cursor is present only when more exist.
    pub async fn list(
        &self,
        user_id: Uuid,
        function_name: &str,
        prefix: &str,
        cursor: &str,
        limit: i64,
    ) -> Result<(Vec<Entry>, Option<String>), String> {
        let limit = limit.clamp(1, MAX_LIST_LIMIT);
        let rows = sqlx::query(
            "SELECT key, value, version FROM function_state
             WHERE user_id = $1 AND function_name = $2
               AND ($3 = '' OR starts_with(key, $3))
               AND key > $4
             ORDER BY key
             LIMIT $5",
        )
        .bind(user_id)
        .bind(function_name)
        .bind(prefix)
        .bind(cursor)
        .bind(limit + 1)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let mut entries: Vec<Entry> = rows
            .iter()
            .map(|row| Entry {
                key: row.get("key"),
                value: row.get("value"),
                version: row.get("version"),
            })
            .collect();
        let next = if entries.len() as i64 > limit {
            entries.truncate(limit as usize);
            entries.last().map(|entry| entry.key.clone())
        } else {
            None
        };
        Ok((entries, next))
    }

    /// The explicit, separate deletion path: removes every key this function
    /// holds. Never called by the runtime — only the admin API / CLI.
    pub async fn purge(&self, user_id: Uuid, function_name: &str) -> Result<u64, String> {
        let removed =
            sqlx::query("DELETE FROM function_state WHERE user_id = $1 AND function_name = $2")
                .bind(user_id)
                .bind(function_name)
                .execute(&self.pool)
                .await
                .map_err(|e| e.to_string())?
                .rows_affected();
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(INVALIDATION_CHANNEL)
            .bind(format!("fnstate:{user_id}:{function_name}"))
            .execute(&self.pool)
            .await;
        self.invalidate(user_id, function_name);
        Ok(removed)
    }
}

/// Serializes writers per (owner, function) for the transaction's lifetime.
/// This is what makes the in-statement accounting exact: two concurrent
/// creates cannot both observe "one slot left".
async fn lock_function(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    function_name: &str,
) -> Result<(), String> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 42))")
        .bind(format!("{user_id}/{function_name}"))
        .execute(&mut **tx)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn notify(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    function_name: &str,
) -> Result<(), String> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(INVALIDATION_CHANNEL)
        .bind(format!("fnstate:{user_id}:{function_name}"))
        .execute(&mut **tx)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
