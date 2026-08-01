//! Invocation analytics. The request path only ever pushes onto an in-memory
//! queue — never awaits a write — so a slow or unreachable database can shed
//! metrics but can never slow down or fail a function call.

use std::time::Duration;

use serde::Serialize;
use sqlx::postgres::PgPool;
use sqlx::Row;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Dropped records are counted, not silently lost.
static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bounded so a database outage costs memory we chose, not memory we didn't.
const QUEUE_CAPACITY: usize = 8192;
const BATCH_SIZE: usize = 256;
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const RETENTION_SWEEP: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub struct Invocation {
    pub function_name: String,
    pub user_id: Option<Uuid>,
    pub outcome: String,
    pub detail: Option<String>,
    pub wall_ms: f64,
    pub cpu_ms: f64,
    pub exec_ms: f64,
}

#[derive(Clone)]
pub struct Recorder {
    tx: mpsc::Sender<Invocation>,
}

impl Recorder {
    /// Non-blocking by construction: `try_send` never awaits, and a full queue
    /// drops the record rather than delaying the caller.
    pub fn record(&self, invocation: Invocation) {
        if self.tx.try_send(invocation).is_err() {
            DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn dropped() -> u64 {
        DROPPED.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Starts the writer and returns the handle the request path uses.
pub fn start(pool: PgPool) -> (Recorder, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let writer = tokio::spawn(write_loop(pool, rx));
    (Recorder { tx }, writer)
}

/// Batches queued records into multi-row inserts. Failures are dropped: an
/// invocation already succeeded or failed on its own merits, and retrying
/// metrics forever would trade correctness for a growing backlog.
async fn write_loop(pool: PgPool, mut rx: mpsc::Receiver<Invocation>) {
    let mut batch: Vec<Invocation> = Vec::with_capacity(BATCH_SIZE);
    loop {
        let deadline = tokio::time::sleep(FLUSH_INTERVAL);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                received = rx.recv() => match received {
                    Some(invocation) => {
                        batch.push(invocation);
                        if batch.len() >= BATCH_SIZE {
                            break;
                        }
                    }
                    // Channel closed: flush what's left and stop.
                    None => {
                        flush(&pool, &mut batch).await;
                        return;
                    }
                },
                _ = &mut deadline => break,
            }
        }
        flush(&pool, &mut batch).await;
    }
}

async fn flush(pool: &PgPool, batch: &mut Vec<Invocation>) {
    if batch.is_empty() {
        return;
    }
    let mut query = sqlx::QueryBuilder::new(
        "INSERT INTO invocations (function_name, user_id, outcome, detail, wall_ms, cpu_ms, exec_ms) ",
    );
    query.push_values(batch.iter(), |mut row, invocation| {
        row.push_bind(invocation.function_name.clone())
            .push_bind(invocation.user_id)
            .push_bind(invocation.outcome.clone())
            .push_bind(invocation.detail.clone())
            .push_bind(invocation.wall_ms)
            .push_bind(invocation.cpu_ms)
            .push_bind(invocation.exec_ms);
    });
    if let Err(e) = query.build().execute(pool).await {
        DROPPED.fetch_add(batch.len() as u64, std::sync::atomic::Ordering::Relaxed);
        eprintln!("rusted: dropped {} analytics rows: {e}", batch.len());
    }
    batch.clear();
}

/// Deletes each user's invocations past their plan's retention window, plus
/// any ownerless rows past the shortest window on offer.
pub async fn retention_sweeper(pool: PgPool) {
    loop {
        tokio::time::sleep(RETENTION_SWEEP).await;
        let _ = sqlx::query(
            "DELETE FROM invocations i
             USING users u
             LEFT JOIN LATERAL (
                 SELECT p.limits FROM subscriptions s JOIN plans p ON p.id = s.plan_id
                 WHERE s.user_id = u.id AND s.status = 'active'
                 ORDER BY s.started_at DESC LIMIT 1
             ) sub ON true
             LEFT JOIN LATERAL (
                 SELECT limits FROM plans WHERE code = 'dev' ORDER BY version DESC LIMIT 1
             ) dev ON true
             WHERE i.user_id = u.id
               AND i.at < now() - make_interval(days =>
                     coalesce((sub.limits->>'analytics_days')::int,
                              (dev.limits->>'analytics_days')::int, 2))",
        )
        .execute(&pool)
        .await;
        let _ = sqlx::query(
            "DELETE FROM invocations WHERE user_id IS NULL AND at < now() - interval '2 days'",
        )
        .execute(&pool)
        .await;
    }
}

// ------------------------------------------------------------------ queries

#[derive(Debug, Serialize, Default)]
pub struct Summary {
    pub invocations: i64,
    pub prior_invocations: i64,
    pub p95_exec_ms: f64,
    pub errors: i64,
    pub retention_days: i64,
}

pub struct DayBucket {
    pub label: String,
    pub value: i64,
}

pub struct RecentRow {
    pub function: String,
    pub outcome: String,
    pub detail: Option<String>,
    pub wall_ms: f64,
    pub cpu_ms: f64,
    pub exec_ms: f64,
    pub at: i64,
}

/// Function names this user has invocations for — the filter's options.
pub async fn invoked_functions(pool: &PgPool, user_id: Uuid) -> Vec<String> {
    sqlx::query(
        "SELECT DISTINCT function_name FROM invocations WHERE user_id = $1 ORDER BY function_name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map(|rows| rows.iter().map(|row| row.get("function_name")).collect())
    .unwrap_or_default()
}

/// Headline numbers for `days` back, plus the prior equal window to compare.
pub async fn summary(pool: &PgPool, user_id: Uuid, days: i64) -> Summary {
    let row = sqlx::query(
        "SELECT
             count(*) FILTER (WHERE at >= now() - make_interval(days => $2)) AS current,
             count(*) FILTER (WHERE at <  now() - make_interval(days => $2)) AS prior,
             count(*) FILTER (WHERE at >= now() - make_interval(days => $2)
                                AND outcome <> 'success') AS errors,
             coalesce(percentile_cont(0.95) WITHIN GROUP (ORDER BY exec_ms)
                      FILTER (WHERE at >= now() - make_interval(days => $2)), 0) AS p95
         FROM invocations
         WHERE user_id = $1 AND at >= now() - make_interval(days => $2 * 2)",
    )
    .bind(user_id)
    .bind(days as i32)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    match row {
        Some(row) => Summary {
            invocations: row.get("current"),
            prior_invocations: row.get("prior"),
            p95_exec_ms: row.get("p95"),
            errors: row.get("errors"),
            retention_days: days,
        },
        None => Summary {
            retention_days: days,
            ..Summary::default()
        },
    }
}

/// One bucket per day, oldest first, including days with no invocations.
pub async fn per_day(pool: &PgPool, user_id: Uuid, days: i64) -> Vec<DayBucket> {
    sqlx::query(
        "SELECT to_char(d.day, 'Dy') AS label, count(i.id) AS value
         FROM generate_series(
                  date_trunc('day', now()) - make_interval(days => $2 - 1),
                  date_trunc('day', now()),
                  interval '1 day') AS d(day)
         LEFT JOIN invocations i
                ON i.user_id = $1 AND date_trunc('day', i.at) = d.day
         GROUP BY d.day ORDER BY d.day",
    )
    .bind(user_id)
    .bind(days as i32)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.iter()
            .map(|row| DayBucket {
                label: row.get::<String, _>("label").trim().to_string(),
                value: row.get("value"),
            })
            .collect()
    })
    .unwrap_or_default()
}

pub async fn recent(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
    offset: i64,
    function: Option<&str>,
    errors_only: bool,
) -> Vec<RecentRow> {
    sqlx::query(
        "SELECT function_name, outcome, detail, wall_ms, cpu_ms, exec_ms,
                extract(epoch FROM at)::bigint AS at
         FROM invocations
         WHERE user_id = $1
           AND ($3::text IS NULL OR function_name = $3)
           AND (NOT $4 OR outcome <> 'success')
         ORDER BY at DESC LIMIT $2 OFFSET $5",
    )
    .bind(user_id)
    .bind(limit)
    .bind(function)
    .bind(errors_only)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.iter()
            .map(|row| RecentRow {
                function: row.get("function_name"),
                outcome: row.get("outcome"),
                detail: row.get("detail"),
                wall_ms: row.get("wall_ms"),
                cpu_ms: row.get("cpu_ms"),
                exec_ms: row.get("exec_ms"),
                at: row.get("at"),
            })
            .collect()
    })
    .unwrap_or_default()
}
