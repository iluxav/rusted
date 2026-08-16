-- The Express-style app surface: kind 'app' functions carry their declared
-- route table (method + pattern; handlers stay in the module), captured at
-- deploy time from the rusted.app(...) builder exactly like mcp tool
-- metadata.
ALTER TABLE functions ADD COLUMN routes JSONB;
