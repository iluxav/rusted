//! Admission control against the memory actually in use.
//!
//! Slots used to double as a memory bound by reserving a fixed share each. That
//! cannot be right: the reservation was either far above what an invocation
//! typically holds — 140 KB measured, against megabytes reserved, capping
//! concurrency for no reason — or far below the 32 MB one may reach, in which
//! case enough concurrent handlers allocating to the ceiling would exhaust the
//! machine. Any fixed guess is wrong in one direction or the other.
//!
//! So nothing is guessed. The process reports what it is using, and admission
//! stops while that is above a ceiling. Slots go back to bounding concurrency,
//! which is what a semaphore is for.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a reading is reused. Long enough that a burst of requests costs one
/// syscall rather than thousands; short enough to notice pressure building.
const CACHE_FOR: Duration = Duration::from_millis(50);

/// Reads the resident memory of this process, in bytes.
pub type Reader = fn() -> Option<usize>;

pub struct MemoryGuard {
    /// Admission stops while usage is above this. Zero disables the guard.
    ceiling_bytes: usize,
    read: Reader,
    cached: Mutex<Option<(Instant, usize)>>,
}

impl MemoryGuard {
    /// A guard sized from what the machine had free at startup.
    ///
    /// The ceiling leaves room for everything else on the box — Postgres, the
    /// proxy, the kernel's caches — so hitting it means refusing work, not
    /// swapping or being killed.
    pub fn new() -> Self {
        Self::with_reader(default_ceiling(), read_resident_bytes)
    }

    pub fn with_reader(ceiling_bytes: usize, read: Reader) -> Self {
        Self {
            ceiling_bytes,
            read,
            cached: Mutex::new(None),
        }
    }

    /// The ceiling in bytes; zero when the guard is inactive.
    pub fn ceiling(&self) -> usize {
        self.ceiling_bytes
    }

    /// Current usage, or `None` where it cannot be read.
    pub fn in_use(&self) -> Option<usize> {
        if self.ceiling_bytes == 0 {
            return None;
        }
        let mut cached = self.cached.lock().unwrap();
        if let Some((at, bytes)) = *cached {
            if at.elapsed() < CACHE_FOR {
                return Some(bytes);
            }
        }
        let bytes = (self.read)()?;
        *cached = Some((Instant::now(), bytes));
        Some(bytes)
    }

    /// Whether another invocation may start.
    ///
    /// Reactive rather than predictive: it cannot know what a handler will
    /// allocate, only what the process holds now. Combined with the per
    /// invocation heap cap that is enough — usage climbs, admission stops, and
    /// in-flight work drains — where a fixed per-slot reservation could be
    /// wrong by two orders of magnitude in either direction.
    pub fn has_headroom(&self) -> bool {
        match self.in_use() {
            Some(bytes) => bytes < self.ceiling_bytes,
            // Unreadable, or disabled: do not invent backpressure.
            None => true,
        }
    }
}

impl Default for MemoryGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Resident set size, from `/proc/self/statm`. `None` off Linux, where the
/// guard stays inactive — deployments are Linux, and a wrong reading would be
/// worse than none.
fn read_resident_bytes() -> Option<usize> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    // 4096 everywhere this runs; only used to turn pages into bytes.
    Some(resident_pages * 4096)
}

/// `RUSTED_MEMORY_CEILING_MB` overrides it; otherwise two thirds of what the
/// machine reported free at startup, and zero (inactive) if that is unknown.
fn default_ceiling() -> usize {
    if let Some(mb) = std::env::var("RUSTED_MEMORY_CEILING_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return mb * 1024 * 1024;
    }
    match crate::state::usable_memory_bytes() {
        Some(bytes) => bytes / 3 * 2,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn under() -> Option<usize> {
        Some(10 * 1024 * 1024)
    }
    fn over() -> Option<usize> {
        Some(900 * 1024 * 1024)
    }
    fn unreadable() -> Option<usize> {
        None
    }

    #[test]
    fn admits_while_below_the_ceiling() {
        let g = MemoryGuard::with_reader(100 * 1024 * 1024, under);
        assert!(g.has_headroom());
        assert_eq!(g.in_use(), Some(10 * 1024 * 1024));
    }

    #[test]
    fn refuses_once_over_the_ceiling() {
        let g = MemoryGuard::with_reader(100 * 1024 * 1024, over);
        assert!(!g.has_headroom(), "should have stopped admitting");
    }

    /// Where usage cannot be read the guard must stay out of the way. Inventing
    /// backpressure from a missing measurement would refuse real work for no
    /// reason.
    #[test]
    fn an_unreadable_reading_does_not_refuse() {
        let g = MemoryGuard::with_reader(100 * 1024 * 1024, unreadable);
        assert!(g.has_headroom());
        assert_eq!(g.in_use(), None);
    }

    #[test]
    fn a_zero_ceiling_disables_the_guard() {
        let g = MemoryGuard::with_reader(0, over);
        assert!(g.has_headroom(), "a disabled guard must admit everything");
        assert_eq!(g.in_use(), None);
    }

    /// The reading is cached, because this sits on the admission path of every
    /// invocation and a syscall per request would be its own problem.
    #[test]
    fn readings_are_cached() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn counting() -> Option<usize> {
            CALLS.fetch_add(1, Ordering::Relaxed);
            Some(1024)
        }
        let g = MemoryGuard::with_reader(100 * 1024 * 1024, counting);
        for _ in 0..500 {
            assert!(g.has_headroom());
        }
        assert_eq!(CALLS.load(Ordering::Relaxed), 1, "should have read once");
    }

    /// On Linux the real reader must return something plausible for this
    /// process; elsewhere it is inactive by design.
    #[test]
    fn the_real_reader_works_where_it_is_meant_to() {
        let reading = read_resident_bytes();
        #[cfg(target_os = "linux")]
        {
            let bytes = reading.expect("the reader must work on Linux, where this deploys");
            assert!(
                bytes > 1024 * 1024 && bytes < 64 * 1024 * 1024 * 1024,
                "implausible RSS: {bytes}"
            );
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Inactive here by design — the assertion is that it says so
            // rather than returning something invented.
            assert!(reading.is_none(), "expected no reading off Linux");
        }
    }
}
