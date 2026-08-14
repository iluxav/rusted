//! OpenTelemetry metrics for the data plane: an invocation counter and a
//! pure-execution-time histogram, recorded beside the analytics writer and
//! read back in-process through a [`ManualReader`] — no exporter, no
//! collector; the server is its own metrics backend.
//!
//! Counters are cumulative and die with the process, so totals are folded
//! over a Postgres baseline: a background task periodically persists
//! `baseline + live`, and a restart loads the table back as the new
//! baseline. A crash loses at most one persist interval of counts.
//!
//! v1 dimensions are `function` and `outcome` (histogram: `function` only) —
//! environments are deliberately not an attribute yet, so stats answer "how
//! is this function doing" across envs.

use std::collections::HashMap;
use std::sync::Mutex;

use std::sync::Arc;

use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::reader::MetricReader;
use opentelemetry_sdk::metrics::{
    InstrumentKind, ManualReader, Pipeline, SdkMeterProvider, Temporality,
};
use serde::Serialize;
use sqlx::postgres::PgPool;
use sqlx::Row;

/// Histogram bucket upper bounds, in milliseconds of pure handler execution.
/// Chosen for the plans' exec budgets (50ms dev … 30s extra).
pub const EXEC_BOUNDS_MS: [f64; 14] = [
    1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0,
];

/// How often the baseline task folds live totals into Postgres.
/// `RUSTED_TELEMETRY_PERSIST_SECS` overrides (tests set it to 1).
pub fn persist_interval() -> std::time::Duration {
    let secs = std::env::var("RUSTED_TELEMETRY_PERSIST_SECS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .filter(|s| *s > 0)
        .unwrap_or(60);
    std::time::Duration::from_secs(secs)
}

#[derive(Debug, Default, Clone)]
struct HistogramState {
    bucket_counts: Vec<u64>,
    total: u64,
    sum_ms: f64,
}

/// The dashboard's headline tiles, across all of a caller's functions.
#[derive(Debug, Default, Clone, Serialize)]
pub struct OverallStats {
    pub invocations: u64,
    pub errors: u64,
    pub error_rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95_exec_ms: Option<f64>,
}

/// Everything known about one function, baseline + live merged.
#[derive(Debug, Default, Clone, Serialize)]
pub struct FunctionStats {
    pub function: String,
    pub invocations: u64,
    pub success: u64,
    pub error: u64,
    pub terminated: u64,
    pub refused: u64,
    /// (error + terminated) / executed — refusals never reached the handler,
    /// so they are not part of the failure rate a handler owns.
    pub error_rate: f64,
    /// Approximate, interpolated from histogram buckets. Absent until the
    /// function has executed at least once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95_exec_ms: Option<f64>,
}

/// [`ManualReader`] is neither Clone nor readable through the provider once
/// handed over, so both the provider and this module hold it through one
/// delegating wrapper — the SDK's intended shape for on-demand readers.
#[derive(Clone, Debug)]
struct SharedReader(Arc<ManualReader>);

impl MetricReader for SharedReader {
    fn register_pipeline(&self, pipeline: std::sync::Weak<Pipeline>) {
        self.0.register_pipeline(pipeline)
    }
    fn collect(&self, rm: &mut ResourceMetrics) -> OTelSdkResult {
        self.0.collect(rm)
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }
    fn shutdown_with_timeout(&self, timeout: std::time::Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }
    fn temporality(&self, kind: InstrumentKind) -> Temporality {
        self.0.temporality(kind)
    }
}

pub struct Telemetry {
    invocations: Counter<u64>,
    exec_ms: Histogram<f64>,
    reader: SharedReader,
    /// Keeps instruments alive; never read after construction.
    _provider: SdkMeterProvider,
    /// Totals from previous processes, loaded once at startup.
    counter_baseline: Mutex<HashMap<(String, String), u64>>,
    histogram_baseline: Mutex<HashMap<String, HistogramState>>,
}

impl Telemetry {
    pub fn new() -> Telemetry {
        let reader = SharedReader(Arc::new(ManualReader::builder().build()));
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .build();
        let meter = provider.meter("rusted");
        let invocations = meter
            .u64_counter("rusted.invocations")
            .with_description("Invocations by function and outcome")
            .build();
        let exec_ms = meter
            .f64_histogram("rusted.exec.duration")
            .with_unit("ms")
            .with_description("Pure handler execution time")
            .with_boundaries(EXEC_BOUNDS_MS.to_vec())
            .build();
        Telemetry {
            invocations,
            exec_ms,
            reader,
            _provider: provider,
            counter_baseline: Mutex::new(HashMap::new()),
            histogram_baseline: Mutex::new(HashMap::new()),
        }
    }

    /// One invocation outcome; `exec_ms` present when a handler actually ran.
    pub fn record(&self, function: &str, outcome: &str, exec_ms: Option<f64>) {
        let attrs = [
            KeyValue::new("function", function.to_string()),
            KeyValue::new("outcome", outcome.to_string()),
        ];
        self.invocations.add(1, &attrs);
        if let Some(exec) = exec_ms {
            self.exec_ms
                .record(exec, &[KeyValue::new("function", function.to_string())]);
        }
    }

    /// The live process totals, straight from the reader.
    fn live(
        &self,
    ) -> (
        HashMap<(String, String), u64>,
        HashMap<String, HistogramState>,
    ) {
        let mut rm = ResourceMetrics::default();
        let mut counters: HashMap<(String, String), u64> = HashMap::new();
        let mut histograms: HashMap<String, HistogramState> = HashMap::new();
        if self.reader.collect(&mut rm).is_err() {
            return (counters, histograms);
        }
        for scope in rm.scope_metrics() {
            for metric in scope.metrics() {
                match (metric.name(), metric.data()) {
                    ("rusted.invocations", AggregatedMetrics::U64(MetricData::Sum(sum))) => {
                        for point in sum.data_points() {
                            let mut function = String::new();
                            let mut outcome = String::new();
                            for kv in point.attributes() {
                                match kv.key.as_str() {
                                    "function" => function = kv.value.to_string(),
                                    "outcome" => outcome = kv.value.to_string(),
                                    _ => {}
                                }
                            }
                            *counters.entry((function, outcome)).or_default() += point.value();
                        }
                    }
                    ("rusted.exec.duration", AggregatedMetrics::F64(MetricData::Histogram(h))) => {
                        for point in h.data_points() {
                            let function = point
                                .attributes()
                                .find(|kv| kv.key.as_str() == "function")
                                .map(|kv| kv.value.to_string())
                                .unwrap_or_default();
                            let entry = histograms.entry(function).or_default();
                            let counts: Vec<u64> = point.bucket_counts().collect();
                            merge_buckets(&mut entry.bucket_counts, &counts);
                            entry.total += point.count();
                            entry.sum_ms += point.sum();
                        }
                    }
                    _ => {}
                }
            }
        }
        (counters, histograms)
    }

    /// Baseline + live, as per-function stats. `only` filters to a caller's
    /// functions — metrics are process-global, scoping is the API's job.
    pub fn snapshot(&self, only: Option<&[String]>) -> Vec<FunctionStats> {
        let (mut counters, mut histograms) = self.live();
        for ((function, outcome), count) in self.counter_baseline.lock().unwrap().iter() {
            *counters
                .entry((function.clone(), outcome.clone()))
                .or_default() += count;
        }
        for (function, base) in self.histogram_baseline.lock().unwrap().iter() {
            let entry = histograms.entry(function.clone()).or_default();
            merge_buckets(&mut entry.bucket_counts, &base.bucket_counts);
            entry.total += base.total;
            entry.sum_ms += base.sum_ms;
        }

        let mut by_function: HashMap<String, FunctionStats> = HashMap::new();
        for ((function, outcome), count) in counters {
            if let Some(allowed) = only {
                if !allowed.contains(&function) {
                    continue;
                }
            }
            let entry = by_function
                .entry(function.clone())
                .or_insert_with(|| FunctionStats {
                    function,
                    ..FunctionStats::default()
                });
            entry.invocations += count;
            match outcome.as_str() {
                "success" => entry.success += count,
                "error" => entry.error += count,
                "terminated" => entry.terminated += count,
                "refused" => entry.refused += count,
                _ => {}
            }
        }
        for stats in by_function.values_mut() {
            let executed = stats.success + stats.error + stats.terminated;
            if executed > 0 {
                stats.error_rate = (stats.error + stats.terminated) as f64 / executed as f64;
            }
            if let Some(h) = histograms.get(&stats.function) {
                stats.p95_exec_ms = percentile_from_buckets(&h.bucket_counts, 0.95);
            }
        }
        let mut all: Vec<FunctionStats> = by_function.into_values().collect();
        all.sort_by_key(|stats| std::cmp::Reverse(stats.invocations));
        all
    }

    /// The headline numbers across a set of functions — the dashboard tiles.
    /// The p95 comes from the merged histograms, not an average of averages.
    pub fn overall(&self, only: Option<&[String]>) -> OverallStats {
        let (mut counters, mut histograms) = self.live();
        for ((function, outcome), count) in self.counter_baseline.lock().unwrap().iter() {
            *counters
                .entry((function.clone(), outcome.clone()))
                .or_default() += count;
        }
        for (function, base) in self.histogram_baseline.lock().unwrap().iter() {
            let entry = histograms.entry(function.clone()).or_default();
            merge_buckets(&mut entry.bucket_counts, &base.bucket_counts);
            entry.total += base.total;
            entry.sum_ms += base.sum_ms;
        }
        let allowed = |function: &str| match only {
            Some(names) => names.iter().any(|name| name == function),
            None => true,
        };
        let mut overall = OverallStats::default();
        let mut executed = 0u64;
        for ((function, outcome), count) in &counters {
            if !allowed(function) {
                continue;
            }
            overall.invocations += count;
            match outcome.as_str() {
                "success" => executed += count,
                "error" | "terminated" => {
                    executed += count;
                    overall.errors += count;
                }
                _ => {}
            }
        }
        if executed > 0 {
            overall.error_rate = overall.errors as f64 / executed as f64;
        }
        let mut merged = Vec::new();
        for (function, h) in &histograms {
            if allowed(function) {
                merge_buckets(&mut merged, &h.bucket_counts);
            }
        }
        overall.p95_exec_ms = percentile_from_buckets(&merged, 0.95);
        overall
    }

    /// Loads the persisted totals as this process's baseline. Once, at boot.
    pub async fn load_baseline(&self, pool: &PgPool) {
        if let Ok(rows) =
            sqlx::query("SELECT function, outcome, invocations FROM telemetry_counters")
                .fetch_all(pool)
                .await
        {
            let mut baseline = self.counter_baseline.lock().unwrap();
            for row in rows {
                baseline.insert(
                    (row.get("function"), row.get("outcome")),
                    row.get::<i64, _>("invocations").max(0) as u64,
                );
            }
        }
        if let Ok(rows) = sqlx::query(
            "SELECT function, bucket_counts, total, sum_ms FROM telemetry_exec_histograms",
        )
        .fetch_all(pool)
        .await
        {
            let mut baseline = self.histogram_baseline.lock().unwrap();
            for row in rows {
                let counts: Vec<u64> =
                    serde_json::from_value(row.get::<serde_json::Value, _>("bucket_counts"))
                        .unwrap_or_default();
                baseline.insert(
                    row.get("function"),
                    HistogramState {
                        bucket_counts: counts,
                        total: row.get::<i64, _>("total").max(0) as u64,
                        sum_ms: row.get("sum_ms"),
                    },
                );
            }
        }
    }

    /// Writes baseline + live back, so a restart starts where this left off.
    pub async fn persist(&self, pool: &PgPool) {
        let (mut counters, mut histograms) = self.live();
        for ((function, outcome), count) in self.counter_baseline.lock().unwrap().iter() {
            *counters
                .entry((function.clone(), outcome.clone()))
                .or_default() += count;
        }
        for (function, base) in self.histogram_baseline.lock().unwrap().iter() {
            let entry = histograms.entry(function.clone()).or_default();
            merge_buckets(&mut entry.bucket_counts, &base.bucket_counts);
            entry.total += base.total;
            entry.sum_ms += base.sum_ms;
        }
        for ((function, outcome), invocations) in counters {
            let _ = sqlx::query(
                "INSERT INTO telemetry_counters (function, outcome, invocations, updated_at)
                 VALUES ($1, $2, $3, now())
                 ON CONFLICT (function, outcome)
                 DO UPDATE SET invocations = $3, updated_at = now()",
            )
            .bind(&function)
            .bind(&outcome)
            .bind(invocations as i64)
            .execute(pool)
            .await;
        }
        for (function, h) in histograms {
            let _ = sqlx::query(
                "INSERT INTO telemetry_exec_histograms
                     (function, bucket_counts, total, sum_ms, updated_at)
                 VALUES ($1, $2, $3, $4, now())
                 ON CONFLICT (function)
                 DO UPDATE SET bucket_counts = $2, total = $3, sum_ms = $4, updated_at = now()",
            )
            .bind(&function)
            .bind(serde_json::json!(h.bucket_counts))
            .bind(h.total as i64)
            .bind(h.sum_ms)
            .execute(pool)
            .await;
        }
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

fn merge_buckets(into: &mut Vec<u64>, from: &[u64]) {
    if into.len() < from.len() {
        into.resize(from.len(), 0);
    }
    for (slot, add) in into.iter_mut().zip(from.iter()) {
        *slot += add;
    }
}

/// Approximate percentile by linear interpolation inside the bucket that
/// crosses the rank. The overflow bucket has no upper bound, so its answer is
/// the last boundary — an underestimate, honestly bounded.
fn percentile_from_buckets(bucket_counts: &[u64], percentile: f64) -> Option<f64> {
    let total: u64 = bucket_counts.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = percentile * total as f64;
    let mut cumulative = 0u64;
    for (index, count) in bucket_counts.iter().enumerate() {
        let before = cumulative as f64;
        cumulative += count;
        if (cumulative as f64) >= rank {
            let lower = if index == 0 {
                0.0
            } else {
                EXEC_BOUNDS_MS[index - 1]
            };
            let upper = if index < EXEC_BOUNDS_MS.len() {
                EXEC_BOUNDS_MS[index]
            } else {
                return Some(*EXEC_BOUNDS_MS.last().unwrap());
            };
            if *count == 0 {
                return Some(upper);
            }
            let fraction = (rank - before) / *count as f64;
            return Some(lower + (upper - lower) * fraction.clamp(0.0, 1.0));
        }
    }
    Some(*EXEC_BOUNDS_MS.last().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_interpolates_inside_the_crossing_bucket() {
        // 100 samples all in the 10..25ms bucket (index 4): p95 lands 95%
        // of the way through it.
        let mut buckets = vec![0u64; EXEC_BOUNDS_MS.len() + 1];
        buckets[4] = 100;
        let p95 = percentile_from_buckets(&buckets, 0.95).unwrap();
        assert!((p95 - (10.0 + 0.95 * 15.0)).abs() < 1e-9, "{p95}");
        // Empty: no answer rather than a fake zero.
        assert_eq!(percentile_from_buckets(&[0, 0, 0], 0.95), None);
        // Overflow bucket answers the last boundary, not infinity.
        let mut overflow = vec![0u64; EXEC_BOUNDS_MS.len() + 1];
        *overflow.last_mut().unwrap() = 10;
        assert_eq!(percentile_from_buckets(&overflow, 0.95), Some(30000.0));
    }

    #[test]
    fn record_and_snapshot_round_trip_through_the_reader() {
        let telemetry = Telemetry::new();
        for _ in 0..19 {
            telemetry.record("fn-a", "success", Some(12.0));
        }
        telemetry.record("fn-a", "error", Some(200.0));
        telemetry.record("fn-a", "refused", None);
        telemetry.record("fn-b", "success", Some(3.0));

        let all = telemetry.snapshot(None);
        let a = all.iter().find(|s| s.function == "fn-a").unwrap();
        assert_eq!(a.invocations, 21);
        assert_eq!(a.success, 19);
        assert_eq!(a.error, 1);
        assert_eq!(a.refused, 1);
        // Refusals are excluded from the failure rate's denominator.
        assert!((a.error_rate - 0.05).abs() < 1e-9, "{}", a.error_rate);
        let p95 = a.p95_exec_ms.unwrap();
        assert!(p95 > 10.0 && p95 <= 250.0, "{p95}");

        // Scoping filters, never renames.
        let only_b = telemetry.snapshot(Some(&["fn-b".to_string()]));
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].function, "fn-b");
    }

    #[test]
    fn baseline_totals_fold_into_the_snapshot() {
        let telemetry = Telemetry::new();
        telemetry.record("fn-a", "success", Some(5.0));
        telemetry
            .counter_baseline
            .lock()
            .unwrap()
            .insert(("fn-a".into(), "success".into()), 100);
        let mut buckets = vec![0u64; EXEC_BOUNDS_MS.len() + 1];
        buckets[2] = 100;
        telemetry.histogram_baseline.lock().unwrap().insert(
            "fn-a".into(),
            HistogramState {
                bucket_counts: buckets,
                total: 100,
                sum_ms: 400.0,
            },
        );
        let all = telemetry.snapshot(None);
        let a = all.iter().find(|s| s.function == "fn-a").unwrap();
        assert_eq!(a.invocations, 101);
        assert_eq!(a.success, 101);
        assert!(a.p95_exec_ms.unwrap() <= 10.0, "{:?}", a.p95_exec_ms);
    }
}
