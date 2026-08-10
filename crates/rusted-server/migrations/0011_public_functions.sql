-- Functions that may be called without an API key even when the server runs
-- with --require-auth: OAuth callbacks and webhook targets are invoked by
-- third parties that cannot present the owner's key.
--
-- Declared in the module (`export const http = { public: true }`) and captured
-- at deploy time, like every other deploy-time fact. MCP functions have
-- declared this inside their metadata since 0009; the column becomes the one
-- place the auth gate checks, so backfill it for them.
ALTER TABLE functions ADD COLUMN public BOOLEAN NOT NULL DEFAULT FALSE;
UPDATE functions SET public = COALESCE((mcp->>'public')::boolean, FALSE) WHERE kind = 'mcp';
