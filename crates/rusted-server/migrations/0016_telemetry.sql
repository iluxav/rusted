-- Persisted OpenTelemetry totals, so process-cumulative counters become
-- deployment-cumulative: a background task folds baseline + live back here,
-- and a restart loads the table as its baseline. A crash loses at most one
-- persist interval of counts.
CREATE TABLE telemetry_counters (
    function    TEXT NOT NULL,
    outcome     TEXT NOT NULL,
    invocations BIGINT NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (function, outcome)
);

CREATE TABLE telemetry_exec_histograms (
    function      TEXT PRIMARY KEY,
    -- Counts per bucket of telemetry::EXEC_BOUNDS_MS, plus the overflow.
    bucket_counts JSONB NOT NULL,
    total         BIGINT NOT NULL,
    sum_ms        DOUBLE PRECISION NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
