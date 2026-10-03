-- Step 6b: projects become a resource (name, lifecycle, soft delete) and can have app users:
-- the auth database and its Zitadel org, a per-branch sync-role password, and app sessions.
ALTER TABLE projects
    ADD COLUMN name text,
    ADD COLUMN status text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deleting', 'deleted')),
    ADD COLUMN error text,
    ADD COLUMN updated_at timestamptz NOT NULL DEFAULT now(),
    ADD COLUMN deleted_at timestamptz;
UPDATE projects SET name = slug;
ALTER TABLE projects ALTER COLUMN name SET NOT NULL;
ALTER TABLE projects DROP CONSTRAINT projects_tenant_id_slug_key;
CREATE UNIQUE INDEX projects_live_slug ON projects (tenant_id, slug) WHERE status <> 'deleted';

CREATE TABLE project_auth (
    project_id uuid PRIMARY KEY REFERENCES projects(id),
    status text NOT NULL CHECK (status IN ('enabling', 'enabled', 'disabling', 'failed')),
    error text,
    zitadel_org_id text UNIQUE,
    database_id uuid NOT NULL REFERENCES databases(id),
    pg_database text NOT NULL CHECK (octet_length(pg_database) BETWEEN 1 AND 63),
    schema_name text NOT NULL DEFAULT 'public' CHECK (schema_name ~ '^[a-z_][a-z0-9_]{0,62}$'
        AND schema_name !~ '^pg_'
        AND schema_name NOT IN ('cheladb', 'information_schema', 'neon', 'neon_migration')),
    table_name text NOT NULL DEFAULT 'users' CHECK (table_name ~ '^[a-z_][a-z0-9_]{0,62}$'),
    email_verification boolean NOT NULL DEFAULT false,
    on_delete text NOT NULL DEFAULT 'delete' CHECK (on_delete IN ('delete', 'mark')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX project_auth_database ON project_auth (database_id);

CREATE TABLE branch_auth_sync (
    branch_id uuid PRIMARY KEY REFERENCES branches(id) ON DELETE CASCADE,
    pg_database text NOT NULL,
    password_sealed text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE app_sessions (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects(id),
    user_id uuid NOT NULL,
    zitadel_session_id text NOT NULL,
    refresh_hash bytea NOT NULL,
    prev_refresh_hash bytea,
    grace_refresh_sealed text,
    grace_until timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz
);
CREATE INDEX app_sessions_live_user ON app_sessions (project_id, user_id) WHERE revoked_at IS NULL;
