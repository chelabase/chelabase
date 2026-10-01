CREATE TABLE data_api_keys (
    id            uuid PRIMARY KEY,
    branch_id     uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    kind          text NOT NULL CHECK (kind IN ('public', 'secret')),
    role          text NOT NULL CHECK (length(role) BETWEEN 1 AND 63),
    name          text NOT NULL CHECK (length(name) BETWEEN 1 AND 63),
    prefix        text NOT NULL,
    sha256        bytea NOT NULL UNIQUE CHECK (length(sha256) = 32),
    sealed        text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_used_at  timestamptz,
    revoked_at    timestamptz,
    CHECK ((kind = 'public') = (sealed IS NOT NULL)),
    CHECK (kind = 'secret' OR role = 'anon')
);
CREATE INDEX data_api_keys_branch ON data_api_keys (branch_id);
CREATE UNIQUE INDEX data_api_keys_one_live_public ON data_api_keys (branch_id)
    WHERE kind = 'public' AND revoked_at IS NULL;

ALTER TABLE branches ADD COLUMN authenticator_password_enc text;

ALTER TABLE databases
    ADD COLUMN max_rows integer NOT NULL DEFAULT 1000 CHECK (max_rows BETWEEN 1 AND 100000),
    ADD COLUMN statement_timeout_ms integer NOT NULL DEFAULT 8000 CHECK (statement_timeout_ms BETWEEN 100 AND 120000),
    ADD COLUMN max_connections integer NOT NULL DEFAULT 5 CHECK (max_connections BETWEEN 1 AND 50),
    ADD COLUMN default_schema text NOT NULL DEFAULT 'public' CHECK (default_schema ~ '^[a-z_][a-z0-9_]{0,62}$');
