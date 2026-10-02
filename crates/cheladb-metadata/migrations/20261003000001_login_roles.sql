-- Step 6a: login roles. Existing rows are owners.
ALTER TABLE neon_roles
    ADD COLUMN kind         text NOT NULL DEFAULT 'owner' CHECK (kind IN ('owner', 'login')),
    ADD COLUMN privileged   boolean NOT NULL DEFAULT true,
    ADD COLUMN password_enc text,
    ADD COLUMN pending      boolean NOT NULL DEFAULT false,
    ADD COLUMN created_at   timestamptz NOT NULL DEFAULT now(),
    ADD CONSTRAINT neon_roles_login_sealed CHECK ((kind = 'login') = (password_enc IS NOT NULL)),
    ADD CONSTRAINT neon_roles_owner_privileged CHECK (kind = 'login' OR privileged),
    ADD CONSTRAINT neon_roles_login_name CHECK (kind = 'owner' OR name ~ '^[a-z_][a-z0-9_]{0,62}$');
CREATE UNIQUE INDEX neon_roles_one_owner ON neon_roles (endpoint_id) WHERE kind = 'owner';
ALTER TABLE neon_endpoints ADD COLUMN roles_version bigint NOT NULL DEFAULT 0;
