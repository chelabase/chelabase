ALTER TABLE neon_endpoints ADD COLUMN dbs_forwarded_at timestamptz;
ALTER TABLE neon_roles ADD COLUMN password_unreadable boolean NOT NULL DEFAULT false;
