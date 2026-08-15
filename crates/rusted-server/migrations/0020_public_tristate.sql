-- `public` becomes tri-state. NULL is "undeclared": open on an open server,
-- gated under --require-auth — exactly what FALSE meant before, minus the
-- ability to say "private on purpose". TRUE stays "callable without a key
-- anywhere". FALSE now means "requires one of the owner's keys on every
-- call, even on an open server".
ALTER TABLE functions ALTER COLUMN public DROP NOT NULL;

-- Existing http rows holding FALSE never meant "explicitly private" — the
-- flag did nothing on open servers, so FALSE was simply the column default
-- for modules that said nothing. MCP rows keep their value: for them FALSE
-- has always genuinely meant key-required.
UPDATE functions SET public = NULL WHERE kind = 'http' AND public = FALSE;
