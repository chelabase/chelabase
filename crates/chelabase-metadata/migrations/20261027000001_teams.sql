-- Members hub step 2: live-linked teams (spec Part 5, D1, MH-1–MH-8).
CREATE TABLE teams (
  id uuid PRIMARY KEY,
  tenant_id uuid NOT NULL REFERENCES tenants(id),
  name text NOT NULL CHECK (name = btrim(name) AND char_length(name) BETWEEN 1 AND 80),
  description text NOT NULL DEFAULT '' CHECK (char_length(description) <= 500),
  created_by uuid REFERENCES members(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE UNIQUE INDEX teams_tenant_name ON teams (tenant_id, lower(name));

CREATE TABLE team_grants (
  team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  project_id uuid NOT NULL REFERENCES projects(id),
  platform_role text NOT NULL CHECK (platform_role IN ('viewer','developer','admin')),
  pg_roles text[] NOT NULL DEFAULT '{}',
  CHECK (NOT ('owner' = ANY(pg_roles))),
  granted_by uuid REFERENCES members(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, project_id));
CREATE INDEX team_grants_project ON team_grants (project_id);

CREATE TABLE team_members (
  team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  member_id uuid NOT NULL REFERENCES members(id),
  added_by uuid REFERENCES members(id),
  added_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, member_id));
-- The member-side lookups (every grant-source read, `touched_projects`).
CREATE INDEX team_members_member ON team_members (member_id, team_id);

-- P5: the one level predicate, now over the own grant and the member's team grants (D1: the
-- platform level is the best source's), capped by the override. LEAST ignores NULL, so "no
-- source" is tested first. `chelabase_personal_login_qualifies` calls it by name at run time,
-- so it isn't recreated.
CREATE OR REPLACE FUNCTION chelabase_member_level(p_member uuid, p_database uuid) RETURNS text
LANGUAGE sql STABLE AS $$
  SELECT CASE
    WHEN m.tenant_admin THEN 'admin'
    WHEN src.best IS NULL THEN NULL
    ELSE (ARRAY['viewer','developer','admin'])[least(src.best,
           coalesce(array_position(ARRAY['viewer','developer','admin'], o.platform_role), 3))]
  END
  FROM members m
  JOIN databases d ON d.id = p_database AND d.status <> 'deleted'
  JOIN projects p ON p.id = d.project_id AND p.tenant_id = m.tenant_id
  LEFT JOIN database_grant_overrides o ON o.member_id = m.id AND o.database_id = d.id
  CROSS JOIN LATERAL (
    SELECT max(array_position(ARRAY['viewer','developer','admin'], s.platform_role)) AS best
    FROM (SELECT g.platform_role FROM project_grants g
           WHERE g.member_id = m.id AND g.project_id = d.project_id
          UNION ALL
          SELECT tg.platform_role FROM team_members tm
            JOIN teams t ON t.id = tm.team_id AND t.tenant_id = m.tenant_id
            JOIN team_grants tg ON tg.team_id = tm.team_id AND tg.project_id = d.project_id
           WHERE tm.member_id = m.id) s) src
  WHERE m.id = p_member AND m.status = 'active'
$$;
