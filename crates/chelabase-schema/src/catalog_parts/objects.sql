-- The table API's object list (follows head.sql and tabs.sql): tables, partitioned tables,
-- views, materialized views and foreign tables in the schemas the model reads, without
-- partitions and extension members. `editable` is "the model reads it as a table" (tabs).
-- `data_api_exposed`: anon or authenticated can reach it through the data API (USAGE on the
-- schema and a privilege the data API serves, as its visibility read checks them).
objects_j AS (
  SELECT n.nspname AS s, c.relname AS r, jsonb_build_object(
    'schema', n.nspname,
    'name', c.relname,
    'kind', CASE c.relkind WHEN 'r' THEN 'table'
                           WHEN 'p' THEN 'partitioned_table'
                           WHEN 'v' THEN 'view'
                           WHEN 'm' THEN 'materialized_view'
                           WHEN 'f' THEN 'foreign_table' END,
    'oid', c.oid::int8,
    'owner', o.rolname,
    'comment', obj_description(c.oid, 'pg_class'),
    'rls_enabled', c.relrowsecurity,
    'rls_forced', c.relforcerowsecurity,
    'data_api_exposed', EXISTS (
      SELECT FROM pg_roles ar
      WHERE ar.rolname IN ('anon', 'authenticated')
        AND has_schema_privilege(ar.oid, n.oid, 'USAGE')
        AND (has_any_column_privilege(ar.oid, c.oid, 'SELECT')
             OR has_any_column_privilege(ar.oid, c.oid, 'INSERT')
             OR has_any_column_privilege(ar.oid, c.oid, 'UPDATE')
             OR has_table_privilege(ar.oid, c.oid, 'DELETE'))),
    'row_estimate', CASE WHEN c.relkind <> 'v' AND c.reltuples >= 0 THEN c.reltuples::int8 END,
    'total_bytes', CASE WHEN c.relkind = 'p' THEN
                     (SELECT coalesce(sum(pg_total_relation_size(pt.relid)), 0)
                      FROM pg_partition_tree(c.oid) pt)
                   ELSE coalesce(pg_total_relation_size(c.oid), 0) END::int8,
    'editable', c.oid IN (SELECT oid FROM tabs)
  ) AS j
  FROM pg_class c
  JOIN ns n ON n.oid = c.relnamespace
  JOIN pg_roles o ON o.oid = c.relowner
  WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT c.relispartition
    AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)
)
SELECT jsonb_build_object(
  'server_version_num', current_setting('server_version_num')::int,
  'objects', (SELECT coalesce(jsonb_agg(j ORDER BY s, r), '[]'::jsonb) FROM objects_j)
)::text;
COMMIT;
