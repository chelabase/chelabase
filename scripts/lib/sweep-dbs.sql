-- What `just sweep-dbs` deletes from the metadata database, in one transaction (psql -1 -f).
-- Every table with a foreign key to `databases` or `branches` is either listed here or reached by
-- an ON DELETE CASCADE / SET NULL from a listed table, `databases` or `branches`. Not a TRUNCATE
-- ... CASCADE: members, teams, tenants and projects stay. scripts/tests/sweep-dbs.sh checks this
-- list against the schema and runs it on a populated one; keep one `DELETE FROM <table>;` per line.
SELECT count(*) AS endpoints FROM neon_endpoints \gset n_
SELECT count(*) AS databases FROM databases \gset n_
-- Sealed refresh values of the app sessions of the projects whose auth rows go below.
DELETE FROM app_sessions;
-- project_auth.database_id and database_grant_overrides.database_id have no cascade.
DELETE FROM project_auth;
DELETE FROM database_grant_overrides;
-- Cascades to neon_roles and neon_databases; clears branches.endpoint_id.
DELETE FROM neon_endpoints;
-- Cascades to data_api_keys, merges, member_branch_roles, branch_auth_sync, ... One statement,
-- because branches.parent_branch_id has no cascade.
DELETE FROM branches;
-- Cascades to personal_logins (and so its neon_roles), saved_queries, merges.
DELETE FROM databases;
SELECT :n_endpoints || ' ' || :n_databases;
