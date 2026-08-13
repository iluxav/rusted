-- Whether a function is being served. Operational state, not a deploy-time
-- fact: the owner flips it from the console, and a push never touches it —
-- redeploying an unpublished function must not silently put it back on the
-- air. Unpublished answers exactly like missing (404) so the toggle reveals
-- nothing to callers; the owner's logs record the refusals.
ALTER TABLE functions ADD COLUMN published BOOLEAN NOT NULL DEFAULT TRUE;
