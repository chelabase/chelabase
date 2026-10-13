-- The table API's type catalog (follows head.sql): the types a column can take, in pg_catalog
-- and the schemas the model reads (extension types included: they are columns' types too).
-- Not pseudo-types, not array types (each is its element type's `array_oid`), not tables' or
-- views' row types (standalone composite types stay).
types_j AS (
  SELECT n.nspname AS s, ty.typname AS t, jsonb_build_object(
    'oid', ty.oid::int8,
    'schema', n.nspname,
    'name', ty.typname,
    'kind', CASE ty.typtype WHEN 'b' THEN 'base'
                            WHEN 'e' THEN 'enum'
                            WHEN 'd' THEN 'domain'
                            WHEN 'c' THEN 'composite'
                            WHEN 'r' THEN 'range'
                            WHEN 'm' THEN 'multirange' END,
    'category', ty.typcategory::text,
    'preferred', ty.typispreferred,
    'takes_modifiers', ty.typmodin::oid <> 0,
    'array_oid', nullif(ty.typarray, 0::oid)::int8,
    'labels', (SELECT coalesce(jsonb_agg(e.enumlabel ORDER BY e.enumsortorder), '[]'::jsonb)
               FROM pg_enum e WHERE e.enumtypid = ty.oid),
    'base_type_oid', CASE WHEN ty.typtype = 'd' THEN ty.typbasetype::int8 END
  ) AS j
  FROM pg_type ty
  JOIN pg_namespace n ON n.oid = ty.typnamespace
  WHERE (n.nspname = 'pg_catalog' OR n.oid IN (SELECT oid FROM ns))
    AND ty.typtype IN ('b', 'e', 'd', 'c', 'r', 'm')
    AND NOT EXISTS (SELECT FROM pg_type el WHERE el.typarray = ty.oid)
    AND (ty.typtype <> 'c'
         OR (SELECT rc.relkind FROM pg_class rc WHERE rc.oid = ty.typrelid) = 'c')
)
SELECT jsonb_build_object(
  'server_version_num', current_setting('server_version_num')::int,
  'types', (SELECT coalesce(jsonb_agg(j ORDER BY s, t), '[]'::jsonb) FROM types_j)
)::text;
COMMIT;
