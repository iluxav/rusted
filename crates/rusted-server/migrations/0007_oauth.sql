-- OAuth 2.1 authorization server, so hosted assistants can connect to /mcp.
--
-- A Bearer key works for anything that can set a header, but the clients this
-- is for — browser and cloud agents — cannot be handed one: they discover a
-- server, register themselves, and send the user through a browser. That is
-- what the MCP authorization spec requires, and it is all this exists to serve.
--
-- Access tokens are not stored here. They are issued as ordinary API keys, so
-- one lookup path, one cache, one plan resolution, and one place to revoke
-- them — the console list a user already has.

-- Registered dynamically (RFC 7591): a client cannot know this server in
-- advance, so it introduces itself and gets an id back. Public clients only —
-- no secret is issued, and PKCE is what proves possession instead.
CREATE TABLE oauth_clients (
    client_id     TEXT PRIMARY KEY,
    client_name   TEXT NOT NULL,
    -- Matched exactly at authorize and token time. Anything less invites the
    -- open-redirect the spec spends a section on.
    redirect_uris TEXT[] NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Authorization codes: hashed like every other credential here, single use,
-- and short-lived because a leaked code is a token.
CREATE TABLE oauth_codes (
    code_hash      TEXT PRIMARY KEY,
    client_id      TEXT NOT NULL REFERENCES oauth_clients(client_id) ON DELETE CASCADE,
    user_id        UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    redirect_uri   TEXT NOT NULL,
    -- S256 only. The spec requires PKCE, and `plain` would defeat it.
    code_challenge TEXT NOT NULL,
    -- RFC 8707: which server the token is meant for, recorded so a token
    -- cannot be minted for one resource and spent at another.
    resource       TEXT,
    expires_at     TIMESTAMPTZ NOT NULL,
    redeemed_at    TIMESTAMPTZ
);

CREATE INDEX oauth_codes_expiry ON oauth_codes (expires_at);
