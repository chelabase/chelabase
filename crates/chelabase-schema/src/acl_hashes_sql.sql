-- The current `acl_hash` of each target (`privileges::acl_hashes_sql`, S3), with the same
-- expression as the privileges read (the `acl_hash` and `rel_acl_hash` fragments, expanded by
-- `privileges.rs`).
--
-- $1 jsonb array of `PrivilegeTarget` in their serde form. One jsonb array
-- `[{object, acl_hash}]`, `object` as given; a target that names no object is left out (a
-- relation target matches a table, view, materialized view, partitioned or foreign table; a
-- sequence target a sequence; a default ACL target an `IN SCHEMA` `pg_default_acl` row).
SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object('object', t.v, 'acl_hash', h.acl_hash)
                ORDER BY t.i), '[]'::jsonb)
FROM pg_catalog.jsonb_array_elements($1::jsonb) WITH ORDINALITY AS t(v, i)
CROSS JOIN LATERAL (
  SELECT @acl_hash(coalesce(n.nspacl, pg_catalog.acldefault('n', n.nspowner)))@ AS acl_hash
  FROM pg_catalog.pg_namespace n
  WHERE t.v ? 'schema' AND n.nspname = t.v -> 'schema' ->> 'schema'
  UNION ALL
  SELECT @rel_acl_hash(c.oid, coalesce(c.relacl, pg_catalog.acldefault('r', c.relowner)))@
  FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
  WHERE t.v ? 'relation'
    AND n.nspname = t.v -> 'relation' ->> 'schema'
    AND c.relname = t.v -> 'relation' ->> 'name'
    AND c.relkind IN ('r', 'v', 'm', 'p', 'f')
  UNION ALL
  SELECT @acl_hash(coalesce(c.relacl, pg_catalog.acldefault('s', c.relowner)))@
  FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
  WHERE t.v ? 'sequence'
    AND n.nspname = t.v -> 'sequence' ->> 'schema'
    AND c.relname = t.v -> 'sequence' ->> 'name'
    AND c.relkind = 'S'
  UNION ALL
  SELECT @acl_hash(coalesce(p.proacl, pg_catalog.acldefault('f', p.proowner)))@
  FROM pg_catalog.pg_proc p
  WHERE t.v ? 'function'
    AND p.oid = (t.v -> 'function' ->> 'oid')::pg_catalog.oid
    AND p.prokind IN ('f', 'p')
  UNION ALL
  SELECT @acl_hash(d.defaclacl)@
  FROM pg_catalog.pg_default_acl d
  JOIN pg_catalog.pg_namespace n ON n.oid = d.defaclnamespace
  JOIN pg_catalog.pg_roles r ON r.oid = d.defaclrole
  WHERE t.v ? 'default_acl'
    AND r.rolname = t.v -> 'default_acl' ->> 'for_role'
    AND n.nspname = t.v -> 'default_acl' ->> 'schema'
    AND d.defaclobjtype = CASE t.v -> 'default_acl' ->> 'object'
                            WHEN 'tables' THEN 'r' WHEN 'sequences' THEN 'S' WHEN 'functions' THEN 'f'
                          END::"char"
  LIMIT 1
) h
