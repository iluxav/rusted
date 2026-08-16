-- The per-(account, env) SQLite database capability. `db` records the
-- module's declaration, like `state`. The lease table exists from day one
-- (scaling.md): a SQLite file is single-writer-single-node, so opening a
-- database requires holding the (user, env) lease — trivially satisfied at
-- one instance, and the schema the N>1 world will need.
ALTER TABLE functions ADD COLUMN db BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE db_leases (
    user_id      UUID NOT NULL,
    env          TEXT NOT NULL,
    instance     TEXT NOT NULL,
    heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, env)
);
