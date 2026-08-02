-- Plans are immutable: a new version rather than an edit, so anyone already
-- subscribed keeps exactly what they signed up for until they re-subscribe.
--
-- v2 makes Dev generous enough to demonstrate every capability — including
-- fetch, without which an example that calls an API is impossible on the free
-- tier — and gives every plan a concurrency allowance. Concurrency was fixed
-- at 1, which is a capacity choice, not a correctness one: each invocation
-- already gets its own context, so parallel calls were always safe.
INSERT INTO plans (code, version, name, price_cents, limits) VALUES
  ('dev', 2, 'Dev', 0, '{
      "max_functions": 5,
      "max_script_bytes": 262144,
      "exec_ms": 100,
      "rate_per_min": 60,
      "outbound_reqs": 2,
      "concurrency": 2,
      "analytics_days": 2
   }'),
  ('pro', 2, 'Pro', 1000, '{
      "max_functions": 10,
      "max_script_bytes": 1048576,
      "exec_ms": 500,
      "rate_per_min": 120,
      "outbound_reqs": 10,
      "concurrency": 5,
      "analytics_days": 10
   }'),
  ('extra', 2, 'Extra', 5000, '{
      "max_functions": 50,
      "max_script_bytes": 5242880,
      "exec_ms": 30000,
      "rate_per_min": 600,
      "outbound_reqs": 25,
      "concurrency": 20,
      "analytics_days": 30
   }');
