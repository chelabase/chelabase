BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
SET LOCAL search_path = pg_catalog;
SET LOCAL DateStyle = 'ISO';
SET LOCAL TimeZone = 'UTC';
-- The users table named in 'chelabase.app_users_table' (when that table exists), as an OID in a
-- transaction-local setting: one SELECT can't name a table that may not exist. Every lookup goes
-- through pg_catalog, so a role that can't read the pointer (the database owner, for the table
-- API's reads) raises nothing: it finds the users table by Chelabase's guard trigger instead
-- (chelabase.users_guard, whose EXECUTE is revoked from PUBLIC, so no one else can attach it),
-- and hides nothing extra unless exactly one table carries it.
DO $chelabase_catalog$
DECLARE
  pointer oid;
  users oid;
BEGIN
  PERFORM set_config('chelabase_catalog.users_table', '', true);
  SELECT c.oid INTO pointer
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE n.nspname = 'chelabase'
    AND format('%I.%I', n.nspname, c.relname) = 'chelabase.app_users_table';
  IF pointer IS NULL THEN
    RETURN;
  END IF;
  IF has_schema_privilege('chelabase', 'USAGE') AND has_table_privilege(pointer, 'SELECT') THEN
    SELECT c.oid INTO users
    FROM chelabase.app_users_table u
    JOIN pg_namespace n ON n.nspname = u.schema_name
    JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = u.table_name
    LIMIT 1;
  ELSE
    SELECT min(tg.tgrelid) INTO users
    FROM pg_trigger tg
    JOIN pg_proc p ON p.oid = tg.tgfoid
    JOIN pg_namespace pn ON pn.oid = p.pronamespace
    WHERE tg.tgname = 'chelabase_users_guard' AND NOT tg.tgisinternal
      AND pn.nspname = 'chelabase' AND p.proname = 'users_guard'
    HAVING count(DISTINCT tg.tgrelid) = 1;
  END IF;
  PERFORM set_config('chelabase_catalog.users_table', coalesce(users::text, ''), true);
END
$chelabase_catalog$;
WITH
users_tab AS (
  SELECT nullif(current_setting('chelabase_catalog.users_table', true), '')::oid AS oid
),
ext AS (
  SELECT classid, objid FROM pg_depend WHERE deptype = 'e'
),
ns AS (
  SELECT n.oid, n.nspname, n.nspowner, n.nspacl
  FROM pg_namespace n
  WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast', 'chelabase', 'chelabase_merge', 'chelabase_guard')
    AND n.nspname NOT LIKE 'pg\_toast%'
    AND n.nspname NOT LIKE 'pg\_temp\_%'
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_namespace'::regclass AND e.objid = n.oid)
),
-- Roles that never appear as an ACL grantee: Chelabase's own and the hidden member login roles.
-- (Not all of `reserved`: grants to anon / authenticated stay.)
hidden AS (
  SELECT r.oid FROM pg_roles r
  WHERE lower(r.rolname) LIKE 'chelabase\_%'
     OR lower(r.rolname) LIKE 'cb\_m\_%'
),
