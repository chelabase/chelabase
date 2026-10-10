BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
SET LOCAL search_path = pg_catalog;
SET LOCAL DateStyle = 'ISO';
SET LOCAL TimeZone = 'UTC';
-- The users table named in 'chelabase.app_users_table' (when that table exists), as an OID in a
-- transaction-local setting: one SELECT can't name a table that may not exist.
DO $chelabase_catalog$
BEGIN
  PERFORM set_config('chelabase_catalog.users_table', '', true);
  IF to_regclass('chelabase.app_users_table') IS NOT NULL THEN
    PERFORM set_config('chelabase_catalog.users_table',
      coalesce((SELECT to_regclass(format('%I.%I', u.schema_name, u.table_name))::oid::text
                FROM chelabase.app_users_table u LIMIT 1), ''),
      true);
  END IF;
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
  WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast', 'chelabase', 'chelabase_merge')
    AND n.nspname NOT LIKE 'pg\_toast%'
    AND n.nspname NOT LIKE 'pg\_temp\_%'
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_namespace'::regclass AND e.objid = n.oid)
),
reserved AS (
  SELECT r.oid FROM pg_roles r
  WHERE r.rolname IN ('authenticator', 'anon', 'authenticated', 'chelabase_admin',
                      'chelabase_auth_sync', 'cloud_admin', 'neon_superuser')
     OR r.rolname LIKE 'pg\_%'
     OR lower(r.rolname) LIKE 'chelabase\_%'
     OR lower(r.rolname) LIKE 'cb\_m\_%'
),
-- Roles that never appear as an ACL grantee: Chelabase's own and the hidden member login roles.
-- (Not all of `reserved`: grants to anon / authenticated stay.)
hidden AS (
  SELECT r.oid FROM pg_roles r
  WHERE lower(r.rolname) LIKE 'chelabase\_%'
     OR lower(r.rolname) LIKE 'cb\_m\_%'
),
tabs AS (
  SELECT c.oid, c.relname, c.relowner, c.relacl, c.relrowsecurity, c.relforcerowsecurity,
         c.relpersistence, c.reloptions, n.nspname,
         (c.oid = (SELECT oid FROM users_tab)) IS TRUE AS is_users
  FROM pg_class c JOIN ns n ON n.oid = c.relnamespace
  WHERE c.relkind = 'r' AND NOT c.relispartition
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)
    -- A table that INHERITS from another is reported (inherited_table), not read as a table.
    AND NOT EXISTS (SELECT FROM pg_inherits ih WHERE ih.inhrelid = c.oid)
),
schemas_j AS (
  SELECT n.nspname AS k, jsonb_build_object(
    'oid', n.oid::int8,
    'name', n.nspname,
    'owner', jsonb_build_object('oid', o.oid::int8, 'name', o.rolname),
    'comment', obj_description(n.oid, 'pg_namespace'),
    'acl', (SELECT coalesce(jsonb_agg(jsonb_build_object(
              'grantor', jsonb_build_object('oid', a.grantor::int8, 'name', gr.rolname),
              'grantee', jsonb_build_object('oid', a.grantee::int8, 'name', ge.rolname),
              'privilege', a.privilege_type, 'grantable', a.is_grantable)
              ORDER BY a.grantee, a.privilege_type, a.grantor), '[]'::jsonb)
            FROM aclexplode(coalesce(n.nspacl, acldefault('n', n.nspowner))) a
            LEFT JOIN pg_roles gr ON gr.oid = a.grantor
            LEFT JOIN pg_roles ge ON ge.oid = a.grantee
            WHERE a.grantee NOT IN (SELECT oid FROM hidden))
  ) AS j
  FROM ns n JOIN pg_roles o ON o.oid = n.nspowner
),
tables_j AS (
  SELECT t.nspname || '.' || t.relname AS k, jsonb_build_object(
    'oid', t.oid::int8,
    'schema', t.nspname,
    'name', t.relname,
    'owner', jsonb_build_object('oid', o.oid::int8, 'name', o.rolname),
    'comment', obj_description(t.oid, 'pg_class'),
    'rls_enabled', t.relrowsecurity,
    'rls_forced', t.relforcerowsecurity,
    'persistence', t.relpersistence,
    'options', coalesce(to_jsonb(t.reloptions), '[]'::jsonb),
    'acl', (SELECT coalesce(jsonb_agg(jsonb_build_object(
              'grantor', jsonb_build_object('oid', a.grantor::int8, 'name', gr.rolname),
              'grantee', jsonb_build_object('oid', a.grantee::int8, 'name', ge.rolname),
              'privilege', a.privilege_type, 'grantable', a.is_grantable)
              ORDER BY a.grantee, a.privilege_type, a.grantor), '[]'::jsonb)
            FROM aclexplode(coalesce(t.relacl, acldefault('r', t.relowner))) a
            LEFT JOIN pg_roles gr ON gr.oid = a.grantor
            LEFT JOIN pg_roles ge ON ge.oid = a.grantee
            WHERE a.grantee NOT IN (SELECT oid FROM hidden)
              AND NOT (t.is_users
                       AND coalesce(ge.rolname, '') IN ('authenticated', 'chelabase_auth_sync'))),
    'columns', (SELECT coalesce(jsonb_agg(jsonb_build_object(
        'attnum', a.attnum,
        'name', a.attname,
        'data_type', format_type(a.atttypid, a.atttypmod),
        'type_oid', a.atttypid::int8,
        'collation', CASE WHEN a.attcollation <> ty.typcollation THEN
            (SELECT quote_ident(cn.nspname) || '.' || quote_ident(co.collname)
             FROM pg_collation co JOIN pg_namespace cn ON cn.oid = co.collnamespace
             WHERE co.oid = a.attcollation) END,
        'default_expr', pg_get_expr(ad.adbin, ad.adrelid),
        'generated', a.attgenerated,
        'identity', a.attidentity,
        'not_null', a.attnotnull,
        'identity_sequence', (
          SELECT jsonb_build_object(
            'oid', s.oid::int8,
            'name', s.relname,
            'params', jsonb_build_object(
              'data_type', format_type(sq.seqtypid, NULL), 'start', sq.seqstart,
              'increment', sq.seqincrement, 'min', sq.seqmin, 'max', sq.seqmax,
              'cache', sq.seqcache, 'cycle', sq.seqcycle),
            'acl', (SELECT coalesce(jsonb_agg(jsonb_build_object(
                      'grantor', jsonb_build_object('oid', sa.grantor::int8, 'name', sgr.rolname),
                      'grantee', jsonb_build_object('oid', sa.grantee::int8, 'name', sge.rolname),
                      'privilege', sa.privilege_type, 'grantable', sa.is_grantable)
                      ORDER BY sa.grantee, sa.privilege_type, sa.grantor), '[]'::jsonb)
                    FROM aclexplode(coalesce(s.relacl, acldefault('s', s.relowner))) sa
                    LEFT JOIN pg_roles sgr ON sgr.oid = sa.grantor
                    LEFT JOIN pg_roles sge ON sge.oid = sa.grantee
                    WHERE sa.grantee NOT IN (SELECT oid FROM hidden)))
          FROM pg_depend d
          JOIN pg_class s ON s.oid = d.objid AND s.relkind = 'S'
          JOIN pg_sequence sq ON sq.seqrelid = s.oid
          WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass
            AND d.refobjid = t.oid AND d.refobjsubid = a.attnum AND d.deptype = 'i'),
        'comment', col_description(t.oid, a.attnum),
        'acl', (SELECT coalesce(jsonb_agg(jsonb_build_object(
                  'grantor', jsonb_build_object('oid', ca.grantor::int8, 'name', cgr.rolname),
                  'grantee', jsonb_build_object('oid', ca.grantee::int8, 'name', cge.rolname),
                  'privilege', ca.privilege_type, 'grantable', ca.is_grantable)
                  ORDER BY ca.grantee, ca.privilege_type, ca.grantor), '[]'::jsonb)
                FROM aclexplode(a.attacl) ca
                LEFT JOIN pg_roles cgr ON cgr.oid = ca.grantor
                LEFT JOIN pg_roles cge ON cge.oid = ca.grantee
                WHERE ca.grantee NOT IN (SELECT oid FROM hidden))
      ) ORDER BY a.attnum), '[]'::jsonb)
      FROM pg_attribute a
      JOIN pg_type ty ON ty.oid = a.atttypid
      LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
      WHERE a.attrelid = t.oid AND a.attnum > 0 AND NOT a.attisdropped
        AND NOT (t.is_users AND a.attname IN ('id', 'email', 'email_verified', 'created_at', 'deleted_at'))),
    'constraints', (SELECT coalesce(jsonb_agg(jsonb_build_object(
        'oid', k.oid::int8,
        'name', k.conname,
        'contype', k.contype,
        'definition', pg_get_constraintdef(k.oid, true),
        'columns', coalesce(to_jsonb(k.conkey), '[]'::jsonb),
        'references', CASE WHEN k.contype = 'f' THEN jsonb_build_object(
            'table_oid', k.confrelid::int8, 'schema', fn.nspname, 'table', fc.relname,
            'columns', coalesce(to_jsonb(k.confkey), '[]'::jsonb)) END,
        'deferrable', k.condeferrable,
        'initially_deferred', k.condeferred,
        'validated', k.convalidated
      ) ORDER BY k.conname, k.oid), '[]'::jsonb)
      FROM pg_constraint k
      LEFT JOIN pg_class fc ON fc.oid = k.confrelid
      LEFT JOIN pg_namespace fn ON fn.oid = fc.relnamespace
      WHERE k.conrelid = t.oid AND k.contype <> 't'
        AND k.conname NOT LIKE 'chelabase\_merge\_nn\_%'
        -- The users table's constraints on its managed columns only (its primary key).
        AND NOT (t.is_users AND k.conkey IS NOT NULL AND k.conkey <@ ARRAY(
              SELECT ma.attnum FROM pg_attribute ma
              WHERE ma.attrelid = t.oid
                AND ma.attname IN ('id', 'email', 'email_verified', 'created_at', 'deleted_at')))),
    'indexes', (SELECT coalesce(jsonb_agg(jsonb_build_object(
        'oid', i.indexrelid::int8,
        'name', ic.relname,
        'method', am.amname,
        'unique', i.indisunique,
        'nulls_not_distinct', i.indnullsnotdistinct,
        'definition', pg_get_indexdef(i.indexrelid, 0, true),
        'key_count', i.indnkeyatts,
        'include', (SELECT coalesce(jsonb_agg(pg_get_indexdef(i.indexrelid, g, true) ORDER BY g), '[]'::jsonb)
                    FROM generate_series(i.indnkeyatts + 1, i.indnatts) g),
        'predicate', pg_get_expr(i.indpred, i.indrelid, true),
        'options', coalesce(to_jsonb(ic.reloptions), '[]'::jsonb),
        'valid', i.indisvalid AND i.indisready,
        'comment', obj_description(i.indexrelid, 'pg_class')
      ) ORDER BY ic.relname), '[]'::jsonb)
      FROM pg_index i
      JOIN pg_class ic ON ic.oid = i.indexrelid
      JOIN pg_am am ON am.oid = ic.relam
      WHERE i.indrelid = t.oid AND i.indislive
        AND NOT EXISTS (SELECT FROM pg_constraint k
                        WHERE k.conindid = i.indexrelid AND k.conrelid = t.oid
                          AND k.contype IN ('p', 'u', 'x'))
        AND ic.relname NOT LIKE 'chelabase\_merge\_ix\_%'
        AND NOT (t.is_users AND ic.relname = 'chelabase_users_email_key')),
    'policies', (SELECT coalesce(jsonb_agg(jsonb_build_object(
        'oid', p.oid::int8,
        'name', p.polname,
        'command', p.polcmd,
        'permissive', p.polpermissive,
        'roles', (SELECT coalesce(jsonb_agg(jsonb_build_object('oid', pr.r::int8, 'name', rr.rolname)
                    ORDER BY pr.r), '[]'::jsonb)
                  FROM unnest(p.polroles) pr(r) LEFT JOIN pg_roles rr ON rr.oid = pr.r),
        'using', pg_get_expr(p.polqual, p.polrelid),
        'with_check', pg_get_expr(p.polwithcheck, p.polrelid),
        'comment', obj_description(p.oid, 'pg_policy')
      ) ORDER BY p.polname), '[]'::jsonb)
      FROM pg_policy p
      WHERE p.polrelid = t.oid
        AND NOT (t.is_users AND p.polname IN ('chelabase_users_select_own', 'chelabase_users_update_own')))
  ) AS j
  FROM tabs t JOIN pg_roles o ON o.oid = t.relowner
),
enums_j AS (
  SELECT n.nspname || '.' || ty.typname AS k, jsonb_build_object(
    'oid', ty.oid::int8,
    'schema', n.nspname,
    'name', ty.typname,
    'owner', jsonb_build_object('oid', o.oid::int8, 'name', o.rolname),
    'comment', obj_description(ty.oid, 'pg_type'),
    'labels', (SELECT coalesce(jsonb_agg(jsonb_build_object('oid', e.oid::int8, 'label', e.enumlabel)
                 ORDER BY e.enumsortorder), '[]'::jsonb)
               FROM pg_enum e WHERE e.enumtypid = ty.oid)
  ) AS j
  FROM pg_type ty JOIN ns n ON n.oid = ty.typnamespace JOIN pg_roles o ON o.oid = ty.typowner
  WHERE ty.typtype = 'e'
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_type'::regclass AND e.objid = ty.oid)
),
sequences_j AS (
  SELECT n.nspname || '.' || s.relname AS k, jsonb_build_object(
    'oid', s.oid::int8,
    'schema', n.nspname,
    'name', s.relname,
    'owner', jsonb_build_object('oid', o.oid::int8, 'name', o.rolname),
    'comment', obj_description(s.oid, 'pg_class'),
    'params', jsonb_build_object(
      'data_type', format_type(sq.seqtypid, NULL), 'start', sq.seqstart,
      'increment', sq.seqincrement, 'min', sq.seqmin, 'max', sq.seqmax,
      'cache', sq.seqcache, 'cycle', sq.seqcycle),
    'owned_by', (SELECT jsonb_build_object(
                   'table_oid', d.refobjid::int8, 'schema', tn.nspname, 'table', tc.relname,
                   'attnum', d.refobjsubid, 'column', ta.attname)
                 FROM pg_depend d
                 JOIN pg_class tc ON tc.oid = d.refobjid
                 JOIN pg_namespace tn ON tn.oid = tc.relnamespace
                 JOIN pg_attribute ta ON ta.attrelid = d.refobjid AND ta.attnum = d.refobjsubid
                 WHERE d.classid = 'pg_class'::regclass AND d.objid = s.oid
                   AND d.refclassid = 'pg_class'::regclass AND d.deptype = 'a'
                   AND d.refobjsubid > 0),
    'acl', (SELECT coalesce(jsonb_agg(jsonb_build_object(
              'grantor', jsonb_build_object('oid', a.grantor::int8, 'name', gr.rolname),
              'grantee', jsonb_build_object('oid', a.grantee::int8, 'name', ge.rolname),
              'privilege', a.privilege_type, 'grantable', a.is_grantable)
              ORDER BY a.grantee, a.privilege_type, a.grantor), '[]'::jsonb)
            FROM aclexplode(coalesce(s.relacl, acldefault('s', s.relowner))) a
            LEFT JOIN pg_roles gr ON gr.oid = a.grantor
            LEFT JOIN pg_roles ge ON ge.oid = a.grantee
            WHERE a.grantee NOT IN (SELECT oid FROM hidden))
  ) AS j
  FROM pg_class s
  JOIN ns n ON n.oid = s.relnamespace
  JOIN pg_roles o ON o.oid = s.relowner
  JOIN pg_sequence sq ON sq.seqrelid = s.oid
  WHERE s.relkind = 'S'
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = s.oid)
    AND NOT EXISTS (SELECT FROM pg_depend d
                    WHERE d.classid = 'pg_class'::regclass AND d.objid = s.oid AND d.deptype = 'i')
),
roles_j AS (
  SELECT r.rolname AS k, jsonb_build_object(
    'oid', r.oid::int8,
    'name', r.rolname,
    'login', r.rolcanlogin,
    'inherit', r.rolinherit,
    'connection_limit', r.rolconnlimit,
    'valid_until', r.rolvaliduntil::text,
    'bypass_rls', r.rolbypassrls,
    'member_of', (SELECT coalesce(jsonb_agg(jsonb_build_object(
                    'role', jsonb_build_object('oid', mr.oid::int8, 'name', mr.rolname),
                    'grantor', jsonb_build_object('oid', mg.oid::int8, 'name', mg.rolname),
                    'admin', m.admin_option, 'inherit', m.inherit_option, 'set', m.set_option)
                    ORDER BY mr.rolname, mg.rolname, m.oid), '[]'::jsonb)
                  FROM pg_auth_members m
                  JOIN pg_roles mr ON mr.oid = m.roleid
                  JOIN pg_roles mg ON mg.oid = m.grantor
                  WHERE m.member = r.oid
                    AND mr.oid NOT IN (SELECT oid FROM hidden)
                    AND mg.oid NOT IN (SELECT oid FROM hidden)),
    'other_attributes', to_jsonb(array_remove(ARRAY[
        CASE WHEN r.rolsuper THEN 'SUPERUSER' END,
        CASE WHEN r.rolcreatedb THEN 'CREATEDB' END,
        CASE WHEN r.rolcreaterole THEN 'CREATEROLE' END,
        CASE WHEN r.rolreplication THEN 'REPLICATION' END], NULL)),
    'comment', shobj_description(r.oid, 'pg_authid')
  ) AS j
  FROM pg_roles r
  WHERE r.oid NOT IN (SELECT oid FROM reserved)
),
default_privileges_raw AS (
  SELECT d.oid, dr.rolname, dn.nspname, d.defaclnamespace, jsonb_build_object(
    'oid', d.oid::int8,
    'role', jsonb_build_object('oid', dr.oid::int8, 'name', dr.rolname),
    'schema', dn.nspname,
    'object_type', d.defaclobjtype,
    -- The data API's per-schema grants to anon / authenticated (on the owner's rows) are
    -- excluded.
    'acl', (SELECT coalesce(jsonb_agg(jsonb_build_object(
              'grantor', jsonb_build_object('oid', a.grantor::int8, 'name', gr.rolname),
              'grantee', jsonb_build_object('oid', a.grantee::int8, 'name', ge.rolname),
              'privilege', a.privilege_type, 'grantable', a.is_grantable)
              ORDER BY a.grantee, a.privilege_type, a.grantor), '[]'::jsonb)
            FROM aclexplode(d.defaclacl) a
            LEFT JOIN pg_roles gr ON gr.oid = a.grantor
            LEFT JOIN pg_roles ge ON ge.oid = a.grantee
            WHERE a.grantee NOT IN (SELECT oid FROM hidden)
              AND NOT (d.defaclnamespace <> 0 AND dr.rolname = 'postgres'
                       AND coalesce(ge.rolname, '') IN ('anon', 'authenticated')))
  ) AS j
  FROM pg_default_acl d
  JOIN pg_roles dr ON dr.oid = d.defaclrole
  LEFT JOIN pg_namespace dn ON dn.oid = d.defaclnamespace
  WHERE d.defaclrole NOT IN (SELECT oid FROM reserved)
    AND (d.defaclnamespace = 0 OR d.defaclnamespace IN (SELECT oid FROM ns))
    -- E16: the data API's own global row, exactly (FOR ROLE postgres REVOKE EXECUTE ON
    -- FUNCTIONS FROM PUBLIC leaves the owner's EXECUTE alone).
    AND NOT (d.defaclnamespace = 0 AND d.defaclobjtype = 'f' AND dr.rolname = 'postgres'
         AND d.defaclacl = ARRAY[makeaclitem(d.defaclrole, d.defaclrole, 'EXECUTE', false)])
),
default_privileges_j AS (
  SELECT rolname || '.' || coalesce(nspname, '') || '.' || (j->>'object_type') AS k, j
  FROM default_privileges_raw
  WHERE defaclnamespace = 0 OR j->'acl' <> '[]'::jsonb
),
reported_rows (kind, oid, schema, name, parent, arguments, definition) AS (
  -- Partitioned tables, partitions, foreign tables, views, materialized views.
  SELECT CASE WHEN c.relispartition THEN 'partition'
              WHEN c.relkind = 'p' THEN 'partitioned_table'
              WHEN c.relkind = 'f' THEN 'foreign_table'
              WHEN c.relkind = 'v' THEN 'view'
              ELSE 'materialized_view' END,
         c.oid, n.nspname, c.relname,
         CASE WHEN c.relispartition THEN
           (SELECT pc.relname FROM pg_inherits ih JOIN pg_class pc ON pc.oid = ih.inhparent
            WHERE ih.inhrelid = c.oid LIMIT 1) END,
         NULL,
         CASE WHEN c.relispartition THEN pg_get_expr(c.relpartbound, c.oid, true)
              WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid)
              WHEN c.relkind IN ('v', 'm') THEN pg_get_viewdef(c.oid, true) END
  FROM pg_class c JOIN ns n ON n.oid = c.relnamespace
  WHERE (c.relkind IN ('p', 'f', 'v', 'm') OR (c.relkind = 'r' AND c.relispartition))
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)
  UNION ALL
  -- Tables that INHERIT from another (not partitions): their first parent, and every parent.
  SELECT 'inherited_table', c.oid, n.nspname, c.relname,
         (SELECT pc.relname FROM pg_inherits ih JOIN pg_class pc ON pc.oid = ih.inhparent
          WHERE ih.inhrelid = c.oid ORDER BY ih.inhseqno LIMIT 1),
         NULL,
         'INHERITS (' || (SELECT string_agg(ih.inhparent::regclass::text, ', ' ORDER BY ih.inhseqno)
                          FROM pg_inherits ih WHERE ih.inhrelid = c.oid) || ')'
  FROM pg_class c JOIN ns n ON n.oid = c.relnamespace
  WHERE c.relkind = 'r' AND NOT c.relispartition
    AND EXISTS (SELECT FROM pg_inherits ih WHERE ih.inhrelid = c.oid)
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)
  UNION ALL
  -- Functions, procedures, aggregates, window functions.
  SELECT CASE p.prokind WHEN 'f' THEN 'function' WHEN 'p' THEN 'procedure'
                        WHEN 'a' THEN 'aggregate' ELSE 'window_function' END,
         p.oid, n.nspname, p.proname, NULL, pg_get_function_identity_arguments(p.oid),
         CASE WHEN p.prokind <> 'a' THEN pg_get_functiondef(p.oid) END
  FROM pg_proc p JOIN ns n ON n.oid = p.pronamespace
  WHERE NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_proc'::regclass AND e.objid = p.oid)
  UNION ALL
  -- Triggers (not internal ones; not the users table's guard triggers).
  SELECT 'trigger', tg.oid, n.nspname, tg.tgname, c.relname, NULL, pg_get_triggerdef(tg.oid, true)
  FROM pg_trigger tg
  JOIN pg_class c ON c.oid = tg.tgrelid
  JOIN ns n ON n.oid = c.relnamespace
  WHERE NOT tg.tgisinternal
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)
    AND NOT ((c.oid = (SELECT oid FROM users_tab)) IS TRUE
             AND tg.tgname IN ('chelabase_users_guard', 'chelabase_users_guard_truncate'))
  UNION ALL
  -- Rules (not a view's own _RETURN rule).
  SELECT 'rule', r.oid, n.nspname, r.rulename, c.relname, NULL, pg_get_ruledef(r.oid, true)
  FROM pg_rewrite r
  JOIN pg_class c ON c.oid = r.ev_class
  JOIN ns n ON n.oid = c.relnamespace
  WHERE r.rulename <> '_RETURN'
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)
  UNION ALL
  -- Domains, composite types, range types, base types (not arrays).
  SELECT CASE ty.typtype WHEN 'd' THEN 'domain' WHEN 'c' THEN 'composite_type'
                         WHEN 'r' THEN 'range_type' ELSE 'base_type' END,
         ty.oid, n.nspname, ty.typname, NULL, NULL,
         CASE WHEN ty.typtype = 'd' THEN format_type(ty.typbasetype, ty.typtypmod) END
  FROM pg_type ty JOIN ns n ON n.oid = ty.typnamespace
  WHERE (ty.typtype IN ('d', 'r')
         OR (ty.typtype = 'c' AND (SELECT rc.relkind FROM pg_class rc WHERE rc.oid = ty.typrelid) = 'c')
         OR (ty.typtype = 'b' AND ty.typcategory <> 'A'))
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_type'::regclass AND e.objid = ty.oid)
  UNION ALL
  -- Extensions (their members are excluded everywhere).
  SELECT 'extension', x.oid, n.nspname, x.extname, NULL, NULL, x.extversion
  FROM pg_extension x JOIN pg_namespace n ON n.oid = x.extnamespace
  UNION ALL
  -- User casts.
  SELECT 'cast', ca.oid, NULL, format_type(ca.castsource, NULL) || ' AS ' || format_type(ca.casttarget, NULL),
         NULL, NULL,
         CASE ca.castmethod WHEN 'f' THEN 'WITH FUNCTION ' || ca.castfunc::regprocedure::text
                            WHEN 'i' THEN 'WITH INOUT' ELSE 'WITHOUT FUNCTION' END
  FROM pg_cast ca
  WHERE ca.oid >= 16384
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_cast'::regclass AND e.objid = ca.oid)
  UNION ALL
  -- Operators.
  SELECT 'operator', o.oid, n.nspname, o.oprname, NULL,
         CASE WHEN o.oprleft = 0 THEN 'NONE' ELSE format_type(o.oprleft, NULL) END || ', ' ||
         CASE WHEN o.oprright = 0 THEN 'NONE' ELSE format_type(o.oprright, NULL) END,
         NULL
  FROM pg_operator o JOIN ns n ON n.oid = o.oprnamespace
  WHERE NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_operator'::regclass AND e.objid = o.oid)
  UNION ALL
  -- Publications.
  SELECT 'publication', pb.oid, NULL, pb.pubname, NULL, NULL, NULL
  FROM pg_publication pb
  UNION ALL
  -- User event triggers (not Chelabase's own, whose functions live in its schemas).
  SELECT 'event_trigger', et.oid, NULL, et.evtname, NULL, NULL, et.evtevent
  FROM pg_event_trigger et
  JOIN pg_proc ep ON ep.oid = et.evtfoid
  JOIN pg_namespace en ON en.oid = ep.pronamespace
  WHERE en.nspname NOT IN ('chelabase', 'chelabase_merge')
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_event_trigger'::regclass AND e.objid = et.oid)
  UNION ALL
  -- Leftovers of an interrupted merge: temporary NOT NULL checks and replacement indexes.
  SELECT 'merge_leftover', k.oid, n.nspname, k.conname, c.relname, NULL, pg_get_constraintdef(k.oid, true)
  FROM pg_constraint k
  JOIN pg_class c ON c.oid = k.conrelid
  JOIN ns n ON n.oid = c.relnamespace
  WHERE k.conname LIKE 'chelabase\_merge\_nn\_%'
  UNION ALL
  SELECT 'merge_leftover', ic.oid, n.nspname, ic.relname, c.relname, NULL, pg_get_indexdef(ic.oid, 0, true)
  FROM pg_index i
  JOIN pg_class ic ON ic.oid = i.indexrelid
  JOIN pg_class c ON c.oid = i.indrelid
  JOIN ns n ON n.oid = c.relnamespace
  WHERE ic.relname LIKE 'chelabase\_merge\_ix\_%'
),
reported_j AS (
  SELECT kind || '.' || coalesce(schema, '') || '.' || name || '.' || oid::int8::text AS k,
         jsonb_build_object(
           'kind', kind, 'oid', oid::int8, 'schema', schema, 'name', name, 'parent', parent,
           'arguments', arguments, 'definition', definition) AS j
  FROM reported_rows
)
SELECT jsonb_build_object(
  'server_version_num', current_setting('server_version_num')::int,
  'schemas', (SELECT coalesce(jsonb_agg(j ORDER BY k), '[]'::jsonb) FROM schemas_j),
  'tables', (SELECT coalesce(jsonb_agg(j ORDER BY k), '[]'::jsonb) FROM tables_j),
  'enums', (SELECT coalesce(jsonb_agg(j ORDER BY k), '[]'::jsonb) FROM enums_j),
  'sequences', (SELECT coalesce(jsonb_agg(j ORDER BY k), '[]'::jsonb) FROM sequences_j),
  'roles', (SELECT coalesce(jsonb_agg(j ORDER BY k), '[]'::jsonb) FROM roles_j),
  'default_privileges', (SELECT coalesce(jsonb_agg(j ORDER BY k), '[]'::jsonb) FROM default_privileges_j),
  'reported', (SELECT coalesce(jsonb_agg(j ORDER BY k), '[]'::jsonb) FROM reported_j)
)::text;
COMMIT;
