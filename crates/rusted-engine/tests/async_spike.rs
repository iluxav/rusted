// Spike: before restructuring the executor, prove the three things an async
// runtime has to keep doing. If any fails, the change is not worth making.
//
//   1. an awaited native fetch really releases the thread
//   2. the wall-clock interrupt still fires on a runaway loop
//   3. the heap cap still holds
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rquickjs::{AsyncContext, AsyncRuntime, Function};

/// A stand-in for an outbound call: yields for `ms`, using no CPU.
fn install_sleep(ctx: &rquickjs::Ctx<'_>) {
    let f = Function::new(
        ctx.clone(),
        rquickjs::function::Async(|ms: u64| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            ms
        }),
    )
    .unwrap();
    ctx.globals().set("__sleep", f).unwrap();
}

/// The whole point: two invocations awaiting concurrently should overlap, so
/// four 200ms waits finish in ~200ms rather than ~800ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn awaiting_releases_the_thread() {
    let started = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..4 {
        handles.push(tokio::spawn(async {
            let rt = AsyncRuntime::new().unwrap();
            let ctx = AsyncContext::full(&rt).await.unwrap();
            ctx.async_with(async |c| {
                install_sleep(&c);
                let p: rquickjs::Promise = c
                    .eval("(async () => { await __sleep(200); return 'done'; })()")
                    .unwrap();
                let out: String = p.into_future().await.unwrap();
                assert_eq!(out, "done");
            })
            .await;
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let elapsed = started.elapsed();
    eprintln!(
        "ASYNC 4 concurrent 200ms awaits took {:.0}ms (serial would be ~800ms)",
        elapsed.as_secs_f64() * 1000.0
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "awaits did not overlap: {elapsed:?}"
    );
}

/// Containment must survive: a runaway loop still has to be interrupted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_interrupt_still_fires_under_the_async_runtime() {
    let rt = AsyncRuntime::new().unwrap();
    let expired = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_millis(300);
    let flag = expired.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        if Instant::now() >= deadline {
            flag.store(true, Ordering::Relaxed);
            return true;
        }
        false
    })))
    .await;
    let ctx = AsyncContext::full(&rt).await.unwrap();

    let started = Instant::now();
    ctx.async_with(async |c| {
        let r = c.eval::<(), _>("while (true) {}");
        assert!(r.is_err(), "an infinite loop should have been interrupted");
    })
    .await;
    let elapsed = started.elapsed();
    eprintln!(
        "ASYNC infinite loop stopped after {:.0}ms",
        elapsed.as_secs_f64() * 1000.0
    );
    assert!(expired.load(Ordering::Relaxed), "interrupt never fired");
    assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
}

/// And the heap cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_heap_cap_still_holds_under_the_async_runtime() {
    let rt = AsyncRuntime::new().unwrap();
    rt.set_memory_limit(8 * 1024 * 1024).await;
    let ctx = AsyncContext::full(&rt).await.unwrap();
    ctx.async_with(async |c| {
        let r = c.eval::<(), _>("const a = []; for (;;) a.push('x'.repeat(4096));");
        assert!(r.is_err(), "allocating past the cap should have failed");
    })
    .await;
    eprintln!("ASYNC heap cap held");
}
