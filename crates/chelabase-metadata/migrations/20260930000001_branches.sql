-- Roadmap step 5: branches. Main is a branch whose id equals its database id, so sealed
-- owner passwords (AAD = that id) stay valid. Per-connection state moves off databases.
CREATE TABLE branches (
    id                     uuid PRIMARY KEY,
    database_id            uuid NOT NULL REFERENCES databases (id),
    name                   text NOT NULL,
    parent_branch_id       uuid REFERENCES branches (id),
    kind                   text NOT NULL DEFAULT 'normal' CHECK (kind IN ('normal', 'backup')),
    status                 text NOT NULL,
    timeline_id            text,
    pending_timeline_id    text,
    superseded_timeline_id text,
    endpoint_id            text REFERENCES neon_endpoints (id) ON DELETE SET NULL,
    owner_password_enc     text,
    branched_at_time       timestamptz,
    branched_at_lsn        text,
    error                  text,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    deleted_at             timestamptz
);
CREATE UNIQUE INDEX branches_live_name ON branches (database_id, name) WHERE status <> 'deleted';
CREATE UNIQUE INDEX branches_live_main ON branches (database_id)
    WHERE parent_branch_id IS NULL AND status <> 'deleted';
CREATE UNIQUE INDEX branches_endpoint ON branches (endpoint_id) WHERE endpoint_id IS NOT NULL;
CREATE INDEX branches_parent ON branches (parent_branch_id);
CREATE INDEX branches_status ON branches (status);

ALTER TABLE databases
    ADD COLUMN neon_tenant_id text,
    ADD COLUMN history_window interval NOT NULL DEFAULT interval '7 days',
    ADD COLUMN max_branches int NOT NULL DEFAULT 5;

INSERT INTO branches (id, database_id, name, parent_branch_id, kind, status, timeline_id,
                      endpoint_id, owner_password_enc, error, created_at, updated_at)
SELECT d.id, d.id, 'main', NULL, 'normal', d.status, e.timeline_id,
       d.endpoint_id, d.owner_password_enc, d.error, d.created_at, d.updated_at
  FROM databases d
  LEFT JOIN neon_endpoints e ON e.id = d.endpoint_id
 WHERE d.status <> 'deleted';

UPDATE databases d SET neon_tenant_id = e.tenant_id
  FROM neon_endpoints e WHERE e.id = d.endpoint_id;

ALTER TABLE databases
    DROP COLUMN endpoint_id,
    DROP COLUMN owner_password_enc;
