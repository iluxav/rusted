-- Invocation metrics. Written from a background batcher (never on the request
-- path) and pruned per the subscriber's plan retention.
CREATE TABLE invocations (
    id            UUID PRIMARY KEY DEFAULT uuidv7(),
    function_name TEXT NOT NULL,
    user_id       UUID REFERENCES users(id) ON DELETE CASCADE,
    outcome       TEXT NOT NULL,
    detail        TEXT,
    wall_ms       DOUBLE PRECISION NOT NULL,
    cpu_ms        DOUBLE PRECISION NOT NULL,
    exec_ms       DOUBLE PRECISION NOT NULL,
    at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Every dashboard query filters by owner and window, and retention prunes by age.
CREATE INDEX invocations_user_at ON invocations (user_id, at DESC);
CREATE INDEX invocations_at ON invocations (at);
