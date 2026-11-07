-- Edge functions m9: external services. A service's signing secret (`whsec_…`, sealed with
-- the secrets key for the function: AAD `{function_id}:service-signing`) is the database's,
-- like the function row; the functions service signs every proxied request with it.
ALTER TABLE functions ADD COLUMN signing_secret text;

-- What the functions service's health checks last saw for an external service's deployment
-- (deployments are immutable, so it lives beside them). No row: not checked yet.
CREATE TABLE service_health (
    deployment_id uuid PRIMARY KEY REFERENCES function_deployments (id) ON DELETE CASCADE,
    status        text NOT NULL CHECK (status IN ('healthy', 'unhealthy')),
    checked_at    timestamptz NOT NULL DEFAULT now()
);
