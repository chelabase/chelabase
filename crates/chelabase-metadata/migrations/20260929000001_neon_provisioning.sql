-- Roadmap step 4: every database runs on Neon. Rows from the postgres:17 era can't be served
-- any more; they become tombstones before their columns go.
ALTER TABLE databases ADD COLUMN endpoint_id text REFERENCES neon_endpoints (id) ON DELETE SET NULL;
UPDATE databases
   SET status = 'deleted', deleted_at = now(), updated_at = now(), owner_password_enc = NULL
 WHERE endpoint_id IS NULL AND status <> 'deleted';
ALTER TABLE databases
    DROP COLUMN container_id,
    DROP COLUMN host,
    DROP COLUMN port,
    DROP COLUMN platform_password_enc;
CREATE UNIQUE INDEX databases_endpoint_id_key ON databases (endpoint_id) WHERE endpoint_id IS NOT NULL;
ALTER TABLE neon_endpoints ADD COLUMN needs_reconfigure boolean NOT NULL DEFAULT false;
