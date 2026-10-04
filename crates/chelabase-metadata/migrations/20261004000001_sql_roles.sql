-- Step 6a revision 2: login roles and Postgres databases managed from SQL.
-- On top of 20261003000001_login_roles.sql (already applied on dev machines; never edit it).

-- neon_roles: the proxy's copy of each role's SCRAM hash, plus bookkeeping.
DROP INDEX neon_roles_one_owner;
ALTER TABLE neon_roles
    DROP CONSTRAINT neon_roles_login_sealed,
    DROP CONSTRAINT neon_roles_owner_privileged,
    DROP CONSTRAINT neon_roles_login_name,
    ADD COLUMN origin       text,
    ADD COLUMN login        boolean NOT NULL DEFAULT true,
    ADD COLUMN level        text NOT NULL DEFAULT 'plain',
    ADD COLUMN forwarded_at timestamptz,
    ALTER COLUMN scram_verifier DROP NOT NULL;
UPDATE neon_roles SET
    origin = CASE kind WHEN 'owner' THEN 'owner' ELSE 'api' END,
    level  = CASE WHEN kind = 'owner' OR privileged THEN 'owner_level' ELSE 'plain' END;
ALTER TABLE neon_roles RENAME COLUMN privileged TO api_owner_level;
UPDATE neon_roles SET api_owner_level = false WHERE origin <> 'api';
ALTER TABLE neon_roles
    ALTER COLUMN origin SET NOT NULL,
    ALTER COLUMN api_owner_level SET DEFAULT false,
    DROP COLUMN kind,
    ADD CONSTRAINT neon_roles_origin CHECK (origin IN ('owner', 'sql', 'api')),
    ADD CONSTRAINT neon_roles_level CHECK (level IN ('plain', 'owner_level')),
    ADD CONSTRAINT neon_roles_owner_verifier CHECK (origin <> 'owner' OR scram_verifier IS NOT NULL),
    ADD CONSTRAINT neon_roles_owner_unsealed CHECK (origin <> 'owner' OR password_enc IS NULL),
    ADD CONSTRAINT neon_roles_api_owner_level CHECK (origin = 'api' OR NOT api_owner_level),
    ADD CONSTRAINT neon_roles_name_length CHECK (octet_length(name) BETWEEN 1 AND 63);
CREATE UNIQUE INDEX neon_roles_one_owner ON neon_roles (endpoint_id) WHERE origin = 'owner';

-- Rollout: computes below the current settings version are re-created once (neon.console_url).
ALTER TABLE neon_endpoints ADD COLUMN compute_settings_version integer NOT NULL DEFAULT 0;

ALTER TABLE branches ADD COLUMN protected boolean NOT NULL DEFAULT false;

-- The Postgres databases of each branch: its main one, those made in SQL, and the system `postgres`
-- database (filled by the first resync).
CREATE TABLE branch_databases (
    branch_id  uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    name       text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 63),
    owner      text NOT NULL,
    origin     text NOT NULL CHECK (origin IN ('main', 'sql', 'system')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, name)
);
CREATE UNIQUE INDEX branch_databases_one_main ON branch_databases (branch_id) WHERE origin = 'main';
INSERT INTO branch_databases (branch_id, name, owner, origin)
SELECT b.id, d.name, d.owner, 'main'
FROM branches b
JOIN neon_databases d ON d.endpoint_id = b.endpoint_id
WHERE b.endpoint_id IS NOT NULL AND b.status <> 'deleted'
ON CONFLICT DO NOTHING;
