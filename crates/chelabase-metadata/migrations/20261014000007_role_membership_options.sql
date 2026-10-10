-- Members end-review I2: the options of a role's memberships (PostgreSQL 16+ `pg_auth_members`).
-- `member_of` lists every direct membership; these two columns name the ones that differ from
-- what the API grants (`SET TRUE, ADMIN FALSE`):
--   member_no_set  memberships WITHOUT the SET option (the role can't `SET ROLE` to them);
--   member_admin   memberships WITH the ADMIN option (the role can grant the group to others).
-- Both are subsets of `member_of`, sorted, and written by the resync from the catalog. Rows
-- start empty: the first resync after the upgrade records what the catalog has.
ALTER TABLE neon_roles
    ADD COLUMN member_no_set text[] NOT NULL DEFAULT '{}',
    ADD COLUMN member_admin  text[] NOT NULL DEFAULT '{}';
