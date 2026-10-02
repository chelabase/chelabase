-- A realistic migration that manages its own roles, as a team would write it for any Postgres.
-- scripts/role-e2e.sh runs it with psql as the branch owner, through Cheladb's proxy.
-- psql runs each statement on its own (CREATE DATABASE can't run inside a transaction).

-- A group role that holds the app's privileges; it never logs in.
CREATE ROLE app_rw NOLOGIN;

-- Two login roles: the web app and a background worker.
CREATE ROLE app_web LOGIN PASSWORD 'web-migration-pass-1';
CREATE ROLE app_worker LOGIN PASSWORD 'worker-migration-pass-2';
GRANT app_rw TO app_web, app_worker;

-- The app's schema and its grants.
CREATE TABLE orders (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  item text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
GRANT USAGE ON SCHEMA public TO app_rw;
GRANT SELECT, INSERT, UPDATE, DELETE ON orders TO app_rw;

-- A second database for reporting, owned by the branch owner.
CREATE DATABASE analytics;
