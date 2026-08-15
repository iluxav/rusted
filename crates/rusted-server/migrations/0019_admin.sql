-- Platform administration. Nobody is an admin by default — the first admin is
-- appointed by hand on the server:
--   UPDATE users SET admin = TRUE WHERE email = '...';
ALTER TABLE users ADD COLUMN admin BOOLEAN NOT NULL DEFAULT FALSE;

-- Sign-in recency for the admin user list, stamped at session creation.
-- Backfilled from the newest session each account still holds; activity
-- older than the session table remembers is unknowable.
ALTER TABLE users ADD COLUMN last_login_at TIMESTAMPTZ;
UPDATE users u SET last_login_at = s.latest
FROM (SELECT user_id, max(created_at) AS latest FROM sessions GROUP BY user_id) s
WHERE s.user_id = u.id;
