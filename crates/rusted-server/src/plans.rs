//! Versioned plans and the limits they grant. A subscription binds to a plan
//! *version*, so changing a plan never alters what existing subscribers bought.
//! Effective limits are cached per user and invalidated by
//! `subscription:<user_id>` on the NOTIFY channel.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

const CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanLimits {
    pub max_functions: i64,
    pub max_script_bytes: i64,
    pub exec_ms: u64,
    pub rate_per_min: i64,
    pub outbound_reqs: u32,
    pub analytics_days: i64,
}

impl Default for PlanLimits {
    /// The Dev plan's numbers, used when the database has no plans seeded.
    fn default() -> Self {
        Self {
            max_functions: 4,
            max_script_bytes: 262_144,
            exec_ms: 50,
            rate_per_min: 60,
            outbound_reqs: 0,
            analytics_days: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub id: Uuid,
    pub code: String,
    pub version: i32,
    pub name: String,
    pub price_cents: i32,
    pub limits: PlanLimits,
}

struct Entry {
    plan: Plan,
    at: Instant,
}

#[derive(Default)]
pub struct PlanCache {
    by_user: Mutex<HashMap<Uuid, Entry>>,
}

impl PlanCache {
    pub fn invalidate(&self, user_id: Uuid) {
        self.by_user.lock().unwrap().remove(&user_id);
    }

    pub fn clear(&self) {
        self.by_user.lock().unwrap().clear();
    }
}

fn plan_from_row(row: &sqlx::postgres::PgRow) -> Plan {
    let limits: serde_json::Value = row.get("limits");
    Plan {
        id: row.get("id"),
        code: row.get("code"),
        version: row.get("version"),
        name: row.get("name"),
        price_cents: row.get("price_cents"),
        limits: serde_json::from_value(limits).unwrap_or_default(),
    }
}

/// The plan a user is on: their active subscription's plan version, or the
/// newest Dev version.
pub async fn effective_plan(pool: &PgPool, cache: &PlanCache, user_id: Option<Uuid>) -> Plan {
    if let Some(user_id) = user_id {
        if let Some(entry) = cache.by_user.lock().unwrap().get(&user_id) {
            if entry.at.elapsed() < CACHE_TTL {
                return entry.plan.clone();
            }
        }
    }
    let row = match user_id {
        Some(user_id) => sqlx::query(
            "SELECT p.id, p.code, p.version, p.name, p.price_cents, p.limits
                 FROM subscriptions s JOIN plans p ON p.id = s.plan_id
                 WHERE s.user_id = $1 AND s.status = 'active'
                 ORDER BY s.started_at DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten(),
        None => None,
    };
    let row = match row {
        Some(row) => Some(row),
        None => sqlx::query(
            "SELECT id, code, version, name, price_cents, limits FROM plans
             WHERE code = 'dev' ORDER BY version DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .ok()
        .flatten(),
    };
    let plan = row.map(|row| plan_from_row(&row)).unwrap_or(Plan {
        id: Uuid::nil(),
        code: "dev".into(),
        version: 1,
        name: "Dev".into(),
        price_cents: 0,
        limits: PlanLimits::default(),
    });
    if let Some(user_id) = user_id {
        cache.by_user.lock().unwrap().insert(
            user_id,
            Entry {
                plan: plan.clone(),
                at: Instant::now(),
            },
        );
    }
    plan
}

/// Newest version of every plan, cheapest first — the billing page's catalog.
pub async fn catalog(pool: &PgPool) -> sqlx::Result<Vec<Plan>> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (code) id, code, version, name, price_cents, limits
         FROM plans ORDER BY code, version DESC",
    )
    .fetch_all(pool)
    .await?;
    let mut plans: Vec<Plan> = rows.iter().map(plan_from_row).collect();
    plans.sort_by_key(|p| p.price_cents);
    Ok(plans)
}

pub async fn latest_by_code(pool: &PgPool, code: &str) -> sqlx::Result<Option<Plan>> {
    Ok(sqlx::query(
        "SELECT id, code, version, name, price_cents, limits FROM plans
         WHERE code = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?
    .map(|row| plan_from_row(&row)))
}

/// Switches the user to `plan_id`, binding them to that exact version.
pub async fn subscribe(pool: &PgPool, user_id: Uuid, plan_id: Uuid) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE subscriptions SET status = 'canceled', canceled_at = now()
         WHERE user_id = $1 AND status = 'active'",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO subscriptions (user_id, plan_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(plan_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(crate::store::INVALIDATION_CHANNEL)
        .bind(format!("subscription:{user_id}"))
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Fixed-window per-minute rate limiter, keyed by function name.
#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, (u64, i64)>>,
}

impl RateLimiter {
    /// Returns Ok(()) or Err(seconds_until_reset).
    pub fn check(&self, key: &str, per_min: i64) -> Result<(), u64> {
        if per_min <= 0 {
            return Ok(());
        }
        let now = crate::state::now_epoch();
        let window = now / 60;
        let mut windows = self.windows.lock().unwrap();
        let entry = windows.entry(key.to_string()).or_insert((window, 0));
        if entry.0 != window {
            *entry = (window, 0);
        }
        if entry.1 >= per_min {
            return Err(60 - (now % 60));
        }
        entry.1 += 1;
        Ok(())
    }

    pub fn forget(&self, key: &str) {
        self.windows.lock().unwrap().remove(key);
    }
}
