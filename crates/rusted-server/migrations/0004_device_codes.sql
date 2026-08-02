-- The OAuth device flow (RFC 8628): a CLI that can't open a browser starts a
-- request here, a human approves it in the console, and the CLI polls until a
-- key is minted. The device code is hashed like every other credential; the
-- user code is short enough to read aloud and typed by a person.
CREATE TABLE device_codes (
    id               UUID PRIMARY KEY DEFAULT uuidv7(),
    device_code_hash TEXT UNIQUE NOT NULL,
    user_code        TEXT UNIQUE NOT NULL,
    label            TEXT NOT NULL,
    user_id          UUID REFERENCES users(id) ON DELETE CASCADE,
    approved_at      TIMESTAMPTZ,
    denied_at        TIMESTAMPTZ,
    -- Set once the CLI has collected the key, so a code can't be redeemed twice.
    redeemed_at      TIMESTAMPTZ,
    expires_at       TIMESTAMPTZ NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX device_codes_user_code ON device_codes (user_code) WHERE redeemed_at IS NULL;
