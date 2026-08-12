-- Durable JSON state for functions that declare `config.state = true`.
--
-- Scoped by (owner, stable function name), with the name a plain string
-- rather than a foreign key on purpose: state survives new revisions and even
-- delete/redeploy of the function. Only the explicit purge operation
-- (admin API / CLI) removes it — a redeploy losing coordination state would
-- be a much worse surprise than stale rows costing a few kilobytes.
--
-- `bytes` stores the serialized size at write time so per-function accounting
-- (plan limits on keys and total bytes) is a SUM over one small index range,
-- not a re-serialization of every value.
CREATE TABLE function_state (
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    function_name TEXT NOT NULL,
    key           TEXT NOT NULL,
    value         JSONB NOT NULL,
    version       BIGINT NOT NULL DEFAULT 1,
    bytes         INTEGER NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, function_name, key)
);

-- Deploy-time capability declarations, captured with the revision like
-- secrets and publicness: `state` gates context.state, `objects` holds the
-- binding configs (endpoints and secret NAMES only — never credentials).
ALTER TABLE functions
    ADD COLUMN state BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN objects JSONB;
