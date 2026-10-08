-- Step 6g, T14 (E32: SQL controls every role). The resync records a role's catalog state
-- (LOGIN, owner-level, INHERIT, CONNECTION LIMIT, VALID UNTIL, BYPASSRLS, memberships) into
-- its row, for roles made through the API too. Only an API change that hasn't been applied
-- yet is driven onto the compute: `attributes_pending` marks a row whose wanted attributes an
-- API create or PATCH wrote and the compute hasn't confirmed. It is separate from `pending`,
-- which a password reset also sets (and must not re-apply attributes).
--
-- Existing rows start false: the first resync after the upgrade records what the catalog
-- has and reverts nothing.
--
-- `attributes_change_id` names the API write that set `attributes_pending`: a confirm clears
-- the flag only for its own change, whatever else (a forwarded DDL) bumped `roles_version`
-- meanwhile; a newer change keeps it set.
ALTER TABLE neon_roles
    ADD COLUMN attributes_pending boolean NOT NULL DEFAULT false,
    ADD COLUMN attributes_change_id uuid NULL;
