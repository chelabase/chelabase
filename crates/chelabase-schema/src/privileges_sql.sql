-- The members hub's privileges read (`privileges::privileges_sql`): one page of one role's ACL
-- entries, every grantor (MH-24). One jsonb value, `privileges::PrivilegesPage`.
--
-- $1 role, $2 schema, $3 after, $4 limit (1..1000; NULL is 200).
-- * $2 NULL: the schema list (paged by name, `after` a schema name) and the default ACLs of the
--   page's schemas (`IN SCHEMA` rows only); no objects.
-- * $2 a schema: every relation, sequence, function and procedure of it (paged by
--   (name, class, oid), `after` the `next_after` of the previous page), each with the role's
--   entries, its columns' entries, the hub's all-rows policy for the role (MH-26: found by
--   shape and the comment marker, not by name), `acl_hash` and `owner_settable`.
-- A marked role (a personal login, MH-13) or a member login role `cb_m_*` (MH-32) reads as
-- having no entries anywhere.
--
-- Fragments between at signs (`entries`, `owner_settable`, `acl_hash`, `rel_acl_hash`) are
-- expanded by `privileges.rs`, so comments here never contain one.
--
-- `acl_hash` (P10, S3) is the expression `acl_hashes_sql` uses too: hex sha256 over the whole ACL
-- (every grantee) as sorted (column, grantee, privilege, grantor, grantable) rows; a relation's
-- also covers its columns' ACLs. T4-4: a NULL ACL is hashed as its default
-- (`acldefault(kind, owner)`), so it hashes apart from an empty one (`{}`).
--
-- `owner_settable` (S4, P27a): the session's role is the object's owner, can SET to it (any
-- path; before Postgres 16 every membership can), or holds ADMIN on it directly or through an
-- INHERIT membership: what the orchestrator's run-as sequence (P26) can reach.
WITH
target AS (
  SELECT r.oid, r.rolname
  FROM pg_catalog.pg_roles r
  WHERE r.rolname = $1::text
    AND pg_catalog.lower(r.rolname) NOT LIKE 'cb\_m\_%'
    AND NOT EXISTS (
      SELECT FROM pg_catalog.pg_db_role_setting s
      WHERE s.setrole = r.oid AND s.setdatabase = 0
        AND EXISTS (SELECT FROM pg_catalog.unnest(s.setconfig) AS c
                    WHERE pg_catalog.starts_with(c, 'chelabase.managed_role=')))
  -- T4-1: `public` (lower case, exactly) is PUBLIC, grantee 0 in an ACL. No real role can be
  -- named so (Postgres refuses `CREATE ROLE "public"` and the rename to it as reserved), so the
  -- branch above never matches it and the two can't both apply.
  UNION ALL
  SELECT 0::pg_catalog.oid, 'public'::pg_catalog.name WHERE $1::text = 'public'
),
me AS (
  SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = current_user
),
lim AS (
  SELECT GREATEST(1, LEAST(coalesce($4::int, 200), 1000)) AS n
),
ext AS (
  SELECT d.classid, d.objid FROM pg_catalog.pg_depend d WHERE d.deptype = 'e'
),
ns AS (
  SELECT n.oid, n.nspname, n.nspowner, n.nspacl
  FROM pg_catalog.pg_namespace n
  WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast', 'chelabase', 'chelabase_merge', 'chelabase_guard')
    AND n.nspname NOT LIKE 'pg\_toast%'
    AND n.nspname NOT LIKE 'pg\_temp\_%'
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND e.objid = n.oid)
),
-- Schema mode.
schema_rows AS (
  SELECT n.*, pg_catalog.row_number() OVER (ORDER BY n.nspname COLLATE "C") AS rn
  FROM (
    SELECT n.* FROM ns n
    WHERE $2::text IS NULL
      AND ($3::text IS NULL OR n.nspname COLLATE "C" > $3::text COLLATE "C")
    ORDER BY n.nspname COLLATE "C"
    LIMIT (SELECT n FROM lim) + 1
  ) n
),
schema_page AS (
  SELECT s.* FROM schema_rows s WHERE s.rn <= (SELECT n FROM lim)
),
-- Object mode.
the_ns AS (
  SELECT n.* FROM ns n WHERE $2::text IS NOT NULL AND n.nspname = $2::text
),
objs AS (
  SELECT c.relname::text AS name, 0 AS cls, c.oid, c.relowner AS owner, c.relacl AS acl,
         CASE c.relkind WHEN 'S' THEN 's' ELSE 'r' END::"char" AS default_kind,
         CASE c.relkind
           WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized_view'
           WHEN 'p' THEN 'partitioned_table' WHEN 'f' THEN 'foreign_table' ELSE 'sequence'
         END AS kind,
         c.relkind, c.relrowsecurity AS rls, n.nspname
  FROM pg_catalog.pg_class c JOIN the_ns n ON n.oid = c.relnamespace
  WHERE c.relkind IN ('r', 'v', 'm', 'p', 'f', 'S')
  UNION ALL
  SELECT p.proname::text, 1, p.oid, p.proowner, p.proacl, 'f'::"char",
         CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END,
         NULL::"char", false, n.nspname
  FROM pg_catalog.pg_proc p JOIN the_ns n ON n.oid = p.pronamespace
  WHERE p.prokind IN ('f', 'p')
),
after_key AS (
  SELECT pg_catalog.split_part($3::text, '/', 1)::int AS cls,
         pg_catalog.split_part($3::text, '/', 2)::pg_catalog.oid AS oid,
         pg_catalog.substr($3::text,
           pg_catalog.length(pg_catalog.split_part($3::text, '/', 1))
           + pg_catalog.length(pg_catalog.split_part($3::text, '/', 2)) + 3) AS name
  WHERE $2::text IS NOT NULL AND $3::text IS NOT NULL
),
obj_rows AS (
  SELECT o.*, pg_catalog.row_number() OVER (ORDER BY o.name COLLATE "C", o.cls, o.oid) AS rn
  FROM (
    SELECT o.* FROM objs o
    WHERE NOT EXISTS (SELECT FROM after_key)
       OR EXISTS (SELECT FROM after_key a
                  WHERE o.name COLLATE "C" > a.name COLLATE "C"
                     OR (o.name = a.name AND (o.cls, o.oid) > (a.cls, a.oid)))
    ORDER BY o.name COLLATE "C", o.cls, o.oid
    LIMIT (SELECT n FROM lim) + 1
  ) o
),
obj_page AS (
  SELECT o.* FROM obj_rows o WHERE o.rn <= (SELECT n FROM lim)
)
SELECT pg_catalog.jsonb_build_object(
  'role', $1::text,
  'server_version_num', pg_catalog.current_setting('server_version_num')::int,
  'offered', pg_catalog.jsonb_build_object(
    'table', CASE WHEN pg_catalog.current_setting('server_version_num')::int >= 170000
                  THEN '["select","insert","update","delete","truncate","references","trigger","maintain"]'::jsonb
                  ELSE '["select","insert","update","delete","truncate","references","trigger"]'::jsonb END,
    'sequence', '["usage","select","update"]'::jsonb,
    'schema', '["usage","create"]'::jsonb,
    'function', '["execute"]'::jsonb),
  'schemas', (
    SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(
      'name', s.nspname,
      'owner', pg_catalog.pg_get_userbyid(s.nspowner),
      'owner_settable', @owner_settable(s.nspowner)@,
      'grants', @entries(coalesce(s.nspacl, pg_catalog.acldefault('n', s.nspowner)))@,
      'acl_hash', @acl_hash(coalesce(s.nspacl, pg_catalog.acldefault('n', s.nspowner)))@
    ) ORDER BY s.nspname COLLATE "C"), '[]'::jsonb)
    FROM schema_page s),
  'default_privileges', (
    SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(
      'for_role', pg_catalog.pg_get_userbyid(d.defaclrole),
      'schema', s.nspname,
      'object', CASE d.defaclobjtype WHEN 'r' THEN 'tables' WHEN 'S' THEN 'sequences' ELSE 'functions' END,
      'grants', @entries(d.defaclacl)@,
      'acl_hash', @acl_hash(d.defaclacl)@
    ) ORDER BY s.nspname COLLATE "C", pg_catalog.pg_get_userbyid(d.defaclrole) COLLATE "C", d.defaclobjtype), '[]'::jsonb)
    FROM pg_catalog.pg_default_acl d JOIN schema_page s ON s.oid = d.defaclnamespace
    WHERE d.defaclobjtype IN ('r', 'S', 'f')),
  'objects', (
    SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(
      'kind', o.kind,
      'schema', o.nspname,
      'name', o.name,
      'oid', o.oid::int8,
      'identity', CASE WHEN o.cls = 1 THEN pg_catalog.format('%I.%I(%s)', o.nspname, o.name,
                         pg_catalog.pg_get_function_identity_arguments(o.oid)) END,
      'owner', pg_catalog.pg_get_userbyid(o.owner),
      'owner_settable', @owner_settable(o.owner)@,
      'rls_enabled', o.rls,
      'grants', @entries(coalesce(o.acl, pg_catalog.acldefault(o.default_kind, o.owner)))@,
      'columns', CASE WHEN o.cls = 0 AND o.relkind <> 'S' THEN (
        SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(
          'name', att.attname,
          'grants', @entries(att.attacl)@
        ) ORDER BY att.attnum), '[]'::jsonb)
        FROM pg_catalog.pg_attribute att
        WHERE att.attrelid = o.oid AND att.attnum > 0 AND NOT att.attisdropped)
        ELSE '[]'::jsonb END,
      'all_rows', CASE WHEN o.cls = 0 THEN (
        SELECT pg_catalog.jsonb_build_object('policy', pol.polname)
        FROM pg_catalog.pg_policy pol, target t
        WHERE pol.polrelid = o.oid
          AND pol.polpermissive
          AND pol.polcmd = '*'
          AND pol.polroles = ARRAY[t.oid]
          AND pg_catalog.pg_get_expr(pol.polqual, pol.polrelid) = 'true'
          AND pg_catalog.pg_get_expr(pol.polwithcheck, pol.polrelid) = 'true'
          AND pg_catalog.obj_description(pol.oid, 'pg_policy') = 'chelabase:all_rows'
        ORDER BY pol.polname COLLATE "C"
        LIMIT 1) END,
      'acl_hash', CASE WHEN o.cls = 0 THEN @rel_acl_hash(o.oid, coalesce(o.acl, pg_catalog.acldefault(o.default_kind, o.owner)))@
                    ELSE @acl_hash(coalesce(o.acl, pg_catalog.acldefault(o.default_kind, o.owner)))@ END
    ) ORDER BY o.rn), '[]'::jsonb)
    FROM obj_page o),
  'next_after', CASE
    WHEN $2::text IS NULL THEN
      (SELECT s.nspname::text FROM schema_rows s
       WHERE s.rn = (SELECT n FROM lim) AND EXISTS (SELECT FROM schema_rows x WHERE x.rn > (SELECT n FROM lim)))
    ELSE
      (SELECT pg_catalog.format('%s/%s/%s', o.cls, o.oid, o.name) FROM obj_rows o
       WHERE o.rn = (SELECT n FROM lim) AND EXISTS (SELECT FROM obj_rows x WHERE x.rn > (SELECT n FROM lim)))
  END
)
