-- Step 6c: the data API serves any database of a branch, not only main. The per-database flag and
-- schema live on branch_databases; a key can be limited to a list of databases.
ALTER TABLE branch_databases
    ADD COLUMN data_api text NOT NULL DEFAULT 'off' CHECK (data_api IN ('off','enabling','on','error')),
    ADD COLUMN data_api_error text,
    ADD COLUMN default_schema text NOT NULL DEFAULT 'public' CHECK (default_schema ~ '^[a-z_][a-z0-9_]{0,62}$'),
    ADD CONSTRAINT branch_databases_system_off CHECK (origin <> 'system' OR data_api = 'off');
UPDATE branch_databases bd SET data_api = 'on', default_schema = d.default_schema
  FROM branches b JOIN databases d ON d.id = b.database_id
 WHERE bd.branch_id = b.id AND bd.origin = 'main';
ALTER TABLE databases DROP COLUMN default_schema;
ALTER TABLE data_api_keys ADD COLUMN databases text[]
    CHECK (databases IS NULL OR (cardinality(databases) BETWEEN 1 AND 20));
