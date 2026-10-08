-- Step 6g, T14 (I1, M7): the restore chain of a branch's timelines, and when each base entry
-- was recorded.
--
-- A restore makes a new timeline branched from the old one at the restore point; its history
-- before that point is the old timeline's. `branch_restores` records that link, one row per
-- restored timeline, so a later restore to a point before an earlier restore's can walk back
-- to the timeline (at any depth) that holds the base current there. It's recorded in the
-- restore's switch whether or not the old timeline had a base to copy.
CREATE TABLE branch_restores (
    branch_id        uuid NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    -- The restored timeline.
    timeline_id      text NOT NULL,
    -- The timeline it was branched from, and the point (an LSN, `X/Y`) on it.
    from_timeline_id text NOT NULL,
    at_lsn           text NOT NULL
        CONSTRAINT branch_restores_at_lsn_valid CHECK (at_lsn::pg_lsn IS NOT NULL),
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, timeline_id),
    CHECK (timeline_id <> from_timeline_id)
);

-- When the entry was written (a restore's copy keeps the original's `taken_at`, but is
-- recorded at the restore). An entry superseded on its timeline by one recorded more than the
-- database's history window ago keeps its location and map but drops its model: a restore
-- that still picks it reads it live from its location (or offers re-baseline).
ALTER TABLE branch_schema_bases
    ADD COLUMN recorded_at timestamptz NOT NULL DEFAULT now();
