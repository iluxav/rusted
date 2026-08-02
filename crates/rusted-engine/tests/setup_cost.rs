// Where does an invocation's time actually go?
use std::time::Instant;

use rusted_engine::{Executor, HttpRequest, Limits, QuickJsExecutor};

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
