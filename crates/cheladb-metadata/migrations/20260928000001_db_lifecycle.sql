-- Credentials: encrypted at rest (cheladb-secrets), revealed via the API.
ALTER TABLE databases
    ADD COLUMN database_name TEXT,
    ADD COLUMN owner_role TEXT,
    ADD COLUMN owner_password_enc TEXT,
    ADD COLUMN platform_password_enc TEXT,
    ADD COLUMN deleted_at TIMESTAMPTZ,
    DROP COLUMN admin_password;

-- Tombstones: names are unique among databases that aren't deleted.
ALTER TABLE databases DROP CONSTRAINT databases_project_id_name_key;
CREATE UNIQUE INDEX databases_live_name_key ON databases (project_id, name) WHERE status <> 'deleted';
