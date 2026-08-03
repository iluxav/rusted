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

/// Invocation payloads above this are rejected at the router with 413. This
/// bounds what a caller can send to a function, not what you can deploy.
pub const REQUEST_BODY_LIMIT: usize = 256 * 1024;

/// Admin uploads carry the script inside JSON, which escapes and inflates it.
///
/// This has to stay above the largest plan's `max_script_bytes`, or the router
/// rejects an oversized push with a bare "Payload Too Large" before the plan
/// check can say which plan allows what. The transport should never be the
/// thing that refuses a deploy.
pub const ADMIN_BODY_LIMIT: usize = 8 * 1024 * 1024;
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
    /// Where handlers run.
    ///
    /// Separate from the runtime serving HTTP on purpose. Between await points
    /// JavaScript executes synchronously, so a CPU-heavy handler occupies
    /// whatever thread is driving it — and on the HTTP runtime that would be a
    /// worker that should be answering requests, including the 429s that shed
    /// load. Isolating execution keeps a runaway handler from making the server
    /// unreachable, while awaits still release the thread to other invocations.
    pub exec_runtime: Arc<ExecRuntime>,
    /// Stops admitting invocations while the process is already using too much.
    pub memory: Arc<crate::memory::MemoryGuard>,
    /// Inboxes in memory, written through to Postgres.
    pub inboxes: Arc<crate::inbox::InboxCache>,
    pub limits: Limits,
    /// Caps how many invocations run on worker threads at once.
    pub exec_slots: Arc<tokio::sync::Semaphore>,
    pub data_addr: OnceLock<SocketAddr>,
    pub admin_addr: OnceLock<SocketAddr>,
    /// Where callers actually reach this server. Behind a reverse proxy the
    /// bound socket is a private address, so every URL we hand out — function
    /// endpoints, device sign-in, the console — has to come from here instead.
    pub public_url: Option<String>,
    pub invoke_seq: AtomicU64,
    /// How long a queued invocation waits for its function's turn before 429.
    pub queue_wait_ms: u64,
    /// Print per-invocation details to stdout.
    pub debug: bool,
}

/// Loose upper bound on in-flight invocations, so slots cannot be set absurdly
/// high on a machine with a lot of RAM and a little CPU. Memory pressure is
/// handled by [`crate::memory::MemoryGuard`] watching what is actually used;
/// this only keeps the number sane.
const MEMORY_PER_SLOT_HINT: usize = 1024 * 1024;

/// How many invocations may be in flight at once.
///
/// Sizing this from core count treats every handler as CPU-bound, which the
/// interesting ones are not: a handler awaiting `fetch` holds its slot while
/// using no CPU at all. Measured on a 2-core server, one waiting on a 1-second
/// API used 2.52ms of CPU per invocation and the server topped out at 15
/// requests/second — rejecting traffic with 85% of the CPU idle.
///
/// Raising slots costs nothing when handlers are CPU-bound. Across 16, 32, 64
/// and 128 slots, a no-fetch handler held steady at 858-941 req/s with p99
/// between 124ms and 138ms — noise — while the slow-upstream handler went from
/// 16 req/s (and 272 rejections) to 59 with none. The OS scheduler handles
/// contention better than admission control does.
///
/// So slots are sized for waiting and bounded by memory, with core count as a
/// floor so CPU-bound work is never worse off than before. `RUSTED_WORKERS`
/// overrides it for a box that knows better.
pub fn worker_slots() -> usize {
    if let Some(explicit) = std::env::var("RUSTED_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return explicit;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    // Room for waiting, since that is where the time goes — and since fetch
    // became async, waiting costs a task and ~140 KB rather than a thread. On a
    // 1-second upstream, 64 slots capped a 2-core box at 66 req/s with 565
    // rejections; 256 reached 178 with none and no queueing at all.
    let by_cpu = cores * 128;
    // A sanity bound, not the memory policy: reserving a fixed share per slot
    // is wrong whichever number you pick, so the guard measures instead.
    let by_memory = usable_memory_bytes()
        .map(|bytes| (bytes / 2) / MEMORY_PER_SLOT_HINT)
        .unwrap_or(cores * 32);
    // Never fewer than cores: that is the floor for CPU-bound work.
    by_cpu.min(by_memory).max(cores).clamp(4, 512)
}

/// Memory the OS reports as available, on the platforms where asking is cheap.
/// `None` elsewhere, which falls back to a conservative slot count.
pub fn usable_memory_bytes() -> Option<usize> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<usize>().ok())
        .map(|kb| kb * 1024)
}

/// Owns the execution runtime and shuts it down without blocking.
///
/// Dropping a tokio runtime waits for its threads, which panics if it happens
/// inside another runtime — and `AppState` is routinely dropped from async
/// code, including at the end of every test. Shutting down in the background
/// gives up that wait, which is what a process that is finishing anyway wants.
pub struct ExecRuntime(Option<tokio::runtime::Runtime>);

impl ExecRuntime {
    fn new(runtime: tokio::runtime::Runtime) -> Self {
        Self(Some(runtime))
    }

    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.0.as_ref().expect("runtime is alive").spawn(future)
    }
}

impl Drop for ExecRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

impl AppState {
    pub fn new(
        store: Store,
        pool: PgPool,
        analytics: Recorder,
        queue_wait_ms: u64,
        debug: bool,
        require_auth: bool,
        public_url: Option<String>,
    ) -> Self {
        let workers = worker_slots();
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
            memory: Arc::new(crate::memory::MemoryGuard::new()),
            inboxes: Arc::new(crate::inbox::InboxCache::default()),
            exec_runtime: Arc::new(ExecRuntime::new(
                tokio::runtime::Builder::new_multi_thread()
                    // Sized by cores, not by slots: this bounds parallel
                    // *execution*, while slots bound how many invocations may be
                    // in flight, most of them waiting on the network.
                    .worker_threads(
                        std::thread::available_parallelism()
                            .map(|n| n.get())
                            .unwrap_or(2),
                    )
                    .thread_name("rusted-exec")
                    .enable_all()
                    .build()
                    .expect("execution runtime"),
            )),
            limits: Limits::default(),
            exec_slots: Arc::new(tokio::sync::Semaphore::new(workers)),
            data_addr: OnceLock::new(),
            admin_addr: OnceLock::new(),
            public_url: public_url.map(|u| u.trim_end_matches('/').to_string()),
            invoke_seq: AtomicU64::new(0),
            queue_wait_ms,
            debug,
        }
    }

    /// Where a human goes to finish something the CLI started.
    pub fn console_url(&self, path: &str) -> String {
        match &self.public_url {
            Some(base) => format!("{base}{path}"),
            None => format!(
                "http://{}{path}",
                self.admin_addr.get().expect("admin_addr set at startup")
            ),
        }
    }

    pub fn data_url(&self, path: &str) -> String {
        match &self.public_url {
            Some(base) => format!("{base}{path}"),
            None => format!(
                "http://{}{path}",
                self.data_addr.get().expect("data_addr set at startup")
            ),
        }
    }
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod worker_slot_tests {
    use super::*;

    /// One test, because these all read the same environment variable and
    /// cargo runs tests in parallel — separate tests would race each other.
    #[test]
    fn worker_slots_are_sized_for_waiting_and_overridable() {
        std::env::remove_var("RUSTED_WORKERS");
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let slots = worker_slots();

        // A handler awaiting fetch holds a slot while using no CPU, so there
        // must be meaningfully more slots than cores.
        assert!(slots >= 4, "too few slots: {slots}");
        assert!(
            slots > cores || slots == 64,
            "slots ({slots}) should exceed cores ({cores}) unless clamped"
        );

        // The override exists so a box can be tuned without a release.
        std::env::set_var("RUSTED_WORKERS", "17");
        assert_eq!(worker_slots(), 17);

        // Nonsense falls through to the default rather than leaving the server
        // with zero slots and refusing everything.
        for bad in ["0", "-3", "banana", ""] {
            std::env::set_var("RUSTED_WORKERS", bad);
            assert!(worker_slots() >= 4, "'{bad}' left too few slots");
        }
        std::env::remove_var("RUSTED_WORKERS");
    }
}
