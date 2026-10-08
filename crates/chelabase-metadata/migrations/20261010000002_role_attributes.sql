-- Step 6g (U1, U2): role attributes and memberships. For an `api` row they're what the
-- control plane wants (the resync restores them); for an `sql` row, what the catalog last
-- showed. `login` already exists (20261004000001_sql_roles.sql). A NOLOGIN `api` row has
-- no verifier and no sealed password: neither column is NOT NULL for non-owner rows.
ALTER TABLE neon_roles
    ADD COLUMN inherit          boolean     NOT NULL DEFAULT true,
    ADD COLUMN connection_limit integer     NOT NULL DEFAULT -1,
    ADD COLUMN valid_until      timestamptz NULL,
    ADD COLUMN bypass_rls       boolean     NOT NULL DEFAULT false,
    ADD COLUMN member_of        text[]      NOT NULL DEFAULT '{}',
    ADD CONSTRAINT neon_roles_connection_limit CHECK (connection_limit >= -1);
