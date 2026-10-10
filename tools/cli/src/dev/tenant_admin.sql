SELECT id FROM tenants WHERE id = :'tenant' FOR NO KEY UPDATE;
UPDATE members SET email = NULL, updated_at = now()
 WHERE tenant_id = :'tenant' AND zitadel_user_id <> :'zuid' AND status <> 'removed'
   AND email = NULLIF(lower(btrim(:'email')), '');
UPDATE invites SET accepted_at = now()
 WHERE accepted_at IS NULL AND revoked_at IS NULL
   AND member_id IN (SELECT id FROM members
                      WHERE tenant_id = :'tenant' AND zitadel_user_id = :'zuid' AND status = 'invited');
WITH p AS (
  SELECT id, status, tenant_admin FROM members
   WHERE tenant_id = :'tenant' AND zitadel_user_id = :'zuid'),
u AS (
  INSERT INTO members (id, tenant_id, zitadel_user_id, email, tenant_admin, status)
  VALUES (gen_random_uuid(), :'tenant', :'zuid', NULLIF(lower(btrim(:'email')), ''), true, 'active')
  ON CONFLICT (tenant_id, zitadel_user_id) DO UPDATE
    SET status = 'active', tenant_admin = true, email = EXCLUDED.email,
        accepted_at = CASE WHEN members.status = 'invited' THEN now() ELSE members.accepted_at END,
        updated_at = now()
    WHERE members.status <> 'active' OR NOT members.tenant_admin
       OR members.email IS DISTINCT FROM EXCLUDED.email
  RETURNING id)
INSERT INTO access_events (tenant_id, actor_member_id, action, target_member_id, before, after)
SELECT :'tenant'::uuid, NULL::uuid, 'admin_set', u.id,
       CASE WHEN p.id IS NULL THEN NULL
            ELSE jsonb_build_object('status', p.status, 'tenant_admin', p.tenant_admin) END,
       jsonb_build_object('status', 'active', 'tenant_admin', true)
  FROM u LEFT JOIN p ON true
 WHERE p.id IS NULL OR p.status <> 'active' OR NOT p.tenant_admin;
