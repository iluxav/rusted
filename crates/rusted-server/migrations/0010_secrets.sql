-- Per-account secrets, decrypted into `context.env` for functions that ask
-- via `export const config = { secrets: [...] }`.
--
-- Values are sealed with AES-256-GCM before they reach this table; the key
-- lives in the server's environment (RUSTED_SECRETS_KEY), never in the
-- database, so a dump of this table alone reveals nothing. Each row stores
-- the 12-byte nonce followed by ciphertext+tag.
CREATE TABLE secrets (
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    ciphertext BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, name)
);

-- The secret names a function's module requested, captured at deploy time so
-- the invocation path knows what to inject without re-inspecting the source.
ALTER TABLE functions ADD COLUMN secrets TEXT[] NOT NULL DEFAULT '{}';
