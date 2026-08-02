-- Plans are immutable: a new version rather than an edit, so anyone already
-- subscribed keeps exactly what they signed up for until they re-subscribe.
--
-- v2 raised Dev's outbound allowance so the free tier could demonstrate
-- `fetch`, but left exec_ms at 100 — and a real API call does not fit in
-- 100ms. Measured against GitHub from the server: 126ms for the cheapest
-- endpoint, 306ms for a repository. So fetch was allowed and then made
-- unusable, and the tier could not show the thing the product is for.
--
-- v3 gives Dev 1000ms: about 3x the slowest call measured, and still well
-- under the 1500ms a caller waits for a worker slot before being turned away,
-- so one slow free-tier invocation cannot push other people's requests into
-- 429s. Pro gets 5s for work that chains several calls.
--
-- Raising these is only safe now that a fetch is bounded by the invocation's
-- remaining time. Before that, exec_ms bounded JavaScript alone: a blocking
-- fetch runs no bytecode, so the interrupt could not fire and timeouts
-- accumulated on top of the budget. Extra's true worst case was
-- 30s + 25 x 30s = 780s of one worker slot; it is now 30s.
INSERT INTO plans (code, version, name, price_cents, limits) VALUES
  ('dev', 3, 'Dev', 0, '{
      "max_functions": 5,
      "max_script_bytes": 262144,
      "exec_ms": 1000,
      "rate_per_min": 60,
      "outbound_reqs": 2,
      "concurrency": 2,
      "analytics_days": 2
   }'),
  ('pro', 3, 'Pro', 1000, '{
      "max_functions": 10,
      "max_script_bytes": 1048576,
      "exec_ms": 5000,
      "rate_per_min": 120,
      "outbound_reqs": 10,
      "concurrency": 5,
      "analytics_days": 10
   }'),
  ('extra', 3, 'Extra', 5000, '{
      "max_functions": 50,
      "max_script_bytes": 5242880,
      "exec_ms": 30000,
      "rate_per_min": 600,
      "outbound_reqs": 25,
      "concurrency": 20,
      "analytics_days": 30
   }');
