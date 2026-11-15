-- Edge functions m2: a database's functions and services, their immutable deployments, and per
-- branch the active deployment, on/off, secrets, schedules and triggers.
--
-- Bundle bytes are not stored here: a deployment records the bundle's SHA-256 and size; the
-- bytes live in object storage at `chelabase-functions/{database_id}/{sha256}`.
--
-- Branch and database rows are tombstoned, never deleted, so the cascades below fire only when
-- a function is deleted (or a test removes rows by hand). The branch and database tombstones
-- purge the per-branch rows themselves, in their own transaction.
CREATE TABLE functions (
    id          uuid PRIMARY KEY,
    database_id uuid NOT NULL REFERENCES databases (id) ON DELETE CASCADE,
    name        text NOT NULL CHECK (name ~ '^[a-z][a-z0-9-]{0,62}$'),
    kind        text NOT NULL CHECK (kind IN ('function', 'service')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (database_id, name)
);

-- Immutable (the trigger below refuses updates). A function's deployment has a bundle; a
-- service's has none, and `spec` holds its image or URL, port and health path.
CREATE TABLE function_deployments (
    id                 uuid PRIMARY KEY,
    function_id        uuid NOT NULL REFERENCES functions (id) ON DELETE CASCADE,
    bundle_sha256      text CHECK (bundle_sha256 ~ '^[0-9a-f]{64}$'),
    bundle_size        bigint CHECK (bundle_size BETWEEN 0 AND 10485760),
    spec               jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(spec) = 'object'),
    compatibility_date date NOT NULL,
    created_by         text NOT NULL CHECK (length(created_by) BETWEEN 1 AND 200),
    created_at         timestamptz NOT NULL DEFAULT now(),
    note               text CHECK (length(note) <= 1000),
    CHECK ((bundle_sha256 IS NULL) = (bundle_size IS NULL)),
    -- The target of branch_functions' (deployment, function) key.
    UNIQUE (id, function_id)
);
CREATE INDEX function_deployments_function ON function_deployments (function_id);
CREATE INDEX function_deployments_bundle ON function_deployments (bundle_sha256)
    WHERE bundle_sha256 IS NOT NULL;

CREATE FUNCTION function_deployments_immutable() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'function deployments are immutable' USING ERRCODE = 'restrict_violation';
END
$$;
CREATE TRIGGER function_deployments_immutable BEFORE UPDATE ON function_deployments
    FOR EACH ROW EXECUTE FUNCTION function_deployments_immutable();

-- A key a function runs as (minted by the worker for that function on that branch) is revoked
-- only through the function, and never copied to a child branch.
ALTER TABLE data_api_keys
    ADD COLUMN managed_by_function_id uuid REFERENCES functions (id) ON DELETE SET NULL;
CREATE INDEX data_api_keys_managed_by ON data_api_keys (managed_by_function_id)
    WHERE managed_by_function_id IS NOT NULL;
-- The target of branch_functions' (key, branch) key: a function runs only as its branch's key.
CREATE UNIQUE INDEX data_api_keys_id_branch ON data_api_keys (id, branch_id);

-- A function's state on a branch, once a deployment is active there. A copy to a new branch
-- keeps `run_as_role` and leaves `run_as_key_id` null: the worker mints the child's own key.
CREATE TABLE branch_functions (
    branch_id     uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    function_id   uuid NOT NULL REFERENCES functions (id) ON DELETE CASCADE,
    deployment_id uuid NOT NULL,
    enabled       boolean NOT NULL DEFAULT false,
    auth          text NOT NULL DEFAULT 'key' CHECK (auth IN ('key', 'app_user', 'public')),
    run_as_role   text CHECK (length(run_as_role) BETWEEN 1 AND 63),
    run_as_key_id uuid,
    last_error    text,
    updated_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, function_id),
    FOREIGN KEY (deployment_id, function_id) REFERENCES function_deployments (id, function_id),
    FOREIGN KEY (run_as_key_id, branch_id) REFERENCES data_api_keys (id, branch_id)
);
CREATE INDEX branch_functions_function ON branch_functions (function_id);
CREATE INDEX branch_functions_deployment ON branch_functions (deployment_id);

-- Sealed with the secrets key, AAD `fn-secret:{branch_id}:{function_id}:{name}`: a value opens
-- only on its own row. A copy to a new branch is opened and sealed again for the child.
CREATE TABLE branch_function_secrets (
    branch_id   uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    function_id uuid NOT NULL REFERENCES functions (id) ON DELETE CASCADE,
    name        text NOT NULL CHECK (name ~ '^[A-Za-z_][A-Za-z0-9_]{0,127}$'),
    ciphertext  text NOT NULL CHECK (length(ciphertext) <= 131072),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, function_id, name)
);
CREATE INDEX branch_function_secrets_function ON branch_function_secrets (function_id);

CREATE TABLE function_schedules (
    id          uuid PRIMARY KEY,
    branch_id   uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    function_id uuid NOT NULL REFERENCES functions (id) ON DELETE CASCADE,
    cron        text NOT NULL CHECK (length(cron) BETWEEN 1 AND 100),
    timezone    text NOT NULL DEFAULT 'UTC' CHECK (length(timezone) BETWEEN 1 AND 64),
    next_run_at timestamptz,
    enabled     boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX function_schedules_branch ON function_schedules (branch_id);
CREATE INDEX function_schedules_function ON function_schedules (function_id);
CREATE INDEX function_schedules_due ON function_schedules (next_run_at) WHERE enabled;

-- `table` triggers start `pending_install` (the worker installs them in the branch's database;
-- a copy always starts there again). `storage` triggers are `installed` from creation, and a
-- copy's `enabled_since` is its branch's creation, so a child never replays its parent's events.
CREATE TABLE function_triggers (
    id            uuid PRIMARY KEY,
    branch_id     uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    function_id   uuid NOT NULL REFERENCES functions (id) ON DELETE CASCADE,
    source        text NOT NULL CHECK (source IN ('table', 'storage')),
    enabled       boolean NOT NULL DEFAULT true,
    state         text NOT NULL CHECK (state IN ('pending_install', 'installed', 'failed')),
    enabled_since timestamptz NOT NULL DEFAULT now(),
    pg_database   text NOT NULL CHECK (length(pg_database) BETWEEN 1 AND 63),
    schema_name   text CHECK (length(schema_name) BETWEEN 1 AND 63),
    table_name    text CHECK (length(table_name) BETWEEN 1 AND 63),
    bucket        text CHECK (length(bucket) BETWEEN 1 AND 100),
    prefix        text CHECK (length(prefix) BETWEEN 1 AND 1024),
    events        text[] NOT NULL,
    last_error    text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT function_triggers_source_columns CHECK (
        (source = 'table'
            AND schema_name IS NOT NULL AND table_name IS NOT NULL
            AND bucket IS NULL AND prefix IS NULL
            AND cardinality(events) BETWEEN 1 AND 3
            AND events <@ ARRAY['insert', 'update', 'delete']::text[])
        OR (source = 'storage'
            AND schema_name IS NULL AND table_name IS NULL
            AND cardinality(events) BETWEEN 1 AND 4
            AND events <@ ARRAY['object.created', 'object.updated', 'object.deleted',
                                'object.moved']::text[])
    )
);
CREATE INDEX function_triggers_branch ON function_triggers (branch_id);
CREATE INDEX function_triggers_function ON function_triggers (function_id);
