-- The app surface joins http and mcp as a data-plane kind.
ALTER TABLE functions DROP CONSTRAINT functions_kind_check;
ALTER TABLE functions ADD CONSTRAINT functions_kind_check
    CHECK (kind IN ('http', 'mcp', 'app'));
