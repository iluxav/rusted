-- Environments: per-account secrets/state overlays selected by URL
-- (`/f/@stage/name`). `prod` is implicit — always valid, never a row, never
-- deletable — so this table holds only the additional environments an account
-- created. The overlay dimension lands on secrets AND durable state: a stage
-- invocation running against prod's coordination state would be the exact
-- incident environments exist to prevent. Existing rows become prod's.
CREATE TABLE environments (
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, name)
);

ALTER TABLE secrets ADD COLUMN env TEXT NOT NULL DEFAULT 'prod';
ALTER TABLE secrets DROP CONSTRAINT secrets_pkey;
ALTER TABLE secrets ADD PRIMARY KEY (user_id, env, name);

ALTER TABLE function_state ADD COLUMN env TEXT NOT NULL DEFAULT 'prod';
ALTER TABLE function_state DROP CONSTRAINT function_state_pkey;
ALTER TABLE function_state ADD PRIMARY KEY (user_id, function_name, env, key);
