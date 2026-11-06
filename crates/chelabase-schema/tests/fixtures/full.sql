-- The catalog reader's live fixture (catalog_live.rs): every object type 6g merges, the types it
-- only reports, quoted names, and the objects the model must exclude (P6). `@R@` is replaced by
-- the run's role prefix (roles are cluster-wide); the reserved roles are created beforehand by
-- the test when missing.

-- Roles: a NOLOGIN group and a login member of it (and owner-level).
CREATE ROLE "@R@reader" NOLOGIN;
COMMENT ON ROLE "@R@reader" IS 'readers';
CREATE ROLE "@R@app" LOGIN NOINHERIT CONNECTION LIMIT 5 VALID UNTIL '2030-01-01 00:00:00+00' BYPASSRLS CREATEDB;
GRANT "@R@reader" TO "@R@app" WITH ADMIN TRUE, INHERIT FALSE;
GRANT neon_superuser TO "@R@app";

-- A schema with a quoted name.
CREATE SCHEMA "Billing";
COMMENT ON SCHEMA "Billing" IS 'money';
GRANT USAGE ON SCHEMA "Billing" TO "@R@reader";

-- An enum whose sort order isn't integral.
CREATE TYPE "Billing".status AS ENUM ('new', 'paid', 'gone');
ALTER TYPE "Billing".status ADD VALUE 'refunded' BEFORE 'gone';

CREATE TABLE public.customer (
  id integer PRIMARY KEY,
  email text,
  region text,
  "Mixed Case" text,
  CONSTRAINT customer_email_key UNIQUE (email)
);
-- A partial unique index with INCLUDE (a plain and a quoted column).
CREATE UNIQUE INDEX customer_region_idx ON public.customer (lower(region) DESC)
  INCLUDE (email, "Mixed Case") WHERE region IS NOT NULL;

CREATE TABLE "Billing"."Order" (
  id bigint GENERATED ALWAYS AS IDENTITY (START WITH 100 INCREMENT BY 5 CACHE 3) PRIMARY KEY,
  "my col" text COLLATE "C" NOT NULL DEFAULT 'x',
  status "Billing".status NOT NULL DEFAULT 'new',
  total numeric(12,2),
  doubled numeric GENERATED ALWAYS AS (total * 2) STORED,
  "a""b" integer,
  customer_id integer
) WITH (fillfactor = 70);
COMMENT ON TABLE "Billing"."Order" IS 'orders';
COMMENT ON COLUMN "Billing"."Order"."my col" IS 'free text';
ALTER TABLE "Billing"."Order" ADD CONSTRAINT order_customer_fk FOREIGN KEY (customer_id)
  REFERENCES public.customer (id) NOT VALID;
ALTER TABLE "Billing"."Order" ADD CONSTRAINT "positive total" CHECK (total > 0);
ALTER TABLE "Billing"."Order" ENABLE ROW LEVEL SECURITY;
CREATE POLICY "own rows" ON "Billing"."Order" AS RESTRICTIVE FOR UPDATE TO "@R@reader"
  USING (total > 0) WITH CHECK (total < 1000);
-- PUBLIC (polroles = {0}).
CREATE POLICY "public read" ON "Billing"."Order" FOR SELECT USING (true);
GRANT SELECT ON "Billing"."Order" TO "@R@reader";
GRANT UPDATE ("my col") ON "Billing"."Order" TO "@R@app";
GRANT USAGE ON SEQUENCE "Billing"."Order_id_seq" TO "@R@app";
CREATE INDEX "Order status" ON "Billing"."Order" (status, "a""b");

CREATE TABLE "Billing"."my table" ("select" integer);
CREATE UNLOGGED TABLE public.scratch (n integer);
-- An exclusion constraint (its index isn't listed as an index).
CREATE TABLE public.booking (during tsrange, EXCLUDE USING gist (during WITH &&));

-- Sequences: one OWNED BY a column, one standalone.
CREATE SEQUENCE public.tickets AS integer START 7 CYCLE OWNED BY public.customer.id;
CREATE SEQUENCE "Billing".counter;
GRANT SELECT ON SEQUENCE "Billing".counter TO "@R@reader";

-- Default privileges: per schema, and global.
ALTER DEFAULT PRIVILEGES IN SCHEMA "Billing" GRANT SELECT ON TABLES TO "@R@reader";
ALTER DEFAULT PRIVILEGES GRANT USAGE ON SEQUENCES TO "@R@app";

-- Reported only.
CREATE TABLE public.events (at date, n integer) PARTITION BY RANGE (at);
CREATE TABLE public.events_2026 PARTITION OF public.events
  FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE VIEW public.big_orders AS SELECT id FROM "Billing"."Order" WHERE total > 100;
CREATE MATERIALIZED VIEW public.order_count AS SELECT count(*) AS n FROM "Billing"."Order";
CREATE FUNCTION public.touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE PROCEDURE public.noop(a integer) LANGUAGE sql AS $$ SELECT 1 $$;
CREATE TRIGGER touch_order BEFORE UPDATE ON "Billing"."Order"
  FOR EACH ROW EXECUTE FUNCTION public.touch();
CREATE DOMAIN public.positive AS integer CHECK (VALUE > 0);
CREATE TYPE public.pair AS (a integer, b integer);
CREATE FUNCTION public.pair_of(integer) RETURNS public.pair LANGUAGE sql
  AS $$ SELECT ROW($1, $1)::public.pair $$;
CREATE CAST (integer AS public.pair) WITH FUNCTION public.pair_of(integer);
CREATE OPERATOR public.=== (LEFTARG = integer, RIGHTARG = integer, FUNCTION = int4eq);
CREATE RULE no_delete AS ON DELETE TO public.scratch DO INSTEAD NOTHING;
CREATE PUBLICATION customers FOR TABLE public.customer;
CREATE FOREIGN DATA WRAPPER cs_dummy;
CREATE SERVER cs_remote FOREIGN DATA WRAPPER cs_dummy;
CREATE FOREIGN TABLE public.remote (a integer) SERVER cs_remote;
-- A table that INHERITS from another: reported; its parent is a table.
CREATE TABLE public.parent_t (a integer);
CREATE TABLE public.child_t (b integer) INHERITS (public.parent_t);

-- Excluded (P6): Chelabase's schemas, the app users table's managed parts, an extension's
-- members, the data API's default privileges.
CREATE SCHEMA chelabase;
CREATE TABLE chelabase.internal (x integer);
CREATE SCHEMA chelabase_merge;
CREATE TABLE chelabase_merge.applied (merge_id uuid PRIMARY KEY);
CREATE FUNCTION chelabase.user_id() RETURNS uuid LANGUAGE sql AS $$ SELECT NULL::uuid $$;
CREATE TABLE public.users (
  id uuid PRIMARY KEY,
  email text NOT NULL,
  email_verified boolean NOT NULL DEFAULT false,
  created_at timestamptz NOT NULL DEFAULT now(),
  deleted_at timestamptz,
  banned_at timestamptz,
  nickname text
);
CREATE TABLE chelabase.app_users_table (one boolean PRIMARY KEY DEFAULT true CHECK (one),
  schema_name text NOT NULL, table_name text NOT NULL, table_oid oid NOT NULL);
INSERT INTO chelabase.app_users_table (schema_name, table_name, table_oid)
  VALUES ('public', 'users', 'public.users'::regclass);
ALTER TABLE public.users ENABLE ROW LEVEL SECURITY;
CREATE POLICY chelabase_users_select_own ON public.users FOR SELECT TO authenticated
  USING (id = chelabase.user_id());
CREATE POLICY chelabase_users_update_own ON public.users FOR UPDATE TO authenticated
  USING (id = chelabase.user_id()) WITH CHECK (id = chelabase.user_id());
CREATE POLICY users_extra ON public.users FOR SELECT TO "@R@reader" USING (nickname IS NOT NULL);
CREATE UNIQUE INDEX chelabase_users_email_key ON public.users (lower(email)) WHERE deleted_at IS NULL;
CREATE INDEX users_nickname_idx ON public.users (nickname);
GRANT SELECT, UPDATE, DELETE ON public.users TO authenticated;
GRANT SELECT, INSERT, UPDATE, DELETE ON public.users TO chelabase_auth_sync;
GRANT SELECT ON public.users TO "@R@reader";
CREATE FUNCTION chelabase.users_guard() RETURNS trigger LANGUAGE plpgsql
  AS $$ BEGIN RETURN NULL; END $$;
CREATE TRIGGER chelabase_users_guard AFTER INSERT OR UPDATE OR DELETE ON public.users
  FOR EACH ROW EXECUTE FUNCTION chelabase.users_guard();
CREATE TRIGGER chelabase_users_guard_truncate BEFORE TRUNCATE ON public.users
  FOR EACH STATEMENT EXECUTE FUNCTION chelabase.users_guard();
CREATE TRIGGER users_touch BEFORE UPDATE ON public.users
  FOR EACH ROW EXECUTE FUNCTION public.touch();
CREATE EXTENSION pgcrypto;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT SELECT ON TABLES TO anon;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT SELECT, INSERT ON TABLES TO authenticated, "@R@reader";
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO authenticated;
-- E16: the data API's global row, exactly, is excluded …
ALTER DEFAULT PRIVILEGES FOR ROLE postgres REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
-- … while the same revoke for another role stays, and so does another role's per-schema row
-- naming authenticated (T14 M5).
ALTER DEFAULT PRIVILEGES FOR ROLE "@R@app" REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
ALTER DEFAULT PRIVILEGES FOR ROLE "@R@app" IN SCHEMA "Billing" GRANT SELECT ON TABLES TO authenticated;

-- An event trigger, last: it fires on later DDL in this database.
CREATE FUNCTION public.on_ddl() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
CREATE EVENT TRIGGER cs_on_ddl ON ddl_command_end EXECUTE FUNCTION public.on_ddl();
CREATE FUNCTION chelabase.protect() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
CREATE EVENT TRIGGER cs_protect ON sql_drop EXECUTE FUNCTION chelabase.protect();
