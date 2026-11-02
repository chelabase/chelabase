//! The `storage` schema in a branch's database: object metadata that branches copy for free
//! and that policies written in plain SQL guard (object-storage m3).
//!
//! [`storage_schema_sql`] runs as the superuser right after the data API script, in every
//! database the data API bootstrap reaches (provision, branch create, compute re-creation and
//! reconcile: [`crate::data_api`]). Reconcile reruns the bootstrap wherever
//! [`storage_check_sql`] (part of [`crate::data_api::schema_check_sql`]) doesn't find the
//! current [`STORAGE_SCHEMA_STAMP`].
//!
//! Who can do what:
//!
//! - `chelabase_admin` (the data API's locked-down NOLOGIN role, no members) owns the schema,
//!   both tables and the `object_ref` type, so the database owner can't `ALTER`, `DISABLE`
//!   or `NO FORCE` row level security, add triggers, or drop the schema.
//! - Row level security is enabled and forced on both tables, with no policies: nothing is
//!   visible or writable until the owner adds policies through `storage.create_policy` /
//!   `storage.drop_policy` (`SECURITY DEFINER`, owned by `chelabase_admin`, executable by the
//!   owner only).
//! - [`STORAGE_ROLE`] (`chelabase_storage`, BYPASSRLS, NOLOGIN until m5 makes it the storage
//!   service's login) is locked down on every run exactly as the data API locks down
//!   `chelabase_admin`: attributes, password, connection limit, settings (then
//!   `search_path = pg_catalog, pg_temp`), members, admins and its own memberships revoked,
//!   objects it owns here reassigned to the owner, its sessions (and its former members')
//!   ended before and again after the commit. A role found changed has its tickets voided.
//! - `anon` and `authenticated` get `USAGE` on the schema (never `CREATE`) and table
//!   privileges that row level security still gates. `authenticated` writes every column of
//!   `storage.objects` a caller declares, the [`MANAGED_COLUMNS`] included, so RLS policies
//!   see them. `authenticator` gets `USAGE` only (it parses statements before it switches
//!   role).
//! - Upload tickets vouch for the managed columns. Only [`STORAGE_ROLE`] writes
//!   `storage.upload_tickets` (`guard_tickets` checks `current_user`, so `pg_write_all_data`
//!   mints nothing: [`TICKET_FORGED`]); a ticket lives at most [`TICKET_MAX_LIFETIME`] and
//!   records the Neon timeline it was minted on. `protect_objects` (after RLS, so a policy's
//!   42501 comes first) lets an insert, or an update changing a managed column, through only
//!   by consuming the ticket matching its bucket, name, version, size and etag, unexpired and
//!   of this timeline; otherwise [`TICKET_MISSING`]. No role is exempt.
//! - A branch copies unconsumed tickets but never its parent's timeline (`neon.timeline_id`
//!   is a postmaster setting, fixed per compute), so they can't be used there.
//! - Owner-level roles (the database owner is a Neon privileged role: `BYPASSRLS`, a member of
//!   `pg_read_all_data` and `pg_write_all_data`) read and write past both RLS and the column
//!   grants, as on any table; the triggers still hold them to tickets.
//!
//! - The policy helpers run with `event_triggers = off` (the bootstrap grants
//!   `chelabase_admin` `SET` on the parameter), so an owner's event trigger never runs as
//!   `chelabase_admin` inside them.
//!
//! The script is two transactions, each under [`BOOTSTRAP_LOCK_KEY`] and safe to run again:
//! the role's lockdown (every role reaching it, at any depth, has its sessions ended, before
//! and again after the commit), then the install. Every object is created only when missing
//! (or replaced); whatever is in `storage` that the install doesn't make
//! ([`foreign_objects_sql`]) is dropped; every ACL entry there is revoked (as its grantor,
//! grant options included) before the install's own grants; the tables' triggers are turned
//! back on. [`storage_check_sql`] notices any of it, so reconcile reruns the install. A
//! `storage` schema that `chelabase_admin` doesn't own (a user's, from before Chelabase
//! reserved the name) is left alone: the install stops at once with [`FOREIGN_SCHEMA_MARK`]
//! and changes nothing; the role's lockdown has already committed.

use crate::data_api::{BOOTSTRAP_LOCK_KEY, LOCK_TIMEOUT_SECS, quote_ident};

/// The schema's name, reserved as a data API default schema and never served by `/data/v1`.
pub const STORAGE_SCHEMA: &str = "storage";

/// The storage service's role.
pub const STORAGE_ROLE: &str = "chelabase_storage";

/// The columns of `storage.objects` a caller may write only with a matching upload ticket.
pub const MANAGED_COLUMNS: [&str; 4] = ["bucket_id", "version_id", "size", "etag"];

/// The schema's comment: which version of [`storage_schema_sql`] installed it. Bump the
/// number whenever the script changes what it installs, so reconcile reinstalls.
pub const STORAGE_SCHEMA_STAMP: &str = "chelabase storage schema 3";

/// The SQLSTATE `protect_objects` raises when an object's managed columns match no unexpired
/// ticket of this timeline (none, a different value, already used, expired, or another
/// branch's). Not `CB001`/`CB002`: those are the data API's (stale schema version, response
/// too large) and rest's pipeline acts on them.
pub const TICKET_MISSING: &str = "CB003";

/// The SQLSTATE `guard_tickets` raises when anyone but [`STORAGE_ROLE`] writes a ticket.
pub const TICKET_FORGED: &str = "CB004";

/// The longest a ticket may live: an upload's final insert follows its body within seconds.
pub const TICKET_MAX_LIFETIME: &str = "15 minutes";

/// The script's first line (the fakes' needle).
pub const STORAGE_SCHEMA_MARK: &str = "-- chelabase:storage-schema";

/// The error the script raises, before changing anything, when `storage` belongs to someone
/// else.
pub const FOREIGN_SCHEMA_MARK: &str = "chelabase_storage_foreign";

/// The install script, for `admin_sql` (the superuser) in one database; `owner_role` is the
/// database owner (the policy helpers' only grantee).
///
/// Plain operators inside the function bodies and table definitions resolve against the
/// pinned `search_path` (`pg_catalog` first) when they're created or run; the `DO` blocks
/// qualify theirs, like the data API script.
pub fn storage_schema_sql(owner_role: &str) -> String {
    let owner = quote_ident(owner_role);
    let foreign_objects = foreign_objects_sql();
    let acl_entries = acl_entries_sql().replace("{owner}", &owner_role.replace('\'', "''"));
    format!(
        r#"{STORAGE_SCHEMA_MARK}
BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});
DO $$ DECLARE
  role pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_storage');
  members pg_catalog.oid[];
  joined boolean;
  tampered boolean;
  m record;
BEGIN
  IF role IS NULL THEN
    CREATE ROLE chelabase_storage NOLOGIN NOINHERIT BYPASSRLS PASSWORD NULL CONNECTION LIMIT 0;
    role := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_storage');
  END IF;
  members := ARRAY(WITH RECURSIVE reach(oid) AS (
      SELECT a.member FROM pg_catalog.pg_auth_members a WHERE a.roleid OPERATOR(pg_catalog.=) role
      UNION SELECT a.member FROM pg_catalog.pg_auth_members a JOIN reach ON a.roleid OPERATOR(pg_catalog.=) reach.oid)
    SELECT oid FROM reach);
  joined := EXISTS (SELECT FROM pg_catalog.pg_auth_members a WHERE a.roleid OPERATOR(pg_catalog.=) role OR a.member OPERATOR(pg_catalog.=) role);
  tampered := EXISTS (SELECT FROM pg_catalog.pg_authid a WHERE a.oid OPERATOR(pg_catalog.=) role
    AND (a.rolcanlogin OR a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolreplication OR NOT a.rolbypassrls OR a.rolinherit
      OR a.rolpassword IS NOT NULL OR a.rolconnlimit OPERATOR(pg_catalog.<>) 0))
    OR EXISTS (SELECT FROM pg_catalog.pg_db_role_setting s WHERE s.setrole OPERATOR(pg_catalog.=) role
      AND (s.setdatabase OPERATOR(pg_catalog.<>) 0::pg_catalog.oid OR s.setconfig OPERATOR(pg_catalog.<>) ARRAY['search_path=pg_catalog, pg_temp']::pg_catalog.text[]));
  ALTER ROLE chelabase_storage NOLOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION BYPASSRLS NOINHERIT PASSWORD NULL CONNECTION LIMIT 0;
  FOR m IN SELECT d.datname FROM pg_catalog.pg_db_role_setting s JOIN pg_catalog.pg_database d ON d.oid OPERATOR(pg_catalog.=) s.setdatabase
    WHERE s.setrole OPERATOR(pg_catalog.=) role
  LOOP
    EXECUTE pg_catalog.format('ALTER ROLE chelabase_storage IN DATABASE %I RESET ALL', m.datname);
  END LOOP;
  ALTER ROLE chelabase_storage RESET ALL;
  ALTER ROLE chelabase_storage SET search_path = pg_catalog, pg_temp;
  FOR m IN SELECT r.rolname AS member, g.rolname AS grantor FROM pg_catalog.pg_auth_members a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.member JOIN pg_catalog.pg_roles g ON g.oid OPERATOR(pg_catalog.=) a.grantor
    WHERE a.roleid OPERATOR(pg_catalog.=) role
  LOOP
    EXECUTE pg_catalog.format('REVOKE chelabase_storage FROM %I GRANTED BY %I CASCADE', m.member, m.grantor);
  END LOOP;
  FOR m IN SELECT r.rolname AS granted, g.rolname AS grantor FROM pg_catalog.pg_auth_members a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.roleid JOIN pg_catalog.pg_roles g ON g.oid OPERATOR(pg_catalog.=) a.grantor
    WHERE a.member OPERATOR(pg_catalog.=) role
  LOOP
    EXECUTE pg_catalog.format('REVOKE %I FROM chelabase_storage GRANTED BY %I CASCADE', m.granted, m.grantor);
  END LOOP;
  REASSIGN OWNED BY chelabase_storage TO {owner};
  PERFORM pg_catalog.set_config('chelabase.storage_former_members', pg_catalog.array_to_string(members, ','), false);
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (a.usesysid OPERATOR(pg_catalog.=) role OR a.usesysid OPERATOR(pg_catalog.=) ANY (members)) AND NOT r.rolsuper
      AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
  IF (joined OR tampered) AND EXISTS (SELECT FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
      JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner
      WHERE n.nspname OPERATOR(pg_catalog.=) 'storage' AND c.relname OPERATOR(pg_catalog.=) 'upload_tickets' AND r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN
    DELETE FROM storage.upload_tickets;
    RAISE WARNING 'chelabase: chelabase_storage had been changed; it was locked down and its upload tickets voided';
  END IF;
END $$;
COMMIT;
DO $$ BEGIN
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (r.rolname OPERATOR(pg_catalog.=) 'chelabase_storage' OR a.usesysid OPERATOR(pg_catalog.=) ANY
        (pg_catalog.string_to_array(pg_catalog.current_setting('chelabase.storage_former_members', true), ',')::pg_catalog.oid[]))
      AND NOT r.rolsuper AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
END $$;
BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});
DO $$ BEGIN
  IF EXISTS (SELECT FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner
      WHERE n.nspname OPERATOR(pg_catalog.=) 'storage' AND r.rolname OPERATOR(pg_catalog.<>) 'chelabase_admin') THEN
    RAISE EXCEPTION '{FOREIGN_SCHEMA_MARK}: the storage schema is not Chelabase''s; it was left alone';
  END IF;
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN
    RAISE EXCEPTION 'chelabase_data_api_missing: the data API bootstrap has not run in this database';
  END IF;
END $$;
GRANT SET ON PARAMETER event_triggers TO chelabase_admin;
CREATE SCHEMA IF NOT EXISTS storage AUTHORIZATION chelabase_admin;
CREATE TABLE IF NOT EXISTS storage.buckets (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  name text NOT NULL UNIQUE,
  public boolean NOT NULL DEFAULT false,
  file_size_limit bigint CHECK (file_size_limit > 0),
  allowed_mime_types text[],
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS storage.objects (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  bucket_id uuid NOT NULL REFERENCES storage.buckets (id),
  name text NOT NULL,
  owner uuid,
  metadata jsonb NOT NULL DEFAULT '{{}}',
  size bigint,
  content_type text,
  etag text,
  version_id uuid,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (bucket_id, name)
);
CREATE TABLE IF NOT EXISTS storage.upload_tickets (
  version_id uuid PRIMARY KEY,
  bucket_id uuid NOT NULL,
  name text NOT NULL,
  size bigint NOT NULL,
  etag text NOT NULL,
  expires_at timestamptz NOT NULL,
  timeline_id text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE storage.upload_tickets OWNER TO chelabase_admin;
ALTER TABLE storage.buckets OWNER TO chelabase_admin;
ALTER TABLE storage.objects OWNER TO chelabase_admin;
ALTER TABLE storage.buckets ENABLE ROW LEVEL SECURITY;
ALTER TABLE storage.buckets FORCE ROW LEVEL SECURITY;
ALTER TABLE storage.objects ENABLE ROW LEVEL SECURITY;
ALTER TABLE storage.objects FORCE ROW LEVEL SECURITY;
DO $$ DECLARE d pg_catalog.text; BEGIN
  FOR d IN {foreign_objects} LOOP
    EXECUTE d;
  END LOOP;
END $$;
DO $$ DECLARE e record; BEGIN
  FOR e IN SELECT g.rolname AS grantor, pg_catalog.format('REVOKE ALL %s FROM %s CASCADE', x.target,
        CASE WHEN x.grantee OPERATOR(pg_catalog.=) 0::pg_catalog.oid THEN 'PUBLIC' ELSE pg_catalog.quote_ident(r.rolname) END) AS statement
      FROM ({acl_entries}) x
      JOIN pg_catalog.pg_roles g ON g.oid OPERATOR(pg_catalog.=) x.grantor
      LEFT JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) x.grantee
  LOOP
    EXECUTE pg_catalog.format('SET LOCAL ROLE %I', e.grantor);
    EXECUTE e.statement;
    RESET ROLE;
  END LOOP;
END $$;
REVOKE ALL ON SCHEMA storage FROM PUBLIC;
GRANT USAGE ON SCHEMA storage TO anon, authenticated, authenticator, chelabase_storage, {owner};
ALTER TABLE storage.buckets ENABLE TRIGGER ALL;
ALTER TABLE storage.objects ENABLE TRIGGER ALL;
ALTER TABLE storage.upload_tickets ENABLE TRIGGER ALL;
ALTER TABLE storage.upload_tickets NO FORCE ROW LEVEL SECURITY;
ALTER TABLE storage.upload_tickets DISABLE ROW LEVEL SECURITY;
REVOKE ALL ON storage.buckets, storage.objects, storage.upload_tickets FROM PUBLIC, anon, authenticated, authenticator, chelabase_storage, {owner};
GRANT SELECT ON storage.buckets, storage.objects TO anon, authenticated;
GRANT INSERT (id, name, public, file_size_limit, allowed_mime_types), UPDATE (name, public, file_size_limit, allowed_mime_types), DELETE ON storage.buckets TO authenticated;
GRANT INSERT (id, bucket_id, name, owner, metadata, size, content_type, etag, version_id), UPDATE (bucket_id, name, owner, metadata, size, content_type, etag, version_id), DELETE ON storage.objects TO authenticated;
GRANT SELECT, INSERT, UPDATE, DELETE ON storage.buckets, storage.objects TO chelabase_storage;
GRANT SELECT, INSERT, DELETE ON storage.upload_tickets TO chelabase_storage;
CREATE OR REPLACE FUNCTION storage.guard_tickets() RETURNS trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
BEGIN
  IF current_user <> 'chelabase_storage' THEN
    RAISE EXCEPTION USING ERRCODE = '{TICKET_FORGED}', MESSAGE = 'storage.upload_tickets is written by Chelabase storage only';
  END IF;
  -- The timeline it was minted on: a branch copies the row, never the timeline.
  NEW.timeline_id := pg_catalog.current_setting('neon.timeline_id', true);
  IF NEW.timeline_id IS NULL OR NEW.timeline_id = '' THEN
    RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'storage.upload_tickets: this server has no Neon timeline';
  END IF;
  IF NEW.expires_at <= clock_timestamp() OR NEW.expires_at > clock_timestamp() + interval '{TICKET_MAX_LIFETIME}' THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.upload_tickets: a ticket expires within {TICKET_MAX_LIFETIME}';
  END IF;
  RETURN NEW;
END
$f$;
ALTER FUNCTION storage.guard_tickets() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION storage.guard_tickets() FROM PUBLIC;
CREATE OR REPLACE TRIGGER guard_tickets BEFORE INSERT OR UPDATE ON storage.upload_tickets
  FOR EACH ROW EXECUTE FUNCTION storage.guard_tickets();
ALTER TABLE storage.upload_tickets ENABLE ALWAYS TRIGGER guard_tickets;
CREATE OR REPLACE FUNCTION storage.protect_objects() RETURNS trigger
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  consumed uuid;
BEGIN
  IF TG_OP = 'UPDATE' AND NEW.bucket_id IS NOT DISTINCT FROM OLD.bucket_id AND NEW.version_id IS NOT DISTINCT FROM OLD.version_id
      AND NEW.size IS NOT DISTINCT FROM OLD.size AND NEW.etag IS NOT DISTINCT FROM OLD.etag THEN
    RETURN NULL;
  END IF;
  DELETE FROM storage.upload_tickets t
    WHERE t.version_id = NEW.version_id AND t.bucket_id = NEW.bucket_id AND t.name = NEW.name
      AND t.size = NEW.size AND t.etag = NEW.etag AND t.expires_at > clock_timestamp()
      AND t.timeline_id = pg_catalog.current_setting('neon.timeline_id', true)
    RETURNING t.version_id INTO consumed;
  IF consumed IS NULL THEN
    RAISE EXCEPTION USING ERRCODE = '{TICKET_MISSING}', MESSAGE = 'storage.objects: bucket_id, name, version_id, size and etag match no unused upload ticket';
  END IF;
  RETURN NULL;
END
$f$;
ALTER FUNCTION storage.protect_objects() OWNER TO chelabase_admin;
REVOKE ALL ON FUNCTION storage.protect_objects() FROM PUBLIC;
CREATE OR REPLACE TRIGGER protect_objects AFTER INSERT OR UPDATE ON storage.objects
  FOR EACH ROW EXECUTE FUNCTION storage.protect_objects();
ALTER TABLE storage.objects ENABLE ALWAYS TRIGGER protect_objects;
DO $$ BEGIN
  IF pg_catalog.to_regtype('storage.object_ref') IS NULL THEN
    CREATE TYPE storage.object_ref AS (bucket_id uuid, name text);
  END IF;
END $$;
ALTER TYPE storage.object_ref OWNER TO chelabase_admin;
GRANT USAGE ON TYPE storage.object_ref TO PUBLIC;
CREATE OR REPLACE FUNCTION storage.foldername(name text) RETURNS text[]
  LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE SET search_path = pg_catalog, pg_temp AS
$f$ SELECT (string_to_array(name, '/'))[1:cardinality(string_to_array(name, '/')) - 1] $f$;
CREATE OR REPLACE FUNCTION storage.filename(name text) RETURNS text
  LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE SET search_path = pg_catalog, pg_temp AS
$f$ SELECT split_part(name, '/', -1) $f$;
CREATE OR REPLACE FUNCTION storage.extension(name text) RETURNS text
  LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE SET search_path = pg_catalog, pg_temp AS
$f$ SELECT CASE WHEN strpos(split_part(name, '/', -1), '.') > 0 THEN split_part(split_part(name, '/', -1), '.', -1) ELSE '' END $f$;
CREATE OR REPLACE FUNCTION storage.object_ref(bucket_id uuid, name text) RETURNS storage.object_ref
  LANGUAGE sql IMMUTABLE PARALLEL SAFE SET search_path = pg_catalog, pg_temp AS
$f$ SELECT ROW(bucket_id, name)::storage.object_ref $f$;
ALTER FUNCTION storage.foldername(text) OWNER TO CURRENT_USER;
ALTER FUNCTION storage.filename(text) OWNER TO CURRENT_USER;
ALTER FUNCTION storage.extension(text) OWNER TO CURRENT_USER;
ALTER FUNCTION storage.object_ref(uuid, text) OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION storage.foldername(text), storage.filename(text), storage.extension(text), storage.object_ref(uuid, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION storage.foldername(text), storage.filename(text), storage.extension(text), storage.object_ref(uuid, text) TO PUBLIC;
CREATE OR REPLACE FUNCTION storage.create_policy(policy_name name, table_name name, command text DEFAULT 'ALL',
    roles name[] DEFAULT ARRAY['public']::name[], using_expr text DEFAULT NULL, check_expr text DEFAULT NULL,
    permissive boolean DEFAULT true) RETURNS void
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp SET event_triggers = off AS
$f$
DECLARE
  targets text;
  statement text;
BEGIN
  IF table_name IS NULL OR table_name NOT IN ('buckets', 'objects') THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: the table must be buckets or objects';
  END IF;
  IF policy_name IS NULL OR permissive IS NULL OR command IS NULL THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: policy_name, command and permissive are required';
  END IF;
  IF pg_catalog.upper(command) NOT IN ('ALL', 'SELECT', 'INSERT', 'UPDATE', 'DELETE') THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: command must be ALL, SELECT, INSERT, UPDATE or DELETE';
  END IF;
  IF using_expr IS NULL AND check_expr IS NULL THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: give using_expr, check_expr or both';
  END IF;
  -- The expressions are pasted into the statement: without a ";" they can't end it and start
  -- another (each sits on lines of its own, so a "--" comment ends with its line).
  IF pg_catalog.strpos(using_expr, ';') > 0 OR pg_catalog.strpos(check_expr, ';') > 0 THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: a policy expression can''t contain ";"';
  END IF;
  IF roles IS NULL OR pg_catalog.cardinality(roles) = 0 OR pg_catalog.array_position(roles, NULL) IS NOT NULL THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: name at least one role (or public)';
  END IF;
  SELECT pg_catalog.string_agg(CASE WHEN r = 'public' THEN 'PUBLIC' ELSE pg_catalog.quote_ident(r) END, ', ')
    INTO targets FROM pg_catalog.unnest(roles) AS r;
  statement := pg_catalog.format('CREATE POLICY %I ON storage.%I AS %s FOR %s TO %s', policy_name, table_name,
    CASE WHEN permissive THEN 'PERMISSIVE' ELSE 'RESTRICTIVE' END, pg_catalog.upper(command), targets);
  IF using_expr IS NOT NULL THEN
    statement := statement || E'\n  USING ((\n' || using_expr || E'\n))';
  END IF;
  IF check_expr IS NOT NULL THEN
    statement := statement || E'\n  WITH CHECK ((\n' || check_expr || E'\n))';
  END IF;
  EXECUTE statement;
END
$f$;
CREATE OR REPLACE FUNCTION storage.drop_policy(policy_name name, table_name name) RETURNS void
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp SET event_triggers = off AS
$f$
BEGIN
  IF table_name IS NULL OR table_name NOT IN ('buckets', 'objects') THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.drop_policy: the table must be buckets or objects';
  END IF;
  EXECUTE pg_catalog.format('DROP POLICY %I ON storage.%I', policy_name, table_name);
END
$f$;
COMMENT ON FUNCTION storage.create_policy(name, name, text, name[], text, text, boolean) IS
  'Creates a policy on storage.buckets or storage.objects (the database owner only). The expressions are parsed with search_path = pg_catalog, pg_temp: qualify your own functions and tables.';
ALTER FUNCTION storage.create_policy(name, name, text, name[], text, text, boolean) OWNER TO chelabase_admin;
ALTER FUNCTION storage.drop_policy(name, name) OWNER TO chelabase_admin;
REVOKE ALL ON FUNCTION storage.create_policy(name, name, text, name[], text, text, boolean) FROM PUBLIC;
REVOKE ALL ON FUNCTION storage.drop_policy(name, name) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION storage.create_policy(name, name, text, name[], text, text, boolean) TO {owner};
GRANT EXECUTE ON FUNCTION storage.drop_policy(name, name) TO {owner};
COMMENT ON SCHEMA storage IS '{STORAGE_SCHEMA_STAMP}';
COMMIT;
"#
    )
}

/// The health check's clause (appended to the data API's): the schema is the current one
/// (`chelabase_admin`'s, stamped [`STORAGE_SCHEMA_STAMP`], both tables `chelabase_admin`'s
/// with row level security enabled and forced, nothing in it the install didn't make, no ACL
/// entry beyond the install's, its triggers on, the policy helpers with event triggers off),
/// or it belongs to someone else (left alone, so reconcile doesn't rerun the bootstrap for it
/// on every pass).
pub fn storage_check_sql(owner_role: &str) -> String {
    format!(
        " AND EXISTS (SELECT FROM pg_catalog.pg_namespace n \
       JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner \
       WHERE n.nspname OPERATOR(pg_catalog.=) 'storage' \
         AND (r.rolname OPERATOR(pg_catalog.<>) 'chelabase_admin' \
           OR (pg_catalog.obj_description(n.oid, 'pg_namespace') OPERATOR(pg_catalog.=) '{STORAGE_SCHEMA_STAMP}' \
             AND (SELECT pg_catalog.count(*) FROM pg_catalog.pg_class c \
               WHERE c.relnamespace OPERATOR(pg_catalog.=) n.oid AND c.relkind OPERATOR(pg_catalog.=) 'r' \
                 AND c.relname OPERATOR(pg_catalog.=) ANY (ARRAY['buckets', 'objects']::pg_catalog.name[]) \
                 AND c.relowner OPERATOR(pg_catalog.=) r.oid AND c.relrowsecurity AND c.relforcerowsecurity) \
               OPERATOR(pg_catalog.=) 2 \
             AND NOT EXISTS ({objects}) \
             AND NOT EXISTS ({acl}) \
             AND NOT EXISTS (SELECT FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) t.tgrelid \
               WHERE c.relnamespace OPERATOR(pg_catalog.=) n.oid \
                 AND ((NOT t.tgisinternal AND t.tgenabled OPERATOR(pg_catalog.<>) 'A') OR (t.tgisinternal AND t.tgenabled OPERATOR(pg_catalog.=) 'D'))) \
             AND (SELECT pg_catalog.count(*) FROM pg_catalog.pg_proc p WHERE p.pronamespace OPERATOR(pg_catalog.=) n.oid \
               AND p.proname OPERATOR(pg_catalog.=) ANY (ARRAY['create_policy', 'drop_policy']::pg_catalog.name[]) \
               AND 'event_triggers=off' OPERATOR(pg_catalog.=) ANY (p.proconfig)) OPERATOR(pg_catalog.=) 2)))",
        objects = foreign_objects_sql(),
        acl = foreign_acl_sql(owner_role),
    )
}

/// One `DROP … CASCADE` per object in `storage` the install doesn't make (only a role acting
/// as `chelabase_admin` could have made one): functions, relations, types, operators,
/// collations, statistics, triggers and rules on its tables, and policies on the tickets.
/// Policies on `buckets` and `objects` are the owner's and stay.
pub fn foreign_objects_sql() -> String {
    let ns = "pg_catalog.to_regnamespace('storage')";
    format!(
        "SELECT pg_catalog.format('DROP ROUTINE IF EXISTS %s CASCADE', p.oid::pg_catalog.regprocedure) FROM pg_catalog.pg_proc p \
  WHERE p.pronamespace OPERATOR(pg_catalog.=) {ns} AND p.oid::pg_catalog.regprocedure::pg_catalog.text OPERATOR(pg_catalog.<>) ALL (ARRAY[\
'storage.foldername(text)', 'storage.filename(text)', 'storage.extension(text)', 'storage.object_ref(uuid,text)', \
'storage.create_policy(name,name,text,name[],text,text,boolean)', 'storage.drop_policy(name,name)', 'storage.protect_objects()', 'storage.guard_tickets()']) \
UNION ALL SELECT pg_catalog.format('DROP %s IF EXISTS %s CASCADE', CASE c.relkind WHEN 'i' THEN 'INDEX' WHEN 'I' THEN 'INDEX' WHEN 'S' THEN 'SEQUENCE' \
    WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'c' THEN 'TYPE' WHEN 'f' THEN 'FOREIGN TABLE' ELSE 'TABLE' END, c.oid::pg_catalog.regclass) \
  FROM pg_catalog.pg_class c WHERE c.relnamespace OPERATOR(pg_catalog.=) {ns} \
    AND NOT ((c.relkind OPERATOR(pg_catalog.=) 'r' AND c.relname OPERATOR(pg_catalog.=) ANY (ARRAY['buckets', 'objects', 'upload_tickets']::pg_catalog.name[])) \
      OR (c.relkind OPERATOR(pg_catalog.=) 'i' AND c.relname OPERATOR(pg_catalog.=) ANY (ARRAY['buckets_pkey', 'buckets_name_key', 'objects_pkey', 'objects_bucket_id_name_key', 'upload_tickets_pkey']::pg_catalog.name[])) \
      OR (c.relkind OPERATOR(pg_catalog.=) 'c' AND c.relname OPERATOR(pg_catalog.=) 'object_ref')) \
UNION ALL SELECT pg_catalog.format('DROP TYPE IF EXISTS %s CASCADE', t.oid::pg_catalog.regtype) FROM pg_catalog.pg_type t \
  WHERE t.typnamespace OPERATOR(pg_catalog.=) {ns} AND t.typrelid OPERATOR(pg_catalog.=) 0::pg_catalog.oid AND t.typtype OPERATOR(pg_catalog.<>) 'm' \
    AND NOT EXISTS (SELECT FROM pg_catalog.pg_type e WHERE e.typarray OPERATOR(pg_catalog.=) t.oid) \
UNION ALL SELECT pg_catalog.format('DROP OPERATOR IF EXISTS %s CASCADE', o.oid::pg_catalog.regoperator) FROM pg_catalog.pg_operator o \
  WHERE o.oprnamespace OPERATOR(pg_catalog.=) {ns} \
UNION ALL SELECT pg_catalog.format('DROP COLLATION IF EXISTS storage.%I CASCADE', k.collname) FROM pg_catalog.pg_collation k \
  WHERE k.collnamespace OPERATOR(pg_catalog.=) {ns} \
UNION ALL SELECT pg_catalog.format('DROP STATISTICS IF EXISTS storage.%I', x.stxname) FROM pg_catalog.pg_statistic_ext x \
  WHERE x.stxnamespace OPERATOR(pg_catalog.=) {ns} \
UNION ALL SELECT pg_catalog.format('DROP TRIGGER IF EXISTS %I ON %s CASCADE', t.tgname, t.tgrelid::pg_catalog.regclass) FROM pg_catalog.pg_trigger t \
  JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) t.tgrelid WHERE c.relnamespace OPERATOR(pg_catalog.=) {ns} AND NOT t.tgisinternal \
    AND NOT ((t.tgname OPERATOR(pg_catalog.=) 'protect_objects' AND c.relname OPERATOR(pg_catalog.=) 'objects') \
      OR (t.tgname OPERATOR(pg_catalog.=) 'guard_tickets' AND c.relname OPERATOR(pg_catalog.=) 'upload_tickets')) \
UNION ALL SELECT pg_catalog.format('DROP RULE IF EXISTS %I ON %s CASCADE', w.rulename, w.ev_class::pg_catalog.regclass) FROM pg_catalog.pg_rewrite w \
  JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) w.ev_class WHERE c.relnamespace OPERATOR(pg_catalog.=) {ns} AND c.relkind OPERATOR(pg_catalog.=) 'r' \
UNION ALL SELECT pg_catalog.format('DROP POLICY IF EXISTS %I ON storage.upload_tickets', y.polname) FROM pg_catalog.pg_policy y \
  WHERE y.polrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass('storage.upload_tickets')"
    )
}

/// Every ACL entry on `storage` and the objects in it, but the owners' own: `(target,
/// grantor, grantee, is_grantable, allowed)`, `target` as `REVOKE ALL <target> FROM …` takes
/// it, `allowed` the grantees the install grants there.
fn acl_entries_sql() -> String {
    let ns = "pg_catalog.to_regnamespace('storage')";
    let tables = "CASE WHEN c.relname OPERATOR(pg_catalog.=) 'upload_tickets' THEN ARRAY['chelabase_storage'] ELSE ARRAY['anon', 'authenticated', 'chelabase_storage'] END";
    format!(
        "SELECT 'ON SCHEMA storage' AS target, a.grantor, a.grantee, a.is_grantable, ARRAY['anon', 'authenticated', 'authenticator', 'chelabase_storage', '{{owner}}'] AS allowed \
    FROM pg_catalog.pg_namespace n, pg_catalog.aclexplode(n.nspacl) a WHERE n.oid OPERATOR(pg_catalog.=) {ns} AND a.grantee OPERATOR(pg_catalog.<>) n.nspowner \
  UNION ALL SELECT 'ON TABLE ' OPERATOR(pg_catalog.||) c.oid::pg_catalog.regclass::pg_catalog.text, a.grantor, a.grantee, a.is_grantable, {tables} \
    FROM pg_catalog.pg_class c, pg_catalog.aclexplode(c.relacl) a WHERE c.relnamespace OPERATOR(pg_catalog.=) {ns} AND a.grantee OPERATOR(pg_catalog.<>) c.relowner \
  UNION ALL SELECT pg_catalog.format('(%I) ON TABLE %s', t.attname, c.oid::pg_catalog.regclass), a.grantor, a.grantee, a.is_grantable, {tables} \
    FROM pg_catalog.pg_attribute t JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) t.attrelid, pg_catalog.aclexplode(t.attacl) a \
    WHERE c.relnamespace OPERATOR(pg_catalog.=) {ns} AND a.grantee OPERATOR(pg_catalog.<>) c.relowner \
  UNION ALL SELECT 'ON ROUTINE ' OPERATOR(pg_catalog.||) p.oid::pg_catalog.regprocedure::pg_catalog.text, a.grantor, a.grantee, a.is_grantable, \
      CASE WHEN p.proname OPERATOR(pg_catalog.=) ANY (ARRAY['create_policy', 'drop_policy']::pg_catalog.name[]) THEN ARRAY['{{owner}}'] \
        WHEN p.proname OPERATOR(pg_catalog.=) ANY (ARRAY['protect_objects', 'guard_tickets']::pg_catalog.name[]) THEN ARRAY[]::pg_catalog.text[] ELSE ARRAY['public'] END \
    FROM pg_catalog.pg_proc p, pg_catalog.aclexplode(p.proacl) a WHERE p.pronamespace OPERATOR(pg_catalog.=) {ns} AND a.grantee OPERATOR(pg_catalog.<>) p.proowner \
  UNION ALL SELECT 'ON TYPE ' OPERATOR(pg_catalog.||) t.oid::pg_catalog.regtype::pg_catalog.text, a.grantor, a.grantee, a.is_grantable, ARRAY['public'] \
    FROM pg_catalog.pg_type t, pg_catalog.aclexplode(t.typacl) a WHERE t.typnamespace OPERATOR(pg_catalog.=) {ns} AND a.grantee OPERATOR(pg_catalog.<>) t.typowner"
    )
}

/// The ACL entries the install wouldn't have made (`owner_role` is the database owner): one
/// granted by anyone but the object's owner, with a grant option, or to a grantee the install
/// doesn't grant there.
pub fn foreign_acl_sql(owner_role: &str) -> String {
    let entries = acl_entries_sql().replace("{owner}", &owner_role.replace('\'', "''"));
    format!(
        "SELECT FROM ({entries}) x LEFT JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) x.grantee \
  WHERE x.is_grantable OR NOT (COALESCE(r.rolname::pg_catalog.text, 'public') OPERATOR(pg_catalog.=) ANY (x.allowed)) \
    OR x.grantor OPERATOR(pg_catalog.<>) ALL (ARRAY(SELECT o.oid FROM pg_catalog.pg_roles o WHERE o.rolname OPERATOR(pg_catalog.=) 'chelabase_admin' OR o.rolsuper))"
    )
}

/// Whether the script stopped because `storage` belongs to someone else (from its error).
pub fn foreign_schema(error: &str) -> bool {
    error.contains(&format!("ERROR:  {FOREIGN_SCHEMA_MARK}:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_api::schema_check_sql;
    use crate::fakes::{FakeDbRow, entries, ready_database, worker};
    use chelabase_metadata::DataApiState;
    use chelabase_types::BranchId;

    fn sql() -> String {
        storage_schema_sql("postgres")
    }

    /// The text between `start` and the next `end`, both included.
    fn between<'a>(sql: &'a str, start: &str, end: &str) -> &'a str {
        let from = sql
            .find(start)
            .unwrap_or_else(|| panic!("{start:?} not in {sql}"));
        let rest = &sql[from..];
        let to = rest
            .find(end)
            .unwrap_or_else(|| panic!("{end:?} after {start:?} not in {sql}"));
        &rest[..to + end.len()]
    }

    /// Two transactions, each under the bootstrap lock: the role's lockdown commits first
    /// (S2: a user's own `storage` schema stops only the second), then its sessions are ended
    /// again, then the schema install.
    #[test]
    fn storage_schema_script_is_two_transactions_under_the_bootstrap_lock() {
        let sql = sql();
        let head = format!(
            "BEGIN;\nSET search_path = pg_catalog, pg_temp;\nSET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';\nSELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});\n"
        );
        assert!(
            sql.starts_with(&format!("{STORAGE_SCHEMA_MARK}\n{head}")),
            "{sql}"
        );
        assert_eq!(sql.matches("BEGIN;").count(), 2, "{sql}");
        assert_eq!(sql.matches("COMMIT;").count(), 2, "{sql}");
        let (lockdown, rest) = sql.split_once("COMMIT;\n").unwrap();
        let (between_them, install) = rest.split_once(&head).expect("the install's own head");
        assert!(
            between_them
                .starts_with("DO $$ BEGIN\n  PERFORM pg_catalog.pg_terminate_backend(a.pid)"),
            "{between_them}"
        );
        assert!(
            between_them.trim_end().ends_with("END $$;"),
            "{between_them}"
        );
        assert_eq!(between_them.matches(';').count(), 2, "{between_them}");
        // The lockdown touches no storage object but voiding tickets; the install does the rest.
        assert!(
            !lockdown.contains("CREATE SCHEMA") && !lockdown.contains(FOREIGN_SCHEMA_MARK),
            "{lockdown}"
        );
        assert!(
            install.contains(FOREIGN_SCHEMA_MARK) && install.trim_end().ends_with("COMMIT;"),
            "{install}"
        );
    }

    /// AC 2: every object is made only when missing, so a second run changes nothing but
    /// the grants it resets to the same list.
    #[test]
    fn storage_schema_script_is_idempotent() {
        let sql = sql();
        let mut creates = 0;
        for line in sql.lines().map(str::trim_start) {
            if !line.starts_with("CREATE ") {
                continue;
            }
            creates += 1;
            assert!(
                line.starts_with("CREATE SCHEMA IF NOT EXISTS ")
                    || line.starts_with("CREATE TABLE IF NOT EXISTS ")
                    || line.starts_with("CREATE OR REPLACE FUNCTION ")
                    || line.starts_with("CREATE OR REPLACE TRIGGER ")
                    // Inside a block that checks first.
                    || line.starts_with("CREATE ROLE chelabase_storage ")
                    || line.starts_with("CREATE TYPE storage.object_ref "),
                "not idempotent: {line}"
            );
        }
        assert!(creates >= 10, "{sql}");
        assert!(
            sql.contains("IF role IS NULL THEN\n    CREATE ROLE chelabase_storage "),
            "{sql}"
        );
        assert!(sql.contains(
            "IF pg_catalog.to_regtype('storage.object_ref') IS NULL THEN\n    CREATE TYPE storage.object_ref "
        ), "{sql}");
        // No DROP of anything the user may depend on (rows, the type, the tables).
        // The healing loop's own DROPs only remove what the install didn't make.
        let sql = sql.replace(&foreign_objects_sql(), "");
        for line in sql.lines() {
            assert!(
                !line.contains("DROP TABLE")
                    && !line.contains("DROP TYPE")
                    && !line.contains("DROP SCHEMA")
                    && !line.contains("TRUNCATE"),
                "{line}"
            );
        }
    }

    /// AC 1: both tables, `chelabase_admin`'s, with row level security enabled and forced
    /// and no policy; the shape the later milestones read.
    #[test]
    fn storage_schema_tables_are_the_admins_with_forced_rls() {
        let sql = sql();
        assert!(
            sql.contains("CREATE SCHEMA IF NOT EXISTS storage AUTHORIZATION chelabase_admin;"),
            "{sql}"
        );
        for table in ["storage.buckets", "storage.objects"] {
            for statement in [
                format!("ALTER TABLE {table} OWNER TO chelabase_admin;"),
                format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;"),
                format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY;"),
            ] {
                assert!(sql.contains(&statement), "{statement}");
            }
        }
        assert!(
            sql.contains("ALTER TYPE storage.object_ref OWNER TO chelabase_admin;"),
            "{sql}"
        );
        assert!(
            !sql.lines()
                .any(|l| l.trim_start().starts_with("CREATE POLICY")),
            "no default policy: {sql}"
        );
        let buckets = between(&sql, "CREATE TABLE IF NOT EXISTS storage.buckets (", ");");
        for column in [
            "id uuid",
            "name text NOT NULL UNIQUE",
            "public boolean",
            "file_size_limit bigint",
            "allowed_mime_types text[]",
            "created_at timestamptz",
        ] {
            assert!(buckets.contains(column), "{column}: {buckets}");
        }
        let objects = between(&sql, "CREATE TABLE IF NOT EXISTS storage.objects (", ");");
        for column in [
            "id uuid",
            "bucket_id uuid NOT NULL REFERENCES storage.buckets (id)",
            "name text NOT NULL",
            "owner uuid",
            "metadata jsonb",
            "size bigint",
            "content_type text",
            "etag text",
            "version_id uuid",
            "created_at timestamptz",
            "updated_at timestamptz",
            "UNIQUE (bucket_id, name)",
        ] {
            assert!(objects.contains(column), "{column}: {objects}");
        }
        assert!(
            sql.contains(&format!(
                "COMMENT ON SCHEMA storage IS '{STORAGE_SCHEMA_STAMP}';\nCOMMIT;"
            )),
            "the stamp comes last: {sql}"
        );
    }

    /// The storage service's role, locked down each run exactly as the data API locks down
    /// `chelabase_admin`: whatever an owner made of a pre-created role (LOGIN, a password, a
    /// connection limit, settings, members, admins, memberships in other roles, objects it
    /// owns, open sessions) is gone before the schema is touched.
    #[test]
    fn storage_schema_role_is_locked_down_like_the_admin() {
        let sql = sql();
        let lockdown = between(&sql, "DO $$ DECLARE\n  role pg_catalog.oid", "END $$;");
        let schema_at = sql.find("CREATE SCHEMA IF NOT EXISTS storage").unwrap();
        assert!(sql.find(lockdown).unwrap() < schema_at, "{sql}");
        // S1: every role that reaches it, at any depth, is a former member whose sessions end.
        assert!(lockdown.contains("members := ARRAY(WITH RECURSIVE reach(oid) AS (\n      SELECT a.member FROM pg_catalog.pg_auth_members a WHERE a.roleid OPERATOR(pg_catalog.=) role\n      UNION SELECT a.member FROM pg_catalog.pg_auth_members a JOIN reach ON a.roleid OPERATOR(pg_catalog.=) reach.oid)\n    SELECT oid FROM reach);"), "{lockdown}");
        for part in [
            "IF role IS NULL THEN\n    CREATE ROLE chelabase_storage NOLOGIN NOINHERIT BYPASSRLS PASSWORD NULL CONNECTION LIMIT 0;",
            "ALTER ROLE chelabase_storage NOLOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION BYPASSRLS NOINHERIT PASSWORD NULL CONNECTION LIMIT 0;",
            "EXECUTE pg_catalog.format('ALTER ROLE chelabase_storage IN DATABASE %I RESET ALL', m.datname);",
            "ALTER ROLE chelabase_storage RESET ALL;",
            "ALTER ROLE chelabase_storage SET search_path = pg_catalog, pg_temp;",
            // Its members and admins (an ADMIN grant is a membership row too)...
            "WHERE a.roleid OPERATOR(pg_catalog.=) role",
            "EXECUTE pg_catalog.format('REVOKE chelabase_storage FROM %I GRANTED BY %I CASCADE', m.member, m.grantor);",
            // ...and its own memberships in other roles.
            "WHERE a.member OPERATOR(pg_catalog.=) role",
            "EXECUTE pg_catalog.format('REVOKE %I FROM chelabase_storage GRANTED BY %I CASCADE', m.granted, m.grantor);",
            // Objects a pre-created role owns here go to the owner, so nothing runs as it.
            "REASSIGN OWNED BY chelabase_storage TO \"postgres\"",
            "PERFORM pg_catalog.set_config('chelabase.storage_former_members', pg_catalog.array_to_string(members, ','), false);",
            "PERFORM pg_catalog.pg_terminate_backend(a.pid)",
            // Tickets minted while it was tampered with are void.
            "IF (joined OR tampered) AND EXISTS (SELECT FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace\n      JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner\n      WHERE n.nspname OPERATOR(pg_catalog.=) 'storage' AND c.relname OPERATOR(pg_catalog.=) 'upload_tickets' AND r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN\n    DELETE FROM storage.upload_tickets;",
        ] {
            assert!(lockdown.contains(part), "{part}: {lockdown}");
        }
        // Tampered: any attribute or setting beyond the ones it is given here.
        let tampered = between(lockdown, "tampered :=", ";\n");
        for attribute in [
            "rolcanlogin",
            "rolsuper",
            "rolcreaterole",
            "rolcreatedb",
            "rolreplication",
            "NOT a.rolbypassrls",
            "rolpassword IS NOT NULL",
            "rolconnlimit OPERATOR(pg_catalog.<>) 0",
            "pg_db_role_setting",
            "ARRAY['search_path=pg_catalog, pg_temp']",
        ] {
            assert!(tampered.contains(attribute), "{attribute}: {tampered}");
        }
        let joined = between(lockdown, "joined :=", ";\n");
        assert!(
            joined.contains(
                "a.roleid OPERATOR(pg_catalog.=) role OR a.member OPERATOR(pg_catalog.=) role"
            ),
            "{joined}"
        );
        // The sessions are ended again once the revokes are committed.
        let after = sql.split_once("COMMIT;\n").unwrap().1;
        assert!(
            after.contains("pg_catalog.current_setting('chelabase.storage_former_members', true)")
                && after.contains("r.rolname OPERATOR(pg_catalog.=) 'chelabase_storage'"),
            "{after}"
        );
        assert_eq!(STORAGE_ROLE, "chelabase_storage");
    }

    /// The grants are reset each run and then given exactly: `USAGE` (never `CREATE`) on the
    /// schema; `SELECT` for `anon`; `authenticated` writes every object column a caller
    /// declares, the managed ones included (so RLS policies see them; the ticket trigger
    /// vouches for them); tickets only for the storage role. Nobody gets `TRUNCATE`,
    /// `TRIGGER`, `REFERENCES`, a grant option or anything on the tickets but the storage role.
    #[test]
    fn storage_schema_grants_let_callers_declare_managed_columns() {
        let sql = sql();
        assert!(
            sql.contains("REVOKE ALL ON SCHEMA storage FROM PUBLIC;"),
            "{sql}"
        );
        assert!(sql.contains("GRANT USAGE ON SCHEMA storage TO anon, authenticated, authenticator, chelabase_storage, \"postgres\";"), "{sql}");
        let reset = "REVOKE ALL ON storage.buckets, storage.objects, storage.upload_tickets FROM PUBLIC, anon, authenticated, authenticator, chelabase_storage, \"postgres\";";
        let reset_at = sql.find(reset).unwrap_or_else(|| panic!("{reset}: {sql}"));
        let grants: Vec<&str> = sql
            .lines()
            .filter(|l| l.starts_with("GRANT ") && l.contains(" ON storage."))
            .collect();
        for grant in &grants {
            assert!(
                sql.find(grant).unwrap() > reset_at,
                "granted before the reset: {grant}"
            );
            assert!(
                !grant.contains("TRUNCATE")
                    && !grant.contains("TRIGGER")
                    && !grant.contains("REFERENCES")
                    && !grant.contains("GRANT OPTION")
                    && !grant.contains(" ALL "),
                "{grant}"
            );
            if grant.contains("upload_tickets") {
                assert_eq!(
                    *grant,
                    "GRANT SELECT, INSERT, DELETE ON storage.upload_tickets TO chelabase_storage;"
                );
            }
            if !grant.ends_with(" TO chelabase_storage;") {
                assert!(!grant.contains("authenticator"), "{grant}");
            }
        }
        assert_eq!(
            grants,
            [
                "GRANT SELECT ON storage.buckets, storage.objects TO anon, authenticated;",
                "GRANT INSERT (id, name, public, file_size_limit, allowed_mime_types), UPDATE (name, public, file_size_limit, allowed_mime_types), DELETE ON storage.buckets TO authenticated;",
                "GRANT INSERT (id, bucket_id, name, owner, metadata, size, content_type, etag, version_id), UPDATE (bucket_id, name, owner, metadata, size, content_type, etag, version_id), DELETE ON storage.objects TO authenticated;",
                "GRANT SELECT, INSERT, UPDATE, DELETE ON storage.buckets, storage.objects TO chelabase_storage;",
                "GRANT SELECT, INSERT, DELETE ON storage.upload_tickets TO chelabase_storage;",
            ]
        );
        for column in MANAGED_COLUMNS {
            assert!(grants[2].contains(column), "{column}");
        }
    }

    /// The tickets table: the admin's, no RLS (nobody but the storage role has a grant), and
    /// a trigger that refuses with [`TICKET_FORGED`] any write not made as the storage role
    /// (so `pg_write_all_data` mints nothing), stamps the timeline, and caps the lifetime.
    #[test]
    fn storage_schema_tickets_are_written_by_the_storage_role_only() {
        let sql = sql();
        let table = between(
            &sql,
            "CREATE TABLE IF NOT EXISTS storage.upload_tickets (",
            ");",
        );
        for column in [
            "version_id uuid PRIMARY KEY",
            "bucket_id uuid NOT NULL",
            "name text NOT NULL",
            "size bigint NOT NULL",
            "etag text NOT NULL",
            "expires_at timestamptz NOT NULL",
            "timeline_id text NOT NULL",
        ] {
            assert!(table.contains(column), "{column}: {table}");
        }
        assert!(
            sql.contains("ALTER TABLE storage.upload_tickets OWNER TO chelabase_admin;"),
            "{sql}"
        );
        let body = between(
            &sql,
            "CREATE OR REPLACE FUNCTION storage.guard_tickets()",
            "$f$;",
        );
        assert!(
            body.contains(
                "LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp"
            ),
            "{body}"
        );
        assert!(body.contains(&format!("IF current_user <> 'chelabase_storage' THEN\n    RAISE EXCEPTION USING ERRCODE = '{TICKET_FORGED}'")), "{body}");
        assert!(
            body.contains(
                "NEW.timeline_id := pg_catalog.current_setting('neon.timeline_id', true);"
            ),
            "{body}"
        );
        assert!(
            body.contains("IF NEW.timeline_id IS NULL OR NEW.timeline_id = '' THEN"),
            "{body}"
        );
        assert!(body.contains(&format!("NEW.expires_at <= clock_timestamp() OR NEW.expires_at > clock_timestamp() + interval '{TICKET_MAX_LIFETIME}'")), "{body}");
        assert!(sql.contains("CREATE OR REPLACE TRIGGER guard_tickets BEFORE INSERT OR UPDATE ON storage.upload_tickets\n  FOR EACH ROW EXECUTE FUNCTION storage.guard_tickets();"), "{sql}");
        assert!(
            sql.contains("ALTER TABLE storage.upload_tickets ENABLE ALWAYS TRIGGER guard_tickets;"),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER FUNCTION storage.guard_tickets() OWNER TO CURRENT_USER;"),
            "{sql}"
        );
    }

    /// AC 3: `protect_objects` (after RLS, so a policy's 42501 comes first) accepts the
    /// managed columns of an insert, or of an update that changes one, only by consuming the
    /// matching unexpired ticket minted on this timeline; no role is exempt. `ENABLE ALWAYS`.
    #[test]
    fn storage_schema_objects_trigger_consumes_one_ticket() {
        let sql = sql();
        let body = between(
            &sql,
            "CREATE OR REPLACE FUNCTION storage.protect_objects()",
            "$f$;",
        );
        assert!(
            body.contains(
                "LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp"
            ),
            "{body}"
        );
        assert!(!body.contains("current_user"), "no role is exempt: {body}");
        assert!(body.contains("IF TG_OP = 'UPDATE' AND NEW.bucket_id IS NOT DISTINCT FROM OLD.bucket_id AND NEW.version_id IS NOT DISTINCT FROM OLD.version_id\n      AND NEW.size IS NOT DISTINCT FROM OLD.size AND NEW.etag IS NOT DISTINCT FROM OLD.etag THEN\n    RETURN NULL;"), "{body}");
        let consume = between(body, "DELETE FROM storage.upload_tickets t", ";");
        for part in [
            "t.version_id = NEW.version_id",
            "t.bucket_id = NEW.bucket_id",
            "t.name = NEW.name",
            "t.size = NEW.size",
            "t.etag = NEW.etag",
            "t.expires_at > clock_timestamp()",
            "t.timeline_id = pg_catalog.current_setting('neon.timeline_id', true)",
            "RETURNING t.version_id INTO consumed",
        ] {
            assert!(consume.contains(part), "{part}: {consume}");
        }
        assert!(
            body.contains(&format!(
                "IF consumed IS NULL THEN\n    RAISE EXCEPTION USING ERRCODE = '{TICKET_MISSING}'"
            )),
            "{body}"
        );
        assert!(sql.contains("CREATE OR REPLACE TRIGGER protect_objects AFTER INSERT OR UPDATE ON storage.objects\n  FOR EACH ROW EXECUTE FUNCTION storage.protect_objects();"), "{sql}");
        assert!(
            sql.contains("ALTER TABLE storage.objects ENABLE ALWAYS TRIGGER protect_objects;"),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER FUNCTION storage.protect_objects() OWNER TO chelabase_admin;"),
            "{sql}"
        );
    }

    /// The triggers' codes are their own: never RLS's 42501, never the data API's `CB001`
    /// (stale schema version: rest retries) or `CB002` (response too large).
    #[test]
    fn storage_schema_ticket_codes_are_dedicated() {
        assert_eq!((TICKET_MISSING, TICKET_FORGED), ("CB003", "CB004"));
        let data_api = crate::data_api::data_api_sql("postgres", "pw", "public");
        for code in [TICKET_MISSING, TICKET_FORGED] {
            assert_ne!(code, "42501");
            assert!(
                !data_api.contains(&format!("'{code}'")),
                "{code} is the data API's"
            );
            assert_eq!(
                sql().matches(&format!("ERRCODE = '{code}'")).count(),
                1,
                "{code}"
            );
        }
        assert!(
            !sql().contains("ERRCODE = '42501'"),
            "the triggers never raise RLS's code"
        );
    }

    /// The policy helpers: `SECURITY DEFINER` as the tables' owner, with a pinned search path,
    /// only for the database owner, only on the two tables, and with no way to end the
    /// `CREATE POLICY` statement and start another (no `;` in an expression, each expression
    /// on lines of its own so a `--` comment ends with its line).
    #[test]
    fn storage_schema_policy_helpers_run_as_the_admin_for_the_owner_only() {
        let sql = sql();
        for (function, signature) in [
            (
                "storage.create_policy",
                "storage.create_policy(name, name, text, name[], text, text, boolean)",
            ),
            ("storage.drop_policy", "storage.drop_policy(name, name)"),
        ] {
            let body = between(
                &sql,
                &format!("CREATE OR REPLACE FUNCTION {function}("),
                "$f$;",
            );
            assert!(
                body.contains(
                    "LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp"
                ),
                "{body}"
            );
            assert!(
                body.contains(
                    "IF table_name IS NULL OR table_name NOT IN ('buckets', 'objects') THEN"
                ),
                "{body}"
            );
            assert!(body.contains("ON storage.%I"), "{body}");
            assert!(
                sql.contains(&format!(
                    "ALTER FUNCTION {signature} OWNER TO chelabase_admin;"
                )),
                "{signature}"
            );
            assert!(
                sql.contains(&format!("REVOKE ALL ON FUNCTION {signature} FROM PUBLIC;")),
                "{signature}"
            );
            assert!(
                sql.contains(&format!(
                    "GRANT EXECUTE ON FUNCTION {signature} TO \"postgres\";"
                )),
                "{signature}"
            );
        }
        let create = between(
            &sql,
            "CREATE OR REPLACE FUNCTION storage.create_policy(",
            "$f$;",
        );
        assert!(
            create.contains(
                "pg_catalog.strpos(using_expr, ';') > 0 OR pg_catalog.strpos(check_expr, ';') > 0"
            ),
            "{create}"
        );
        assert!(create.contains("IF pg_catalog.upper(command) NOT IN ('ALL', 'SELECT', 'INSERT', 'UPDATE', 'DELETE') THEN"), "{create}");
        assert!(
            create.contains("E'\\n  USING ((\\n' || using_expr || E'\\n))'"),
            "{create}"
        );
        assert!(
            create.contains("E'\\n  WITH CHECK ((\\n' || check_expr || E'\\n))'"),
            "{create}"
        );
        assert!(create.contains("pg_catalog.quote_ident(r)"), "{create}");
    }

    /// B1: the policy helpers run their DDL with event triggers off, so no owner event
    /// trigger runs as `chelabase_admin` (the superuser lets the admin set the parameter).
    #[test]
    fn storage_schema_policy_helpers_fire_no_event_trigger() {
        let sql = sql();
        assert!(
            sql.contains("GRANT SET ON PARAMETER event_triggers TO chelabase_admin;"),
            "{sql}"
        );
        for function in ["storage.create_policy(", "storage.drop_policy("] {
            let body = between(
                &sql,
                &format!("CREATE OR REPLACE FUNCTION {function}"),
                "$f$;",
            );
            assert!(body.contains("LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp SET event_triggers = off AS"), "{body}");
        }
    }

    /// B1: every run removes what it didn't make in `storage`, revokes every ACL entry before
    /// granting its own (whoever granted it, grant options included), and turns every trigger
    /// on its tables back on.
    #[test]
    fn storage_schema_heals_what_it_did_not_make() {
        let sql = sql();
        let install = sql.rsplit_once(FOREIGN_SCHEMA_MARK).unwrap().1;
        let heal = install
            .find(&format!(
                "FOR d IN {} LOOP\n    EXECUTE d;",
                foreign_objects_sql()
            ))
            .expect("the drop loop");
        let revoke = install
            .find("FOR e IN SELECT g.rolname AS grantor, pg_catalog.format(")
            .expect("the ACL reset");
        let first_grant = install.find("\nGRANT SELECT ON storage.buckets").unwrap();
        assert!(heal < revoke && revoke < first_grant, "{install}");
        assert!(
            install.contains("EXECUTE pg_catalog.format('SET LOCAL ROLE %I', e.grantor);"),
            "{install}"
        );
        for table in ["buckets", "objects", "upload_tickets"] {
            assert!(
                install.contains(&format!("ALTER TABLE storage.{table} ENABLE TRIGGER ALL;")),
                "{table}"
            );
        }
        assert!(
            install.contains("GRANT USAGE ON TYPE storage.object_ref TO PUBLIC;"),
            "{install}"
        );
        let objects = foreign_objects_sql();
        for part in [
            "DROP ROUTINE IF EXISTS",
            "DROP TRIGGER IF EXISTS",
            "DROP RULE IF EXISTS",
            "DROP OPERATOR IF EXISTS",
            "DROP TYPE IF EXISTS",
            "DROP STATISTICS IF EXISTS",
            "DROP COLLATION IF EXISTS",
            "'storage.create_policy(name,name,text,name[],text,text,boolean)'",
            "'objects_bucket_id_name_key'",
        ] {
            assert!(objects.contains(part), "{part}: {objects}");
        }
        let acl = foreign_acl_sql("postgres");
        for part in [
            "x.is_grantable",
            "x.grantor OPERATOR(pg_catalog.<>) ALL",
            "attacl",
            "proacl",
            "nspacl",
            "typacl",
        ] {
            assert!(acl.contains(part), "{part}: {acl}");
        }
    }

    /// S3: a reserved default schema (stored before it was reserved) gets none of the data
    /// API's grants.
    #[tokio::test]
    async fn storage_schema_reserved_default_schema_gets_no_grants() {
        for schema in [
            "storage",
            "chelabase",
            "chelabase_x",
            "pg_catalog",
            "information_schema",
        ] {
            assert!(crate::data_api::reserved_default_schema(schema), "{schema}");
        }
        assert!(!crate::data_api::reserved_default_schema("storage_app"));
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.metadata.state().db_rows.insert(
            main.id,
            vec![FakeDbRow::new("app", DataApiState::On, "storage")],
        );
        w.computes.sql.lock().unwrap().clear();
        w.bootstrap_data_api(&main, "app").await.unwrap();
        let scripts: Vec<String> = w
            .computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.starts_with("BEGIN;\n"))
            .map(|(_, _, s)| s.clone())
            .collect();
        assert_eq!(scripts.len(), 1, "{scripts:?}");
        // Both blocks that name the default schema are switched off.
        assert!(scripts[0].contains("  IF false THEN\n    ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA \"storage\""), "{}", scripts[0]);
        assert!(scripts[0].contains("  IF false AND NOT EXISTS (SELECT FROM chelabase.bootstrap) THEN\n    GRANT SELECT ON ALL TABLES IN SCHEMA \"storage\""), "{}", scripts[0]);
        assert_eq!(
            scripts[0]
                .matches("to_regnamespace('\"storage\"') IS NOT NULL")
                .count(),
            0,
            "{}",
            scripts[0]
        );
    }

    /// The helpers m5 and m11 use: pure functions anyone may call.
    #[test]
    fn storage_schema_helpers_are_pure_and_public() {
        let sql = sql();
        for signature in [
            "storage.foldername(text)",
            "storage.filename(text)",
            "storage.extension(text)",
            "storage.object_ref(uuid, text)",
        ] {
            let name = signature.split('(').next().unwrap();
            let head = between(&sql, &format!("CREATE OR REPLACE FUNCTION {name}("), " AS");
            assert!(
                head.contains("IMMUTABLE")
                    && head.contains("SET search_path = pg_catalog, pg_temp"),
                "{head}"
            );
            assert!(!head.contains("SECURITY DEFINER"), "{head}");
            assert!(
                sql.contains(&format!(
                    "ALTER FUNCTION {signature} OWNER TO CURRENT_USER;"
                )),
                "{signature}"
            );
        }
        assert!(sql.contains("GRANT EXECUTE ON FUNCTION storage.foldername(text), storage.filename(text), storage.extension(text), storage.object_ref(uuid, text) TO PUBLIC;"), "{sql}");
        assert!(
            sql.contains("CREATE TYPE storage.object_ref AS (bucket_id uuid, name text);"),
            "{sql}"
        );
    }

    /// A `storage` schema someone else owns stops the script before its first change.
    #[test]
    fn storage_schema_foreign_schema_stops_the_script_first() {
        let sql = sql();
        let check = sql.find(FOREIGN_SCHEMA_MARK).expect("the foreign check");
        let lock = sql.rfind("pg_advisory_xact_lock").unwrap();
        let first_change = sql.find("CREATE SCHEMA").unwrap();
        assert!(lock < check && check < first_change, "{sql}");
        // ...but after the role's lockdown has committed (S2).
        assert!(
            sql.find("ALTER ROLE chelabase_storage NOLOGIN").unwrap()
                < sql.find("COMMIT;").unwrap(),
            "{sql}"
        );
        assert!(sql.find("COMMIT;").unwrap() < check, "{sql}");
        assert!(
            between(
                &sql,
                "IF EXISTS (SELECT FROM pg_catalog.pg_namespace n",
                "END IF;"
            )
            .contains("r.rolname OPERATOR(pg_catalog.<>) 'chelabase_admin'")
        );
        assert!(foreign_schema(&format!(
            "psql in compute ep-1 exited with 3: ERROR:  {FOREIGN_SCHEMA_MARK}: the storage schema is not Chelabase's"
        )));
        assert!(!foreign_schema(
            "psql in compute ep-1 exited with 3: ERROR:  permission denied"
        ));
    }

    /// AC 2: reconcile's check fails until the current version is installed.
    #[test]
    fn storage_schema_is_part_of_the_health_check() {
        let check = schema_check_sql("public");
        let storage = storage_check_sql("postgres");
        assert!(check.ends_with(&storage), "{check}");
        let stamp = format!("'{STORAGE_SCHEMA_STAMP}'");
        for part in [
            stamp.as_str(),
            "relforcerowsecurity",
            "relrowsecurity",
            "'chelabase_admin'",
            "'buckets'",
            "'objects'",
            // B1: anything planted is noticed, so reconcile reruns the install.
            &format!("NOT EXISTS ({})", foreign_objects_sql()),
            &format!("NOT EXISTS ({})", foreign_acl_sql("postgres")),
            "t.tgenabled OPERATOR(pg_catalog.<>) 'A'",
            "t.tgenabled OPERATOR(pg_catalog.=) 'D'",
            "'event_triggers=off'",
        ] {
            assert!(storage.contains(part), "{part}: {storage}");
        }
    }

    /// AC 1: provisioning installs it right after the data API script, in each served
    /// database.
    #[tokio::test]
    async fn storage_schema_is_installed_after_the_data_api_in_each_served_database() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        let data_api = entries(&journal)
            .iter()
            .position(|e| e.starts_with(&format!("compute.sql {ep} app BEGIN;")))
            .expect("the data API script");
        let storage = entries(&journal)
            .iter()
            .position(|e| e == &format!("compute.sql {ep} app {STORAGE_SCHEMA_MARK}"))
            .expect("the storage script");
        assert!(data_api < storage, "{:?}", entries(&journal));
        let scripts: Vec<String> = w
            .computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.starts_with(STORAGE_SCHEMA_MARK))
            .map(|(_, _, s)| s.clone())
            .collect();
        assert_eq!(scripts, [storage_schema_sql("postgres")]);

        w.metadata.state().db_rows.insert(
            main.id,
            vec![
                FakeDbRow::new("app", DataApiState::On, "public"),
                FakeDbRow::new("reports", DataApiState::On, "public"),
            ],
        );
        w.computes.sql.lock().unwrap().clear();
        w.bootstrap_data_api(&main, "app").await.unwrap();
        let databases: Vec<String> = w
            .computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.starts_with(STORAGE_SCHEMA_MARK))
            .map(|(_, d, _)| d.clone())
            .collect();
        assert_eq!(databases, ["app", "reports"]);
    }

    /// A user's own `storage` schema is left alone: the bootstrap still succeeds. Any other
    /// failure of the script fails it, like the data API script's.
    #[tokio::test]
    async fn storage_schema_failures_fail_the_bootstrap_unless_the_schema_is_foreign() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.computes.fail_in_with("app", STORAGE_SCHEMA_MARK, &format!("psql in compute ep exited with 3: ERROR:  {FOREIGN_SCHEMA_MARK}: the storage schema is not Chelabase's"));
        w.bootstrap_data_api(&main, "app").await.unwrap();

        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.computes.fail_in_with(
            "app",
            STORAGE_SCHEMA_MARK,
            "psql in compute ep exited with 3: ERROR:  permission denied",
        );
        let err = format!(
            "{:#}",
            w.bootstrap_data_api(&main, "app").await.unwrap_err()
        );
        assert!(
            err.contains("installing the storage schema") && err.contains("permission denied"),
            "{err}"
        );
    }

    /// No secret key may use the storage role.
    #[tokio::test]
    async fn storage_schema_role_is_reserved_for_keys() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        assert_eq!(
            w.check_data_role(BranchId::from(id), STORAGE_ROLE).await,
            crate::data_api::RoleCheck::Refused("the role chelabase_storage is reserved".into())
        );
    }

    /// The merge's exclusion list names the schema (`chelabase-query`'s own test covers the
    /// data API's reserved list).
    #[test]
    fn storage_schema_is_excluded_from_merge() {
        assert!(chelabase_schema::exclusions::EXCLUDED_SCHEMAS.contains(&STORAGE_SCHEMA));
    }
}
