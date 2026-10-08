-- Step 6g, E18: schema merge bases are a history, not one row per (branch, database).
-- Every write appends an entry; the current base is the newest entry (highest id). Each entry
-- also records the branch's own position from which it is valid (`branch_timeline_id`,
-- `branch_lsn`), so a restore can pick the newest entry at or before its point.

ALTER TABLE branch_schema_bases DROP CONSTRAINT branch_schema_bases_pkey;

-- Existing rows get ids in table order: they become the first entries.
ALTER TABLE branch_schema_bases ADD COLUMN id bigserial;
ALTER TABLE branch_schema_bases ADD PRIMARY KEY (id);

ALTER TABLE branch_schema_bases
    ADD COLUMN branch_timeline_id text NULL,
    -- An LSN as text (`X/Y`), like `lsn`; compared as `pg_lsn`.
    ADD COLUMN branch_lsn text NULL
        CONSTRAINT branch_schema_bases_branch_lsn_valid
        CHECK (branch_lsn IS NULL OR branch_lsn::pg_lsn IS NOT NULL);

-- Existing entries are positioned at the branch's current timeline and branch point.
UPDATE branch_schema_bases s
SET branch_timeline_id = b.timeline_id, branch_lsn = b.branched_at_lsn
FROM branches b
WHERE b.id = s.branch_id;

-- The current entry (newest) and a restore's pick (newest on a timeline) per database.
CREATE INDEX branch_schema_bases_history ON branch_schema_bases (branch_id, database_name, id DESC);
