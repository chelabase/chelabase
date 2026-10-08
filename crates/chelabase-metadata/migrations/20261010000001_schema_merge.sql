-- Step 6g: three-way schema merge. Rehearsal branches, base snapshots, merges and their events.
-- (Endpoint routing columns live in 20261010000003_endpoint_routing.sql.)

-- The inline CHECK of 20260930000001_branches.sql is auto-named `branches_kind_check`.
ALTER TABLE branches DROP CONSTRAINT branches_kind_check;
ALTER TABLE branches ADD CONSTRAINT branches_kind_check CHECK (kind IN ('normal', 'backup', 'rehearsal'));

CREATE TABLE branch_schema_bases (
    branch_id      uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    database_name  text NOT NULL,
    model          jsonb NULL,              -- NULL: not read yet; the location still says where to read it
    model_version  integer NULL,
    server_major   integer NULL,
    source         text NOT NULL CHECK (source IN ('create', 'reset', 'merge', 'update', 'rebaseline')),
    timeline_id    text NULL,               -- live location; NULL for 'rebaseline'
    lsn            text NULL,
    identity_map   jsonb NOT NULL DEFAULT '{"pairs":[]}',
    taken_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, database_name)
);

CREATE TABLE merges (
    id                 uuid PRIMARY KEY,
    database_id        uuid NOT NULL REFERENCES databases (id) ON DELETE CASCADE,
    source_branch_id   uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    target_branch_id   uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    direction          text NOT NULL CHECK (direction IN ('into_parent', 'from_parent')),
    database_name      text NOT NULL,
    status             text NOT NULL CHECK (status IN ('computing', 'draft', 'rehearsing', 'rehearsed', 'applying',
                        'online', 'finishing', 'done', 'applied_with_drift', 'failed', 'stale', 'cancelled')),
    failed_phase       text NULL,
    version            bigint NOT NULL DEFAULT 1,
    base_fingerprint   text NULL,
    source_fingerprint text NULL,
    target_fingerprint text NULL,
    analysis           jsonb NULL,          -- chelabase_schema::Analysis
    resolutions        jsonb NOT NULL DEFAULT '[]',
    user_steps         jsonb NOT NULL DEFAULT '[]',
    acknowledgements   jsonb NOT NULL DEFAULT '[]',
    options            jsonb NOT NULL DEFAULT '{"lock_timeout_ms":5000,"after":"keep"}',
    plan               jsonb NULL,
    rehearsal          jsonb NULL,
    result             jsonb NULL,          -- created roles, drift, after-merge outcome
    rebaselined        boolean NOT NULL DEFAULT false,
    error              text NULL,
    claimed_at         timestamptz NULL,    -- claim heartbeat
    created_by         text NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX merges_one_open ON merges (source_branch_id, target_branch_id, database_name)
    WHERE status NOT IN ('done', 'applied_with_drift', 'failed', 'stale', 'cancelled');
CREATE UNIQUE INDEX merges_one_applying ON merges (target_branch_id)
    WHERE status IN ('applying', 'online', 'finishing');
CREATE INDEX merges_database ON merges (database_id, created_at DESC);

CREATE TABLE merge_events (
    id          bigserial PRIMARY KEY,
    merge_id    uuid NOT NULL REFERENCES merges (id) ON DELETE CASCADE,
    at          timestamptz NOT NULL DEFAULT now(),
    kind        text NOT NULL,              -- status | step | note
    step_id     text NULL,
    phase       text NULL,
    outcome     text NULL,                  -- ok | failed | skipped
    duration_ms bigint NULL,
    detail      jsonb NULL
);
CREATE INDEX merge_events_merge ON merge_events (merge_id, id);
