-- The email GitHub reports for the account, refreshed at every sign-in.
-- Nullable: GitHub only reveals it with the user:email scope, and accounts
-- that signed in before this column existed backfill on their next login.
ALTER TABLE users ADD COLUMN email TEXT;
