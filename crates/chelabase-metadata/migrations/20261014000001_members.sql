-- Members and roles: tenant members, per-project grants and narrowing overrides, invites, the
-- reconciler's per-member Postgres roles, and the append-only access log.
--
-- R7: `owner` is never stored in a role set: the Admin platform role implies the database owner.
-- R8: the newest claim of an email wins (Zitadel is the truth); a member whose email another
-- member claimed since keeps NULL (unknown or stale) until its own next claim.
ALTER TABLE tenants ADD COLUMN open_signup boolean NOT NULL DEFAULT false;
CREATE TABLE members (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES tenants(id),
  zitadel_user_id text NOT NULL, email text CHECK (email IS NULL OR email = lower(btrim(email))),
  display_name text NOT NULL DEFAULT '', tenant_admin boolean NOT NULL DEFAULT false,
  status text NOT NULL CHECK (status IN ('invited','active','removed')),
  invited_by uuid REFERENCES members(id), accepted_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, zitadel_user_id));
CREATE UNIQUE INDEX members_live_email ON members (tenant_id, email)
  WHERE status <> 'removed' AND email IS NOT NULL;
CREATE TABLE project_grants (
  member_id uuid NOT NULL REFERENCES members(id), project_id uuid NOT NULL REFERENCES projects(id),
  platform_role text NOT NULL CHECK (platform_role IN ('viewer','developer','admin')),
  pg_roles text[] NOT NULL DEFAULT '{}',
  CHECK (NOT ('owner' = ANY(pg_roles))),
  granted_by uuid REFERENCES members(id), created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (member_id, project_id));
CREATE INDEX project_grants_project ON project_grants (project_id);
CREATE TABLE database_grant_overrides (
  member_id uuid NOT NULL REFERENCES members(id), database_id uuid NOT NULL REFERENCES databases(id),
  platform_role text CHECK (platform_role IN ('viewer','developer','admin')),
  pg_roles text[], set_by uuid REFERENCES members(id),
  created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
  CHECK (platform_role IS NOT NULL OR pg_roles IS NOT NULL),
  CHECK (pg_roles IS NULL OR NOT ('owner' = ANY(pg_roles))),
  PRIMARY KEY (member_id, database_id));
CREATE TABLE invites (
  id uuid PRIMARY KEY, member_id uuid NOT NULL REFERENCES members(id), zitadel_user_id text NOT NULL,
  invited_by uuid NOT NULL REFERENCES members(id), expires_at timestamptz NOT NULL,
  revoked_at timestamptz, accepted_at timestamptz, created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX invites_member ON invites (member_id);
CREATE TABLE member_branch_roles (
  member_id uuid NOT NULL REFERENCES members(id), branch_id uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
  role_name text NOT NULL, password_enc text NOT NULL,
  applied_roles text[] NOT NULL DEFAULT '{}', applied_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (member_id, branch_id), UNIQUE (branch_id, role_name));
CREATE TABLE access_events (
  id bigserial PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES tenants(id),
  actor_member_id uuid REFERENCES members(id), action text NOT NULL,
  target_member_id uuid REFERENCES members(id), project_id uuid, database_id uuid,
  before jsonb, after jsonb, at timestamptz NOT NULL DEFAULT now());
-- The log pages newest first by id within a tenant.
CREATE INDEX access_events_tenant_id ON access_events (tenant_id, id DESC);
CREATE FUNCTION access_events_append_only() RETURNS trigger LANGUAGE plpgsql AS
  $$BEGIN RAISE EXCEPTION 'access_events is append-only'; END$$;
CREATE TRIGGER access_events_no_change BEFORE UPDATE OR DELETE ON access_events
  FOR EACH ROW EXECUTE FUNCTION access_events_append_only();
-- TRUNCATE fires no row trigger.
CREATE TRIGGER access_events_no_truncate BEFORE TRUNCATE ON access_events
  FOR EACH STATEMENT EXECUTE FUNCTION access_events_append_only();
