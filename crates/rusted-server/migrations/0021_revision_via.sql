-- Which surface pushed each revision: 'cli' (the API with a key), 'editor'
-- (the console web editor), or 'agent' (the MCP deploy tool). Provenance for
-- the function page and the fork warnings — pre-existing rows default to
-- 'cli', the only surface that existed for most of history.
ALTER TABLE revisions ADD COLUMN via TEXT NOT NULL DEFAULT 'cli';
