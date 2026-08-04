-- MCP-type functions: the kind discriminates the data-plane protocol, and
-- mcp holds the deploy-time tool metadata (handlers stripped) for the
-- revision currently being served.
ALTER TABLE functions
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'http' CHECK (kind IN ('http', 'mcp')),
    ADD COLUMN mcp JSONB;
