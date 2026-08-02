-- An internal plan with the policy limits taken out of the way, so load tests
-- measure the machine instead of the rate limiter. Without it the Dev ceiling
-- of 60 requests/minute is hit roughly 250x before any hardware is stressed.
--
-- Not on offer: `checkout` only accepts publicly listed codes, so this cannot
-- be self-assigned by naming it in a URL. Grant it deliberately, in SQL.
--
-- These are large numbers rather than sentinels, because every limit here is
-- already "the biggest value that means yes" everywhere it is read — except
-- rate_per_min, where <= 0 already means unlimited.
INSERT INTO plans (code, version, name, price_cents, limits) VALUES
  ('unlimited', 1, 'Unlimited (internal)', 0, '{
      "max_functions": 1000000,
      "max_script_bytes": 104857600,
      "exec_ms": 300000,
      "rate_per_min": 0,
      "outbound_reqs": 1000000,
      "concurrency": 1000000,
      "analytics_days": 365
   }');
