  jsonb_build_object(
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
        AND NOT (t.is_users AND a.attname IN ('id', 'email', 'email_verified', 'created_at', 'deleted_at', 'banned_at'))),
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
                AND ma.attname IN ('id', 'email', 'email_verified', 'created_at', 'deleted_at', 'banned_at')))),
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
                  FROM unnest(p.polroles) pr(r) LEFT JOIN pg_roles rr ON rr.oid = pr.r
                  WHERE pr.r NOT IN (SELECT oid FROM hidden)),
        'using', pg_get_expr(p.polqual, p.polrelid),
        'with_check', pg_get_expr(p.polwithcheck, p.polrelid),
        'comment', obj_description(p.oid, 'pg_policy')
      ) ORDER BY p.polname), '[]'::jsonb)
      FROM pg_policy p
      WHERE p.polrelid = t.oid
        -- T4-7: a policy's roles get the ACL grantees' exclusion; one whose every role is
        -- hidden isn't listed (an empty list would read as TO PUBLIC).
        AND EXISTS (SELECT FROM unnest(p.polroles) pr(r) WHERE pr.r NOT IN (SELECT oid FROM hidden))
        AND NOT (t.is_users AND p.polname IN ('chelabase_users_select_own', 'chelabase_users_update_own')))
  )
