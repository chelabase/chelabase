-- Neon endpoints (roadmap step 3). The proxy, compute_ctl and the storage controller ask the
-- orchestrator's internal listener about these. Step 4 links them to `databases`.
CREATE TABLE neon_endpoints (
    id                   text PRIMARY KEY,           -- 'ep-' + 12 hex
    tenant_id            text NOT NULL,              -- 32 hex
    timeline_id          text NOT NULL,              -- 32 hex
    compute_token_sha256 text NOT NULL,              -- hex SHA-256 of compute_ctl's token
    created_at           timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON neon_endpoints (tenant_id);
CREATE INDEX ON neon_endpoints (timeline_id);

-- Keyed by endpoint, not branch: each branch gets one endpoint, so step 5 copies rows.
CREATE TABLE neon_roles (
    endpoint_id    text NOT NULL REFERENCES neon_endpoints (id) ON DELETE CASCADE,
    name           text NOT NULL,
    scram_verifier text NOT NULL,
    PRIMARY KEY (endpoint_id, name)
);

CREATE TABLE neon_databases (
    endpoint_id text NOT NULL REFERENCES neon_endpoints (id) ON DELETE CASCADE,
    name        text NOT NULL,
    owner       text NOT NULL,
    PRIMARY KEY (endpoint_id, name),
    FOREIGN KEY (endpoint_id, owner) REFERENCES neon_roles (endpoint_id, name)
);
