-- Step 5: when a reset or restore recorded its pending timeline. A restore whose detach stays
-- unresolved for too long after this fails the branch (keeping the timeline recorded). Rows
-- already carrying a pending timeline start their clock now.
ALTER TABLE branches ADD COLUMN pending_timeline_at timestamptz;
UPDATE branches SET pending_timeline_at = now() WHERE pending_timeline_id IS NOT NULL;
