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
