-- Somewhere for an agent to receive things.
--
-- A browser or cloud agent has no inbound address: it can call out, but nothing
-- can reach it. That rules out OAuth callbacks, webhooks, "notify me when this
-- finishes", and anything another party has to initiate. An inbox is a
-- throwaway URL that accepts a POST from anyone and holds it until the owner
-- reads it or it expires.
--
-- The write address and the read handle are deliberately different. `address`
-- is a capability: whoever holds it may write, and that is all. Reading is by
-- name and requires the owner's key. So handing the URL to Stripe never hands
-- over the ability to read what Stripe sent.
CREATE TABLE inboxes (
    id         UUID PRIMARY KEY DEFAULT uuidv7(),
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- How the owner asks for it back.
    name       TEXT NOT NULL,
    -- The public half. Unguessable, because it is the entire write credential.
    address    TEXT NOT NULL UNIQUE,
    -- 'append' keeps every POST; 'upsert' keeps only the most recent. append is
    -- the default because it is the one that cannot silently lose a message.
    store      TEXT NOT NULL DEFAULT 'append',
    -- Remove on first read, like a queue. At-most-once: if the read response is
    -- lost in flight the message is gone, which is fine for a single-use OAuth
    -- code and wrong for anything you cannot ask for again. Off by default.
    drain      BOOLEAN NOT NULL DEFAULT FALSE,
    -- Fixed at creation and never extended. Sliding expiry would let anyone
    -- holding the write URL keep the inbox — and its storage — alive forever.
    expires_at TIMESTAMPTZ NOT NULL,
    -- Total accepted writes, so a public endpoint cannot be used as an
    -- unbounded write primitive. Storage is capped by the message count, but
    -- 'upsert' overwrites in place, so load needs its own bound.
    writes     INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, name)
);

CREATE INDEX inboxes_expiry ON inboxes (expires_at);

CREATE TABLE inbox_messages (
    id          UUID PRIMARY KEY DEFAULT uuidv7(),
    inbox_id    UUID NOT NULL REFERENCES inboxes(id) ON DELETE CASCADE,
    -- Text, not bytes: the same UTF-8 rule as function bodies. A payload that
    -- is not text is refused rather than stored mangled.
    body        TEXT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX inbox_messages_by_inbox ON inbox_messages (inbox_id, received_at);
