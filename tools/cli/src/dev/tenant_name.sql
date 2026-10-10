WITH u AS (
  UPDATE tenants SET display_name = btrim(left(btrim(:'name'), 80))
   WHERE id = :'tenant' AND display_name IS NULL AND btrim(:'name') <> ''
  RETURNING id, display_name)
INSERT INTO access_events (tenant_id, actor_member_id, action, before, after)
SELECT u.id, NULL::uuid, 'tenant_renamed',
       jsonb_build_object('display_name', NULL), jsonb_build_object('display_name', u.display_name)
  FROM u;
