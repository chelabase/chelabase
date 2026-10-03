CREATE TABLE saved_queries (
    id UUID PRIMARY KEY,
    database_id UUID NOT NULL REFERENCES databases (id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
    sql TEXT NOT NULL CHECK (octet_length(sql) <= 1048576),
    pg_database TEXT,
    role TEXT,
    version INT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX saved_queries_name ON saved_queries (database_id, user_id, lower(name));
