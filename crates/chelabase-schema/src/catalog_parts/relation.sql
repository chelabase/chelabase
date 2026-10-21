  jsonb_build_object(
    'oid', c.oid::int8,
    'schema', n.nspname,
    'name', c.relname,
    'owner', jsonb_build_object('oid', o.oid::int8, 'name', o.rolname),
    'comment', obj_description(c.oid, 'pg_class'),
    'rls_enabled', c.relrowsecurity,
    'rls_forced', c.relforcerowsecurity,
    'persistence', c.relpersistence,
    'options', '[]'::jsonb,
    'acl', '[]'::jsonb,
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
            'acl', '[]'::jsonb)
          FROM pg_depend d
          JOIN pg_class s ON s.oid = d.objid AND s.relkind = 'S'
          JOIN pg_sequence sq ON sq.seqrelid = s.oid
          WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass
            AND d.refobjid = c.oid AND d.refobjsubid = a.attnum AND d.deptype = 'i'),
        'comment', col_description(c.oid, a.attnum),
        'acl', '[]'::jsonb
      ) ORDER BY a.attnum), '[]'::jsonb)
      FROM pg_attribute a
      JOIN pg_type ty ON ty.oid = a.atttypid
      LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
      WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped),
    'constraints', '[]'::jsonb,
    'indexes', '[]'::jsonb,
    'policies', '[]'::jsonb
  )