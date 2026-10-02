-- Step 6c review: a row remembers that an enable ever began on it (the bootstrap may have run
-- there), so removing the machinery can be refused on a database the data API never touched.
ALTER TABLE branch_databases ADD COLUMN data_api_bootstrapped boolean NOT NULL DEFAULT false;
UPDATE branch_databases SET data_api_bootstrapped = true WHERE data_api <> 'off' OR origin = 'main';
