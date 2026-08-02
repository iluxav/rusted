use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusted_engine::{Limits, LogEntry, QuickJsExecutor};
use serde::Serialize;
use sqlx::postgres::PgPool;

use crate::analytics::Recorder;
use crate::auth::AuthCaches;
use crate::plans::{PlanCache, RateLimiter};
use crate::store::Store;

/// Request bodies above this are rejected at the router with 413.
pub const REQUEST_BODY_LIMIT: usize = 256 * 1024;
/// Invocation records kept per function.
pub const RECORD_CAP: usize = 50;

pub struct TempRun {
    pub source: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct InvocationRecord {
    pub at: u64,
    pub outcome: String,
    /// Error message or termination reason — owner-facing only; the data API
    /// never returns this to endpoint callers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub wall_ms: f64,
    pub cpu_ms: f64,
    pub logs: Vec<LogEntry>,
}

pub struct AppState {
    pub store: Store,
    pub pool: PgPool,
    pub auth: AuthCaches,
    pub analytics: Recorder,
    pub plan_cache: PlanCache,
    pub rate_limiter: RateLimiter,
    /// Require `Authorization: Bearer rk_live_…` on the data plane.
    pub require_auth: bool,
    pub temp_runs: Mutex<HashMap<String, TempRun>>,
    /// Per-function semaphores enforcing the plan's concurrency allowance.
    /// Keyed by function; the permit count comes from the owner's plan, and a
    /// changed allowance replaces the semaphore rather than resizing it.
    pub fn_locks: Mutex<HashMap<String, (usize, Arc<tokio::sync::Semaphore>)>>,
    pub records: Mutex<HashMap<String, VecDeque<InvocationRecord>>>,
    pub executor: Arc<QuickJsExecutor>,
    pub limits: Limits,
    /// Caps how many invocations run on worker threads at once.
    pub exec_slots: Arc<tokio::sync::Semaphore>,
    pub data_addr: OnceLock<SocketAddr>,
    pub admin_addr: OnceLock<SocketAddr>,
    pub invoke_seq: AtomicU64,
    /// How long a queued invocation waits for its function's turn before 429.
    pub queue_wait_ms: u64,
    /// Print per-invocation details to stdout.
    pub debug: bool,
}

impl AppState {
    pub fn new(
        store: Store,
        pool: PgPool,
        analytics: Recorder,
        queue_wait_ms: u64,
        debug: bool,
        require_auth: bool,
    ) -> Self {
        // JS execution is CPU-bound, so more workers than cores buys nothing
        // but context switching. This is the ceiling on real parallelism; a
        // plan's per-function allowance can exceed it, and the excess queues.
        let workers = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 32))
            .unwrap_or(4);
        Self {
            store,
            pool,
            auth: AuthCaches::default(),
            analytics,
            plan_cache: PlanCache::default(),
            rate_limiter: RateLimiter::default(),
            require_auth,
            temp_runs: Mutex::new(HashMap::new()),
            fn_locks: Mutex::new(HashMap::new()),
            records: Mutex::new(HashMap::new()),
            executor: Arc::new(QuickJsExecutor::new()),
            limits: Limits::default(),
            exec_slots: Arc::new(tokio::sync::Semaphore::new(workers)),
            data_addr: OnceLock::new(),
            admin_addr: OnceLock::new(),
            invoke_seq: AtomicU64::new(0),
            queue_wait_ms,
            debug,
        }
    }

    /// Where a human goes to finish something the CLI started.
    pub fn console_url(&self, path: &str) -> String {
        format!(
            "http://{}{path}",
            self.admin_addr.get().expect("admin_addr set at startup")
        )
    }

    pub fn data_url(&self, path: &str) -> String {
        format!(
            "http://{}{path}",
            self.data_addr.get().expect("data_addr set at startup")
        )
    }
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
