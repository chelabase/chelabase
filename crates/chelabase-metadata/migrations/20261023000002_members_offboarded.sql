-- Members hub step 1, D7 (P19a): the offboarded mark. The Actions webhook (`user.deactivated`,
-- `user.locked`, `user.removed`) and the identity sweep set it when they revoke a member's
-- logins; `user.reactivated`/`user.unlocked`, or the sweep reading the user active, clear it.
-- It is independent of `status`: while it is set, the member is refused every route but
-- `GET /v1/me` and qualifies for no personal login.
ALTER TABLE members ADD COLUMN offboarded_at timestamptz;
-- The sweep's read of the marked members.
CREATE INDEX members_offboarded ON members (tenant_id, id) WHERE offboarded_at IS NOT NULL;

-- MH-17 with D7, MH-9 (EOS-6, T3-M3): no login for an offboarded member, nor on a database
-- that is deleting or deleted, or whose project is. `chelabase_member_level` is unchanged
-- (step 2 replaces its body for team grants).
CREATE OR REPLACE FUNCTION chelabase_personal_login_qualifies(p_member uuid, p_database uuid)
RETURNS boolean
LANGUAGE sql STABLE AS $$
  SELECT coalesce(chelabase_member_level(p_member, p_database) IN ('developer','admin'), false)
     AND EXISTS (SELECT 1 FROM members m WHERE m.id = p_member AND m.offboarded_at IS NULL)
     AND EXISTS (SELECT 1 FROM databases d JOIN projects p ON p.id = d.project_id
                 WHERE d.id = p_database
                   AND d.status NOT IN ('deleting','deleted')
                   AND p.status NOT IN ('deleting','deleted'))
$$;
