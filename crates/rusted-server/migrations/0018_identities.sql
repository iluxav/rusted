-- One row per (provider, provider-account): the shape that lets a user sign
-- in with GitHub today and Google tomorrow and land in the same account.
-- users.github_id stays for now as a legacy column (dropped once nothing
-- reads it); new accounts may have it NULL.
CREATE TABLE identities (
    provider   TEXT NOT NULL,
    subject    TEXT NOT NULL,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, subject)
);

CREATE INDEX identities_by_user ON identities(user_id);

INSERT INTO identities (provider, subject, user_id)
    SELECT 'github', github_id::TEXT, id FROM users;

ALTER TABLE users ALTER COLUMN github_id DROP NOT NULL;
