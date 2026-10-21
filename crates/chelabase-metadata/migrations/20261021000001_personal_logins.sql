-- RT4-4 / P10: a personal login is a `neon_roles` row of origin `personal`.
ALTER TABLE neon_roles
    DROP CONSTRAINT neon_roles_origin,
    ADD CONSTRAINT neon_roles_origin CHECK (origin IN ('owner', 'sql', 'api', 'personal'));
