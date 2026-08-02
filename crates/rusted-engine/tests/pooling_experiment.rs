// Experiment: what would reusing a QuickJS context across invocations buy,
// and what would it cost? Threads are not part of the question — `Context` is
// neither `Send` nor `Sync` unless rquickjs's `parallel` feature is on, and
// that feature works by putting every runtime call behind a mutex, so a shared
// context serializes rather than parallelises.
use std::time::Instant;

use rquickjs::{Context, Runtime};

const HANDLER: &str = r#"globalThis.__handler = (n) => { let a = 0; for (let i = 0; i < n; i++) a += i % 7; return a; };"#;

#[test]
#[ignore = "measurement, not an assertion"]
fn what_reuse_saves_per_invocation() {
    // Today: a fresh runtime + context + handler for every call.
    let t = Instant::now();
    for _ in 0..200 {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|c| {
            c.eval::<(), _>(HANDLER).unwrap();
            c.eval::<i32, _>("__handler(1000)").unwrap();
        });
    }
    let fresh = t.elapsed().as_secs_f64() * 1000.0 / 200.0;

    // Proposed: build once, invoke many.
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|c| {
        c.eval::<(), _>(HANDLER).unwrap();
    });
    let t = Instant::now();
    for _ in 0..200 {
        ctx.with(|c| {
            c.eval::<i32, _>("__handler(1000)").unwrap();
        });
    }
    let reused = t.elapsed().as_secs_f64() * 1000.0 / 200.0;

    eprintln!(
        "REUSE fresh={fresh:.3}ms/call  reused={reused:.3}ms/call  saved={:.3}ms ({:.0}%)",
        fresh - reused,
        (fresh - reused) / fresh * 100.0
    );
}

#[test]
#[ignore = "measurement, not an assertion"]
fn what_survives_between_invocations_on_a_reused_context() {
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();

    // "Invocation 1": one caller's request touches the shared world.
    ctx.with(|c| {
        c.eval::<(), _>(
            r#"globalThis.__cache = "caller-1 bearer sk-live-abc123";
               Array.prototype.map = function () { return "poisoned"; };"#,
        )
        .unwrap();
    });

    // "Invocation 2": a different caller, same function, same context.
    let seen: String = ctx
        .with(|c| {
            c.eval::<String, _>(
                r#"String(globalThis.__cache) + "  ||  [1,2,3].map(x=>x*2) === " + [1,2,3].map(x => x * 2)"#,
            )
        })
        .unwrap();
    eprintln!("LEAK invocation 2 sees: {seen}");
}

#[test]
#[ignore = "measurement, not an assertion"]
fn does_the_heap_cap_still_bound_a_reused_context() {
    let rt = Runtime::new().unwrap();
    rt.set_memory_limit(8 * 1024 * 1024);
    let ctx = Context::full(&rt).unwrap();

    // Each "invocation" leaks a little, as ordinary code does.
    let mut died_on = None;
    for i in 1..=40 {
        let ok = ctx.with(|c| {
            c.eval::<(), _>(
                r#"globalThis.__leak = globalThis.__leak || [];
                   for (let i = 0; i < 500; i++) globalThis.__leak.push({ id: i, pad: "x".repeat(200) });"#,
            )
            .is_ok()
        });
        if !ok {
            died_on = Some(i);
            break;
        }
    }
    let used = rt.memory_usage().malloc_size;
    eprintln!(
        "ACCUMULATE died_on_invocation={:?} heap_now={}KB of 8192KB cap",
        died_on,
        used / 1024
    );
}
