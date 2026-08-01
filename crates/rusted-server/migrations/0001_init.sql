-- rusted's schema. Identities are UUIDv7 (native in Postgres 18): generated
-- anywhere without coordination, so multi-region writes never collide, and
-- time-ordered, so index locality stays close to a sequence.

CREATE TABLE users (
    id          UUID PRIMARY KEY DEFAULT uuidv7(),
    github_id   BIGINT UNIQUE NOT NULL,
    login       TEXT NOT NULL,
    name        TEXT,
    avatar_url  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id          UUID PRIMARY KEY DEFAULT uuidv7(),
    token_hash  TEXT UNIQUE NOT NULL,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL
);

-- `lookup` is the random handle inside `rk_live_<lookup>_<secret>`; only the
-- secret's hash is stored, and the token exposes no row identity.
CREATE TABLE api_keys (
    id           UUID PRIMARY KEY DEFAULT uuidv7(),
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    lookup       TEXT UNIQUE NOT NULL,
    secret_hash  TEXT NOT NULL,
    prefix       TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ
);

-- Function sources are content-addressed, so identical pushes dedupe.
CREATE TABLE artifacts (
    hash        TEXT PRIMARY KEY,
    source      TEXT NOT NULL,
    size_bytes  BIGINT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE functions (
    name        TEXT PRIMARY KEY,
    user_id     UUID REFERENCES users(id) ON DELETE CASCADE,
    current_rev BIGINT NOT NULL,
    methods     TEXT[] NOT NULL,
    path        TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE revisions (
    function_name TEXT NOT NULL REFERENCES functions(name) ON DELETE CASCADE,
    rev           BIGINT NOT NULL,
    artifact_hash TEXT NOT NULL REFERENCES artifacts(hash),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (function_name, rev)
);

-- Plans are immutable, versioned rows: changing a plan inserts a new version,
-- and subscriptions bind to the exact version (and price) purchased.
CREATE TABLE plans (
    id          UUID PRIMARY KEY DEFAULT uuidv7(),
    code        TEXT NOT NULL,
    version     INT NOT NULL,
    name        TEXT NOT NULL,
    price_cents INT NOT NULL,
    limits      JSONB NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (code, version)
);

CREATE TABLE subscriptions (
    id          UUID PRIMARY KEY DEFAULT uuidv7(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    plan_id     UUID NOT NULL REFERENCES plans(id),
    status      TEXT NOT NULL DEFAULT 'active',
    started_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    canceled_at TIMESTAMPTZ
);

CREATE INDEX subscriptions_active ON subscriptions (user_id) WHERE status = 'active';

INSERT INTO plans (code, version, name, price_cents, limits) VALUES
  ('dev', 1, 'Dev', 0, '{
      "max_functions": 4,
      "max_script_bytes": 262144,
      "exec_ms": 50,
      "rate_per_min": 60,
      "outbound_reqs": 0,
      "analytics_days": 2
   }'),
  ('pro', 1, 'Pro', 1000, '{
      "max_functions": 10,
      "max_script_bytes": 1048576,
      "exec_ms": 500,
      "rate_per_min": 120,
      "outbound_reqs": 2,
      "analytics_days": 10
   }'),
  ('extra', 1, 'Extra', 5000, '{
      "max_functions": 50,
      "max_script_bytes": 5242880,
      "exec_ms": 30000,
      "rate_per_min": 600,
      "outbound_reqs": 10,
      "analytics_days": 30
   }');
