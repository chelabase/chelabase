-- Step 5: where a branch's history begins (the commit time at its timeline's branch point, or
-- when the timeline was made). Studio bounds its time pickers by it. NULL means unknown: rows
-- made before this column, until their timeline changes.
ALTER TABLE branches ADD COLUMN history_start timestamptz;
