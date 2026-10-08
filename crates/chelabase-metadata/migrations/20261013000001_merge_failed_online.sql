-- Step 6g T14 (E27): a merge that failed in the online phase is half applied (phase 1 committed),
-- so it stays open for its pair and holds its target like a running merge until it is retried to
-- the end or abandoned. The predicates are `chelabase_metadata::merges::{OPEN_SQL, APPLYING_SQL}`.
DROP INDEX merges_one_open;
CREATE UNIQUE INDEX merges_one_open ON merges (source_branch_id, target_branch_id, database_name)
    WHERE NOT (status IN ('done', 'applied_with_drift', 'stale', 'cancelled')
               OR (status = 'failed' AND failed_phase IS DISTINCT FROM 'online'));
DROP INDEX merges_one_applying;
CREATE UNIQUE INDEX merges_one_applying ON merges (target_branch_id)
    WHERE (status IN ('applying', 'online', 'finishing')
           OR (status = 'failed' AND failed_phase = 'online'));

-- T14 M10: when reconcile last handed a silent merge back (`Resume`); it isn't handed back again
-- while that job may still be queued (see `stale_merge_claims`).
ALTER TABLE merges ADD COLUMN handed_back_at timestamptz NULL;
