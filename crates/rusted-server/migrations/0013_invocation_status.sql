-- The HTTP status an invocation answered with — the half of the story
-- `outcome` alone cannot tell. A handler that returns a 403 error envelope
-- completed successfully as far as the engine cares, but the owner reading
-- their logs cares that callers were being refused. NULL for mcp tool calls
-- (no HTTP status of their own) and rows from before this column.
ALTER TABLE invocations ADD COLUMN status SMALLINT;
