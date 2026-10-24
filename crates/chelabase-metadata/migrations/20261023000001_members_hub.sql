-- Members hub step 1: suspended members, personal logins per (member, database), the log's branch.

-- A suspended member keeps its grants but reaches nothing (spec Part 6 "Suspend").
ALTER TABLE members DROP CONSTRAINT members_status_check;
ALTER TABLE members ADD CONSTRAINT members_status_check
  CHECK (status IN ('invited','pending','active','suspended','removed'));

-- MH-37: per-branch events (role changes) name their branch. The append-only triggers are
-- unchanged and cover the new column. The two indexes serve the log's member filter (target
-- or actor), newest first.
ALTER TABLE access_events ADD COLUMN branch_id uuid;
CREATE INDEX access_events_target ON access_events (tenant_id, target_member_id, id DESC);
CREATE INDEX access_events_actor ON access_events (tenant_id, actor_member_id, id DESC);

-- One personal login per (member, Chelabase database): the record every branch's role and
-- proxy row follows. `scram_verifier` is plain, like the proxy's copy (P5), and NULL once the
-- login is revoked; a revoked record keeps its name and reason until it is deleted or replaced
-- (P6). The name is unique among the database's logins (D3; `neon_roles` rows of the
-- database's branches are checked by the writer).
CREATE TABLE personal_logins (
  member_id uuid NOT NULL REFERENCES members(id),
  database_id uuid NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
  role_name text NOT NULL CHECK (role_name ~ '^[a-z_][a-z0-9_]{0,62}$'),
  scram_verifier text,
  valid_until timestamptz NOT NULL,
  revoked_at timestamptz,
  revoked_reason text CHECK (revoked_reason IN
    ('access_lost','suspended','revoked_now','identity_offboarded','member_removed')),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (member_id, database_id),
  UNIQUE (database_id, role_name),
  UNIQUE (member_id, database_id, role_name),
  CHECK ((revoked_at IS NULL) = (scram_verifier IS NOT NULL)),
  CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL)));

-- P18: no API ever made a personal row; only test databases hold any. They have no record,
-- so they go before the constraints below (the reconciler drops their marked roles as
-- orphans).
DELETE FROM neon_roles WHERE origin = 'personal';

-- A personal proxy row belongs to its record (MH-15): it names the member and database, only a
-- personal row does, it never holds a sealed password (the platform never knows the
-- password), and it goes with its record.
ALTER TABLE neon_roles
  ADD COLUMN member_id uuid,
  ADD COLUMN personal_database_id uuid,
  ADD CONSTRAINT neon_roles_personal_member
    CHECK ((origin = 'personal') = (member_id IS NOT NULL AND personal_database_id IS NOT NULL)),
  ADD CONSTRAINT neon_roles_personal_unsealed CHECK (origin <> 'personal' OR password_enc IS NULL),
  ADD CONSTRAINT neon_roles_personal_login
    FOREIGN KEY (member_id, personal_database_id, name)
    REFERENCES personal_logins (member_id, database_id, role_name) ON DELETE CASCADE;
-- The cascade's and `member_login_endpoints`' lookup.
CREATE INDEX neon_roles_personal ON neon_roles (member_id, personal_database_id, name)
  WHERE member_id IS NOT NULL;

-- P2: the one level predicate (step 2 replaces the body to add team grants). The member's
-- level on a live database of its tenant: `admin` for an active tenant admin; otherwise its
-- project grant capped by its override's level (an override naming only roles caps nothing).
-- NULL for a member that isn't active, a database deleted or of another tenant, or no grant.
CREATE FUNCTION chelabase_member_level(p_member uuid, p_database uuid) RETURNS text
LANGUAGE sql STABLE AS $$
  SELECT CASE
    WHEN m.tenant_admin THEN 'admin'
    WHEN g.platform_role IS NULL THEN NULL
    ELSE (ARRAY['viewer','developer','admin'])[least(
           array_position(ARRAY['viewer','developer','admin'], g.platform_role),
           coalesce(array_position(ARRAY['viewer','developer','admin'], o.platform_role), 3))]
  END
  FROM members m
  JOIN databases d ON d.id = p_database AND d.status <> 'deleted'
  JOIN projects p ON p.id = d.project_id AND p.tenant_id = m.tenant_id
  LEFT JOIN project_grants g ON g.member_id = m.id AND g.project_id = d.project_id
  LEFT JOIN database_grant_overrides o ON o.member_id = m.id AND o.database_id = d.id
  WHERE m.id = p_member AND m.status = 'active'
$$;

-- MH-17: a personal login needs Developer or above (the Rust mirror is `access::qualifies`).
CREATE FUNCTION chelabase_personal_login_qualifies(p_member uuid, p_database uuid) RETURNS boolean
LANGUAGE sql STABLE AS $$
  SELECT coalesce(chelabase_member_level(p_member, p_database) IN ('developer','admin'), false)
$$;
