tabs AS (
  SELECT c.oid, c.relname, c.relowner, c.relacl, c.relrowsecurity, c.relforcerowsecurity,
         c.relpersistence, c.reloptions, n.nspname,
         (c.oid = (SELECT oid FROM users_tab)) IS TRUE AS is_users
  FROM pg_class c JOIN ns n ON n.oid = c.relnamespace
  WHERE c.relkind = 'r' AND NOT c.relispartition
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)
    -- A table that INHERITS from another is reported (inherited_table), not read as a table.
    AND NOT EXISTS (SELECT FROM pg_inherits ih WHERE ih.inhrelid = c.oid)
