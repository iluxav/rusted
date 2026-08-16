// Where does an invocation's time actually go?
use std::time::Instant;

use rusted_engine::{Executor, HttpRequest, Limits, QuickJsExecutor};

/// How much of the per-invocation setup is prelude eval — the cost of the
/// "API shape in JS, compute in Rust" design, and the number that would
/// justify (or bury) precompiling preludes to bytecode.
#[test]
#[ignore = "measurement, not an assertion"]
fn prelude_eval_cost() {
    const ROUNDS: usize = 300;
    let mut per: Vec<(&str, f64)> = Vec::new();
    let mut totals = Vec::new();
    let mut context_only = Vec::new();

    for _ in 0..ROUNDS {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let mut round_total = 0.0;
            for (name, source) in rusted_engine::preludes::ALL {
                let t = Instant::now();
                ctx.eval::<(), _>(*source).unwrap();
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                round_total += ms;
                match per.iter_mut().find(|(n, _)| n == name) {
                    Some((_, sum)) => *sum += ms,
                    None => per.push((name, ms)),
                }
            }
            totals.push(round_total);
        });
    }
    // The denominator: what a bare runtime + context costs by itself.
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|_| {});
        context_only.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let bytes: usize = rusted_engine::preludes::ALL
        .iter()
        .map(|(_, s)| s.len())
        .sum();
    eprintln!("PRELUDES {bytes} bytes total, {ROUNDS} rounds:");
    for (name, sum) in &per {
        let source_len = rusted_engine::preludes::ALL
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| s.len())
            .unwrap_or(0);
        eprintln!(
            "  {name:<8} {:>7.1}µs  ({source_len} bytes)",
            sum / ROUNDS as f64 * 1000.0
        );
    }
    eprintln!(
        "  all preludes {:.1}µs  vs bare runtime+context {:.1}µs",
        mean(&totals) * 1000.0,
        mean(&context_only) * 1000.0
    );
}

/// The bytecode experiment: if preludes ship as module bytecode (the same
/// machinery user modules use), how much of the per-context eval cost —
/// which is mostly parsing — disappears?
#[test]
#[ignore = "measurement, not an assertion"]
fn prelude_bytecode_vs_source() {
    const ROUNDS: usize = 300;
    let preludes = rusted_engine::preludes::ALL;

    // One-time compile per process, timed for completeness.
    let t = Instant::now();
    let compiled: Vec<(&str, Vec<u8>)> = preludes
        .iter()
        .map(|(name, source)| {
            let rt = rquickjs::Runtime::new().unwrap();
            let ctx = rquickjs::Context::full(&rt).unwrap();
            let bytes = ctx.with(|c| {
                rquickjs::Module::declare(c.clone(), *name, *source)
                    .unwrap()
                    .write(Default::default())
                    .unwrap()
            });
            (*name, bytes)
        })
        .collect();
    let compile_ms = t.elapsed().as_secs_f64() * 1000.0;

    let mut source_sum = vec![0.0f64; preludes.len()];
    let mut bytecode_sum = vec![0.0f64; preludes.len()];

    for _ in 0..ROUNDS {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|c| {
            for (i, (_, source)) in preludes.iter().enumerate() {
                let t = Instant::now();
                c.eval::<(), _>(*source).unwrap();
                source_sum[i] += t.elapsed().as_secs_f64() * 1000.0;
            }
        });

        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|c| {
            for (i, (_, bytes)) in compiled.iter().enumerate() {
                let t = Instant::now();
                // SAFETY: written by this same process, exactly like the
                // executor's own bytecode cache.
                let declared = unsafe { rquickjs::Module::load(c.clone(), bytes) }.unwrap();
                let (_, progress) = declared.eval().unwrap();
                progress.finish::<()>().unwrap();
                bytecode_sum[i] += t.elapsed().as_secs_f64() * 1000.0;
            }
        });
    }

    eprintln!("BYTECODE vs SOURCE ({ROUNDS} rounds; one-time compile {compile_ms:.1}ms):");
    let (mut s_total, mut b_total) = (0.0, 0.0);
    for (i, (name, source)) in preludes.iter().enumerate() {
        let s = source_sum[i] / ROUNDS as f64 * 1000.0;
        let b = bytecode_sum[i] / ROUNDS as f64 * 1000.0;
        s_total += s;
        b_total += b;
        eprintln!(
            "  {name:<8} source {s:>7.1}µs  bytecode {b:>7.1}µs  ({:.0}% saved, {} bytes src)",
            (s - b) / s * 100.0,
            source.len(),
        );
    }
    eprintln!(
        "  total    source {s_total:>7.1}µs  bytecode {b_total:>7.1}µs  ({:.0}% saved)",
        (s_total - b_total) / s_total * 100.0
    );
}

#[test]
#[ignore = "measurement, not an assertion"]
fn decompose_invocation_cost() {
    let src = r#"export default async function handler(request, context) {
        return context.json({ ok: true });
    }"#;
    let ex = QuickJsExecutor::new();
    let limits = Limits::default();
    let req = HttpRequest::post_json("{}");

    // Warm the bytecode cache the same way a live server would be warm.
    let _ = ex.execute(src, &req, &limits);

    let mut wall = Vec::new();
    let mut exec = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        let r = ex.execute(src, &req, &limits);
        wall.push(t.elapsed().as_secs_f64() * 1000.0);
        exec.push(r.exec_wall.as_secs_f64() * 1000.0);
    }
    let mean = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len() as f64;
    let w = mean(&wall);
    let e = mean(&exec);
    eprintln!(
        "SETUP total={w:.3}ms handler={e:.3}ms setup={:.3}ms ({:.0}% overhead)",
        w - e,
        (w - e) / w * 100.0
    );
}
