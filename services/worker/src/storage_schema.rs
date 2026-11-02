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
//! - [`STORAGE_ROLE`] (`chelabase_storage`, BYPASSRLS) is the storage service's own login
//!   (m5), locked down on every run like the data API's `chelabase_admin`: attributes (LOGIN,
//!   [`STORAGE_CONNECTION_LIMIT`], no expiry), exactly the branch's password verifier
//!   (`crate::data_api`'s storage login: never in connection details), settings (then
//!   `search_path = pg_catalog, pg_temp` and `event_triggers = off`, so no event trigger,
//!   a login one included, fires for its sessions; it has no `SET` on the parameter to turn
//!   them back on), members, admins and its own memberships revoked, objects it owns here
//!   reassigned to the owner. Its former members' sessions are ended before and again after
//!   the commit, and its own too when it had drifted or had a membership either way (else
//!   they are the storage service's). Changed attributes, password or settings, or a member,
//!   void its tickets; the health check notices any drift or membership, so reconcile reruns
//!   the lockdown.
//! - `anon` and `authenticated` get `USAGE` on the schema (never `CREATE`) and table
//!   privileges that row level security still gates. `authenticated` writes every column of
//!   `storage.objects` a caller declares, the [`MANAGED_COLUMNS`] included, so RLS policies
//!   see them. `authenticator` gets `USAGE` only (it parses statements before it switches
//!   role).
//! - Upload tickets vouch for the managed columns. Only [`STORAGE_ROLE`] writes
//!   `storage.upload_tickets` (`guard_tickets` checks `current_user`, so `pg_write_all_data`
//!   mints nothing: [`TICKET_FORGED`]); a ticket lives at most [`TICKET_MAX_LIFETIME`] and
//!   records the Neon timeline it was minted on, the role of the request it was minted for
//!   and the object's `owner`. `protect_objects` (after RLS, so a policy's 42501 comes
//!   first; `SECURITY INVOKER`, so `current_user` is the inserting role) lets an insert, or an
//!   update changing a managed column, through only by consuming (`consume_ticket`) the
//!   ticket matching its bucket, name, version, size, etag, owner and role, unexpired and of
//!   this timeline; otherwise [`TICKET_MISSING`]. No role is exempt, the owner included.
//! - Resumable upload sessions (`storage.upload_sessions`, m7) are the storage service's
//!   alone, like the tickets: `guard_sessions` lets only [`STORAGE_ROLE`] write them, stamps
//!   the timeline, caps them at 24 hours and lets an update change only the parts (and bring
//!   the expiry closer). A session vouches for nothing: completing one mints a ticket and
//!   writes the row as the caller, so row level security decides again.
//! - A branch copies unconsumed tickets but never its parent's timeline (`neon.timeline_id`
//!   is a postmaster setting, fixed per compute), so they can't be used there.
//! - Owner-level roles (the database owner is a Neon privileged role: `BYPASSRLS`, a member of
//!   `pg_read_all_data` and `pg_write_all_data`) read and write past both RLS and the column
//!   grants, as on any table; the triggers still hold them to tickets.
//!
//! - The policy helpers run with `event_triggers = off` (the bootstrap grants
//!   `chelabase_admin` `SET` on the parameter), so an owner's event trigger never runs as
//!   `chelabase_admin` inside them. They apply the protected-branch guard themselves: while
//!   it is installed only an unmarked owner session or a superuser may change policies
//!   (stricter than the guard, which also lets exempt logins and Admin members through).
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
use chelabase_types::table_api::{BRANCH_PROTECTED_HINT, SCHEMA_LOCKED_ROLE};

/// The schema's name, reserved as a data API default schema and never served by `/data/v1`.
pub const STORAGE_SCHEMA: &str = "storage";

/// The storage service's role.
pub const STORAGE_ROLE: &str = "chelabase_storage";

/// The columns of `storage.objects` a caller may write only with a matching upload ticket.
pub const MANAGED_COLUMNS: [&str; 4] = ["bucket_id", "version_id", "size", "etag"];

/// Which version of [`storage_schema_sql`] this worker installs. Bump it (and
/// [`STORAGE_SCHEMA_STAMP`]) whenever the script changes what it installs, so reconcile
/// reinstalls. A schema stamped with a higher version is a newer worker's: this one leaves it
/// alone (mixed versions during a deploy never downgrade it).
pub const STORAGE_SCHEMA_VERSION: u32 = 6;

/// The schema's comment: `chelabase storage schema <STORAGE_SCHEMA_VERSION>`.
pub const STORAGE_SCHEMA_STAMP: &str = "chelabase storage schema 6";

/// The stamp's form, as a POSIX regex whose one group is the version ([`stamp_version`]).
pub const STAMP_PATTERN: &str = "^chelabase storage schema ([0-9]{1,9})$";

/// The error the install raises, before changing anything, when the schema is a newer
/// worker's.
pub const NEWER_SCHEMA_MARK: &str = "chelabase_storage_newer";

/// The SQLSTATE `protect_objects` raises when an object's managed columns match no unexpired
/// ticket of this timeline (none, a different value, already used, expired, or another
/// branch's). Not `CB001`/`CB002`: those are the data API's (stale schema version, response
/// too large) and rest's pipeline acts on them.
pub const TICKET_MISSING: &str = "CB003";

/// The SQLSTATE `guard_tickets` raises when anyone but [`STORAGE_ROLE`] writes a ticket.
pub const TICKET_FORGED: &str = "CB004";

/// `chelabase_storage`'s connection limit. Each storage service process holds at most a
/// branch's `storage_max_connections` (1 to 20) to it; during a rolling deploy two replicas
/// can each reach 20, so the limit is twice the setting's top.
pub const STORAGE_CONNECTION_LIMIT: u32 = 40;

/// The longest a ticket may live: an upload's final insert follows its body within seconds.
pub const TICKET_MAX_LIFETIME: &str = "15 minutes";

/// The script's first line (the fakes' needle).
pub const STORAGE_SCHEMA_MARK: &str = "-- chelabase:storage-schema";

/// The error the script raises, before changing anything, when `storage` belongs to someone
/// else.
pub const FOREIGN_SCHEMA_MARK: &str = "chelabase_storage_foreign";

/// The install script, for `admin_sql` (the superuser) in one database; `owner_role` is the
/// database owner (the policy helpers' only grantee), `verifier` the SCRAM verifier of the
/// branch's `chelabase_storage` password, `parent_verifier` its parent branch's (a fresh child
/// still carries it: its first provisioning, not drift).
///
/// Plain operators inside the function bodies and table definitions resolve against the
/// pinned `search_path` (`pg_catalog` first) when they're created or run; the `DO` blocks
/// qualify theirs, like the data API script.
pub fn storage_schema_sql(owner_role: &str, verifier: &str, parent_verifier: Option<&str>) -> String {
    let owner = quote_ident(owner_role);
    let verifier_lit = crate::data_api::quote_literal(verifier);
    let foreign_objects = foreign_objects_sql();
    let acl_entries = acl_entries_sql().replace("{owner}", &owner_role.replace('\'', "''"));
    let owner_lit = crate::data_api::quote_literal(owner_role);
    let newer = newer_than_mine_sql();
    let drift = role_drift_sql("role", verifier);
    let inherited = parent_verifier.map_or_else(
        || "false".to_owned(),
        |parent| {
            format!(
                "NOT has_members AND NOT EXISTS (SELECT FROM pg_catalog.pg_auth_members a WHERE a.member OPERATOR(pg_catalog.=) role) AND NOT {}",
                role_drift_sql("role", parent)
            )
        },
    );
    let (defaults, constraints) = (
        expected_list(&EXPECTED_DEFAULTS),
        expected_list(&EXPECTED_CONSTRAINTS),
    );
    let (default_entry, constraint_entry) = (DEFAULT_ENTRY, CONSTRAINT_ENTRY);
    format!(
        r#"{STORAGE_SCHEMA_MARK}
BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});
DO $$ DECLARE
  role pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_storage');
  members pg_catalog.oid[];
  has_members boolean;
  tampered boolean;
  inherited boolean;
  ended boolean;
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
  has_members := EXISTS (SELECT FROM pg_catalog.pg_auth_members a WHERE a.roleid OPERATOR(pg_catalog.=) role);
  inherited := {inherited};
  tampered := {drift} AND NOT inherited;
  ended := has_members OR tampered OR EXISTS (SELECT FROM pg_catalog.pg_auth_members a WHERE a.member OPERATOR(pg_catalog.=) role);
  ALTER ROLE chelabase_storage LOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION BYPASSRLS NOINHERIT CONNECTION LIMIT {STORAGE_CONNECTION_LIMIT} VALID UNTIL 'infinity' PASSWORD {verifier_lit};
  FOR m IN SELECT d.datname FROM pg_catalog.pg_db_role_setting s JOIN pg_catalog.pg_database d ON d.oid OPERATOR(pg_catalog.=) s.setdatabase
    WHERE s.setrole OPERATOR(pg_catalog.=) role
  LOOP
    EXECUTE pg_catalog.format('ALTER ROLE chelabase_storage IN DATABASE %I RESET ALL', m.datname);
  END LOOP;
  ALTER ROLE chelabase_storage RESET ALL;
  ALTER ROLE chelabase_storage SET search_path = pg_catalog, pg_temp;
  ALTER ROLE chelabase_storage SET event_triggers = off;
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
  PERFORM pg_catalog.set_config('chelabase.storage_end_sessions', ended::pg_catalog.text, false);
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE ((a.usesysid OPERATOR(pg_catalog.=) role AND ended) OR a.usesysid OPERATOR(pg_catalog.=) ANY (members)) AND NOT r.rolsuper
      AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
  IF (has_members OR tampered) AND EXISTS (SELECT FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
      JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner
      WHERE n.nspname OPERATOR(pg_catalog.=) 'storage' AND c.relname OPERATOR(pg_catalog.=) 'upload_tickets' AND r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN
    DELETE FROM storage.upload_tickets;
    IF pg_catalog.to_regclass('storage.upload_sessions') IS NOT NULL THEN
      DELETE FROM storage.upload_sessions;
    END IF;
    RAISE WARNING 'chelabase: chelabase_storage had been changed; it was locked down and its upload tickets voided';
  END IF;
END $$;
COMMIT;
DO $$ BEGIN
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE ((r.rolname OPERATOR(pg_catalog.=) 'chelabase_storage' AND pg_catalog.current_setting('chelabase.storage_end_sessions', true) OPERATOR(pg_catalog.=) 'true') OR a.usesysid OPERATOR(pg_catalog.=) ANY
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
  IF EXISTS (SELECT FROM pg_catalog.pg_namespace n WHERE n.nspname OPERATOR(pg_catalog.=) 'storage'
      AND {newer}) THEN
    RAISE EXCEPTION '{NEWER_SCHEMA_MARK}: the storage schema is version %, newer than this worker''s {STORAGE_SCHEMA_VERSION}; it was left alone',
      (SELECT pg_catalog.substring(pg_catalog.obj_description(n.oid, 'pg_namespace'), '{STAMP_PATTERN}') FROM pg_catalog.pg_namespace n
        WHERE n.nspname OPERATOR(pg_catalog.=) 'storage');
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
  caller_role name NOT NULL,
  owner uuid,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS storage.upload_sessions (
  id uuid PRIMARY KEY,
  bucket_id uuid NOT NULL,
  name text NOT NULL,
  version_id uuid NOT NULL,
  upload_id text NOT NULL,
  content_type text NOT NULL,
  upsert boolean NOT NULL,
  datoid bigint NOT NULL,
  caller_role name NOT NULL,
  owner uuid,
  parts jsonb NOT NULL,
  timeline_id text NOT NULL,
  expires_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE storage.upload_sessions OWNER TO chelabase_admin;
ALTER TABLE storage.upload_tickets ADD COLUMN IF NOT EXISTS caller_role name NOT NULL DEFAULT '', ADD COLUMN IF NOT EXISTS owner uuid;
DELETE FROM storage.upload_tickets WHERE caller_role OPERATOR(pg_catalog.=) '';
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
DO $$ DECLARE d record; k record; BEGIN
  FOR d IN SELECT c.oid::pg_catalog.regclass AS rel, a.attname AS att FROM pg_catalog.pg_attrdef x
      JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) x.adrelid
      JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) x.adrelid AND a.attnum OPERATOR(pg_catalog.=) x.adnum
    WHERE c.relnamespace OPERATOR(pg_catalog.=) pg_catalog.to_regnamespace('storage')
      AND {default_entry} OPERATOR(pg_catalog.<>) ALL ({defaults})
  LOOP
    EXECUTE pg_catalog.format('ALTER TABLE %s ALTER COLUMN %I DROP DEFAULT', d.rel, d.att);
  END LOOP;
  FOR k IN SELECT c.oid::pg_catalog.regclass AS rel, x.conname FROM pg_catalog.pg_constraint x
      JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) x.conrelid
    WHERE c.relnamespace OPERATOR(pg_catalog.=) pg_catalog.to_regnamespace('storage')
      AND {constraint_entry} OPERATOR(pg_catalog.<>) ALL ({constraints})
  LOOP
    EXECUTE pg_catalog.format('ALTER TABLE %s DROP CONSTRAINT %I CASCADE', k.rel, k.conname);
  END LOOP;
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_constraint WHERE conrelid OPERATOR(pg_catalog.=) 'storage.objects'::pg_catalog.regclass
      AND conname OPERATOR(pg_catalog.=) 'objects_bucket_id_fkey') THEN
    ALTER TABLE storage.objects ADD CONSTRAINT objects_bucket_id_fkey FOREIGN KEY (bucket_id) REFERENCES storage.buckets (id);
  END IF;
END $$;
ALTER TABLE storage.buckets ALTER COLUMN id SET DEFAULT gen_random_uuid();
ALTER TABLE storage.buckets ALTER COLUMN public SET DEFAULT false;
ALTER TABLE storage.buckets ALTER COLUMN created_at SET DEFAULT now();
ALTER TABLE storage.objects ALTER COLUMN id SET DEFAULT gen_random_uuid();
ALTER TABLE storage.objects ALTER COLUMN metadata SET DEFAULT '{{}}';
ALTER TABLE storage.objects ALTER COLUMN created_at SET DEFAULT now();
ALTER TABLE storage.objects ALTER COLUMN updated_at SET DEFAULT now();
ALTER TABLE storage.upload_tickets ALTER COLUMN created_at SET DEFAULT now();
ALTER TABLE storage.upload_sessions ALTER COLUMN created_at SET DEFAULT now();
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
ALTER TABLE storage.upload_sessions ENABLE TRIGGER ALL;
ALTER TABLE storage.upload_tickets NO FORCE ROW LEVEL SECURITY;
ALTER TABLE storage.upload_tickets DISABLE ROW LEVEL SECURITY;
ALTER TABLE storage.upload_sessions NO FORCE ROW LEVEL SECURITY;
ALTER TABLE storage.upload_sessions DISABLE ROW LEVEL SECURITY;
REVOKE ALL ON storage.buckets, storage.objects, storage.upload_tickets, storage.upload_sessions FROM PUBLIC, anon, authenticated, authenticator, chelabase_storage, {owner};
GRANT SELECT ON storage.buckets, storage.objects TO anon, authenticated;
GRANT INSERT (id, name, public, file_size_limit, allowed_mime_types), UPDATE (name, public, file_size_limit, allowed_mime_types), DELETE ON storage.buckets TO authenticated;
GRANT INSERT (id, bucket_id, name, owner, metadata, size, content_type, etag, version_id), UPDATE (bucket_id, name, owner, metadata, size, content_type, etag, version_id), DELETE ON storage.objects TO authenticated;
GRANT SELECT, INSERT, UPDATE, DELETE ON storage.buckets, storage.objects TO chelabase_storage;
GRANT SELECT, INSERT, DELETE ON storage.upload_tickets TO chelabase_storage;
GRANT SELECT, INSERT, UPDATE, DELETE ON storage.upload_sessions TO chelabase_storage;
CREATE OR REPLACE FUNCTION storage.guard_tickets() RETURNS trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f${GUARD_TICKETS_BODY}$f$;
ALTER FUNCTION storage.guard_tickets() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION storage.guard_tickets() FROM PUBLIC;
CREATE OR REPLACE TRIGGER guard_tickets BEFORE INSERT OR UPDATE ON storage.upload_tickets
  FOR EACH ROW EXECUTE FUNCTION storage.guard_tickets();
ALTER TABLE storage.upload_tickets ENABLE ALWAYS TRIGGER guard_tickets;
CREATE OR REPLACE FUNCTION storage.guard_sessions() RETURNS trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f${GUARD_SESSIONS_BODY}$f$;
ALTER FUNCTION storage.guard_sessions() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION storage.guard_sessions() FROM PUBLIC;
CREATE OR REPLACE TRIGGER guard_sessions BEFORE INSERT OR UPDATE ON storage.upload_sessions
  FOR EACH ROW EXECUTE FUNCTION storage.guard_sessions();
ALTER TABLE storage.upload_sessions ENABLE ALWAYS TRIGGER guard_sessions;
CREATE OR REPLACE FUNCTION storage.record_part(p_session uuid, p_part integer, p_size bigint, p_etag text) RETURNS boolean
  LANGUAGE sql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f${RECORD_PART_BODY}$f$;
ALTER FUNCTION storage.record_part(uuid, integer, bigint, text) OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION storage.record_part(uuid, integer, bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION storage.record_part(uuid, integer, bigint, text) TO chelabase_storage;
CREATE OR REPLACE FUNCTION storage.consume_ticket(p_version uuid, p_bucket uuid, p_name text, p_size bigint, p_etag text, p_owner uuid, p_caller name)
  RETURNS boolean LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
$f${CONSUME_TICKET_BODY}$f$;
ALTER FUNCTION storage.consume_ticket(uuid, uuid, text, bigint, text, uuid, name) OWNER TO chelabase_admin;
REVOKE ALL ON FUNCTION storage.consume_ticket(uuid, uuid, text, bigint, text, uuid, name) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION storage.consume_ticket(uuid, uuid, text, bigint, text, uuid, name) TO PUBLIC;
CREATE OR REPLACE FUNCTION storage.protect_objects() RETURNS trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f${PROTECT_OBJECTS_BODY}$f$;
ALTER FUNCTION storage.protect_objects() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION storage.protect_objects() FROM PUBLIC;
CREATE OR REPLACE TRIGGER protect_objects AFTER INSERT OR UPDATE ON storage.objects
  FOR EACH ROW EXECUTE FUNCTION storage.protect_objects();
ALTER TABLE storage.objects ENABLE ALWAYS TRIGGER protect_objects;
CREATE OR REPLACE FUNCTION storage.touch_objects() RETURNS trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f${TOUCH_OBJECTS_BODY}$f$;
ALTER FUNCTION storage.touch_objects() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION storage.touch_objects() FROM PUBLIC;
CREATE OR REPLACE TRIGGER touch_objects BEFORE UPDATE ON storage.objects
  FOR EACH ROW EXECUTE FUNCTION storage.touch_objects();
ALTER TABLE storage.objects ENABLE ALWAYS TRIGGER touch_objects;
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
  IF EXISTS (SELECT FROM pg_catalog.pg_event_trigger e WHERE e.evtname OPERATOR(pg_catalog.=) 'chelabase_schema_guard_ddl')
      AND NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) session_user AND rolsuper)
      AND (session_user OPERATOR(pg_catalog.<>) {owner_lit}
        OR CASE WHEN EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) '{SCHEMA_LOCKED_ROLE}')
          THEN pg_catalog.pg_has_role(session_user, '{SCHEMA_LOCKED_ROLE}', 'MEMBER') ELSE false END) THEN
    RAISE EXCEPTION 'branch is protected: schema changes need Admin' USING ERRCODE = '42501', HINT = '{BRANCH_PROTECTED_HINT}';
  END IF;
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
  IF EXISTS (SELECT FROM pg_catalog.pg_event_trigger e WHERE e.evtname OPERATOR(pg_catalog.=) 'chelabase_schema_guard_ddl')
      AND NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) session_user AND rolsuper)
      AND (session_user OPERATOR(pg_catalog.<>) {owner_lit}
        OR CASE WHEN EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) '{SCHEMA_LOCKED_ROLE}')
          THEN pg_catalog.pg_has_role(session_user, '{SCHEMA_LOCKED_ROLE}', 'MEMBER') ELSE false END) THEN
    RAISE EXCEPTION 'branch is protected: schema changes need Admin' USING ERRCODE = '42501', HINT = '{BRANCH_PROTECTED_HINT}';
  END IF;
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
/// on every pass); and `chelabase_storage` as the lockdown leaves it, with exactly `verifier`
/// as its password. `None` (the branch has no storage login yet) is never healthy.
pub fn storage_check_sql(owner_role: &str, verifier: Option<&str>) -> String {
    let Some(verifier) = verifier else {
        return " AND false".to_owned();
    };
    format!(
        " AND EXISTS (SELECT FROM pg_catalog.pg_namespace n \
       JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner \
       WHERE n.nspname OPERATOR(pg_catalog.=) 'storage' \
         AND (r.rolname OPERATOR(pg_catalog.<>) 'chelabase_admin' \
           OR COALESCE({newer}, false) \
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
               AND 'event_triggers=off' OPERATOR(pg_catalog.=) ANY (p.proconfig)) OPERATOR(pg_catalog.=) 2 \
             AND (SELECT pg_catalog.count(*) FROM pg_catalog.pg_proc p WHERE p.pronamespace OPERATOR(pg_catalog.=) n.oid \
               AND ((p.proname OPERATOR(pg_catalog.=) 'protect_objects' AND NOT p.prosecdef AND p.prosrc OPERATOR(pg_catalog.=) $b${PROTECT_OBJECTS_BODY}$b$) \
                 OR (p.proname OPERATOR(pg_catalog.=) 'guard_tickets' AND NOT p.prosecdef AND p.prosrc OPERATOR(pg_catalog.=) $b${GUARD_TICKETS_BODY}$b$) \
                 OR (p.proname OPERATOR(pg_catalog.=) 'touch_objects' AND NOT p.prosecdef AND p.prosrc OPERATOR(pg_catalog.=) $b${TOUCH_OBJECTS_BODY}$b$) \
                 OR (p.proname OPERATOR(pg_catalog.=) 'guard_sessions' AND NOT p.prosecdef AND p.prosrc OPERATOR(pg_catalog.=) $b${GUARD_SESSIONS_BODY}$b$) \
                 OR (p.proname OPERATOR(pg_catalog.=) 'record_part' AND NOT p.prosecdef AND p.prosrc OPERATOR(pg_catalog.=) $b${RECORD_PART_BODY}$b$) \
                 OR (p.proname OPERATOR(pg_catalog.=) 'consume_ticket' AND p.prosecdef AND p.proowner OPERATOR(pg_catalog.=) r.oid \
                   AND p.prosrc OPERATOR(pg_catalog.=) $b${CONSUME_TICKET_BODY}$b$))) OPERATOR(pg_catalog.=) 6 \
             AND (SELECT pg_catalog.count(*) FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) t.tgrelid \
               WHERE c.relnamespace OPERATOR(pg_catalog.=) n.oid AND NOT t.tgisinternal) OPERATOR(pg_catalog.=) 4 \
             AND (SELECT pg_catalog.array_agg(v ORDER BY v) FROM (SELECT {default_entry} AS v FROM pg_catalog.pg_attrdef x \
               JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) x.adrelid \
               JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) x.adrelid AND a.attnum OPERATOR(pg_catalog.=) x.adnum \
               WHERE c.relnamespace OPERATOR(pg_catalog.=) n.oid) d) OPERATOR(pg_catalog.=) {defaults} \
             AND (SELECT pg_catalog.array_agg(v ORDER BY v) FROM (SELECT {constraint_entry} AS v FROM pg_catalog.pg_constraint x \
               JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) x.conrelid WHERE c.relnamespace OPERATOR(pg_catalog.=) n.oid) k) \
               OPERATOR(pg_catalog.=) {constraints}))) \
     AND NOT EXISTS (SELECT FROM pg_catalog.pg_auth_members a WHERE a.roleid OPERATOR(pg_catalog.=) {storage_role} OR a.member OPERATOR(pg_catalog.=) {storage_role}) \
     AND NOT {drift} \
     AND EXISTS (SELECT FROM pg_catalog.pg_db_role_setting s WHERE s.setrole OPERATOR(pg_catalog.=) {storage_role} AND s.setdatabase OPERATOR(pg_catalog.=) 0::pg_catalog.oid)",
        objects = foreign_objects_sql(),
        acl = foreign_acl_sql(owner_role),
        newer = newer_than_mine_sql(),
        default_entry = DEFAULT_ENTRY,
        constraint_entry = CONSTRAINT_ENTRY,
        defaults = expected_list(&EXPECTED_DEFAULTS),
        constraints = expected_list(&EXPECTED_CONSTRAINTS),
        storage_role = STORAGE_ROLE_OID,
        drift = role_drift_sql(STORAGE_ROLE_OID, verifier),
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
'storage.create_policy(name,name,text,name[],text,text,boolean)', 'storage.drop_policy(name,name)', 'storage.protect_objects()', 'storage.guard_tickets()', 'storage.touch_objects()', 'storage.guard_sessions()', 'storage.record_part(uuid,integer,bigint,text)', \
'storage.consume_ticket(uuid,uuid,text,bigint,text,uuid,name)']) \
UNION ALL SELECT pg_catalog.format('DROP %s IF EXISTS %s CASCADE', CASE c.relkind WHEN 'i' THEN 'INDEX' WHEN 'I' THEN 'INDEX' WHEN 'S' THEN 'SEQUENCE' \
    WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'c' THEN 'TYPE' WHEN 'f' THEN 'FOREIGN TABLE' ELSE 'TABLE' END, c.oid::pg_catalog.regclass) \
  FROM pg_catalog.pg_class c WHERE c.relnamespace OPERATOR(pg_catalog.=) {ns} \
    AND NOT ((c.relkind OPERATOR(pg_catalog.=) 'r' AND c.relname OPERATOR(pg_catalog.=) ANY (ARRAY['buckets', 'objects', 'upload_tickets', 'upload_sessions']::pg_catalog.name[])) \
      OR (c.relkind OPERATOR(pg_catalog.=) 'i' AND c.relname OPERATOR(pg_catalog.=) ANY (ARRAY['buckets_pkey', 'buckets_name_key', 'objects_pkey', 'objects_bucket_id_name_key', 'upload_tickets_pkey', 'upload_sessions_pkey']::pg_catalog.name[])) \
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
      OR (t.tgname OPERATOR(pg_catalog.=) 'touch_objects' AND c.relname OPERATOR(pg_catalog.=) 'objects') \
      OR (t.tgname OPERATOR(pg_catalog.=) 'guard_tickets' AND c.relname OPERATOR(pg_catalog.=) 'upload_tickets') \
      OR (t.tgname OPERATOR(pg_catalog.=) 'guard_sessions' AND c.relname OPERATOR(pg_catalog.=) 'upload_sessions')) \
UNION ALL SELECT pg_catalog.format('DROP RULE IF EXISTS %I ON %s CASCADE', w.rulename, w.ev_class::pg_catalog.regclass) FROM pg_catalog.pg_rewrite w \
  JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) w.ev_class WHERE c.relnamespace OPERATOR(pg_catalog.=) {ns} AND c.relkind OPERATOR(pg_catalog.=) 'r' \
UNION ALL SELECT pg_catalog.format('DROP POLICY IF EXISTS %I ON storage.upload_tickets', y.polname) FROM pg_catalog.pg_policy y \
  WHERE y.polrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass('storage.upload_tickets') \
    OR y.polrelid OPERATOR(pg_catalog.=) pg_catalog.to_regclass('storage.upload_sessions')"
    )
}

/// Every ACL entry on `storage` and the objects in it, but the owners' own: `(target,
/// grantor, grantee, is_grantable, allowed)`, `target` as `REVOKE ALL <target> FROM …` takes
/// it, `allowed` the grantees the install grants there.
fn acl_entries_sql() -> String {
    let ns = "pg_catalog.to_regnamespace('storage')";
    let tables = "CASE WHEN c.relname OPERATOR(pg_catalog.=) ANY (ARRAY['upload_tickets', 'upload_sessions']::pg_catalog.name[]) THEN ARRAY['chelabase_storage'] ELSE ARRAY['anon', 'authenticated', 'chelabase_storage'] END";
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
        WHEN p.proname OPERATOR(pg_catalog.=) 'record_part' THEN ARRAY['chelabase_storage'] \
        WHEN p.proname OPERATOR(pg_catalog.=) ANY (ARRAY['protect_objects', 'guard_tickets', 'touch_objects', 'guard_sessions']::pg_catalog.name[]) THEN ARRAY[]::pg_catalog.text[] ELSE ARRAY['public'] END \
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

/// `guard_tickets`'s body (its `prosrc`, which [`storage_check_sql`] compares).
pub const GUARD_TICKETS_BODY: &str = "
BEGIN
  IF current_user <> 'chelabase_storage' THEN
    RAISE EXCEPTION USING ERRCODE = 'CB004', MESSAGE = 'storage.upload_tickets is written by Chelabase storage only';
  END IF;
  -- The timeline it was minted on: a branch copies the row, never the timeline.
  NEW.timeline_id := pg_catalog.current_setting('neon.timeline_id', true);
  IF NEW.timeline_id IS NULL OR NEW.timeline_id = '' THEN
    RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'storage.upload_tickets: this server has no Neon timeline';
  END IF;
  IF NEW.expires_at <= clock_timestamp() OR NEW.expires_at > clock_timestamp() + interval '15 minutes' THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.upload_tickets: a ticket expires within 15 minutes';
  END IF;
  RETURN NEW;
END
";

/// `protect_objects`'s body: it runs as the inserting role, so `current_user` is the role
/// the ticket must have been minted for.
pub const PROTECT_OBJECTS_BODY: &str = "
BEGIN
  IF TG_OP = 'UPDATE' AND NEW.bucket_id IS NOT DISTINCT FROM OLD.bucket_id AND NEW.version_id IS NOT DISTINCT FROM OLD.version_id
      AND NEW.size IS NOT DISTINCT FROM OLD.size AND NEW.etag IS NOT DISTINCT FROM OLD.etag THEN
    RETURN NULL;
  END IF;
  IF NOT storage.consume_ticket(NEW.version_id, NEW.bucket_id, NEW.name, NEW.size, NEW.etag, NEW.owner, current_user) THEN
    RAISE EXCEPTION USING ERRCODE = 'CB003', MESSAGE = 'storage.objects: the row matches no unused upload ticket for this role and owner';
  END IF;
  RETURN NULL;
END
";

/// `guard_sessions`'s body (m7).
/// Only [`STORAGE_ROLE`] writes a session; an insert is stamped with this timeline (a branch
/// copies the row, never the timeline) and lives at most 24 hours; an update changes only the
/// parts, and the expiry only to come sooner.
pub const GUARD_SESSIONS_BODY: &str = "
BEGIN
  IF current_user <> 'chelabase_storage' THEN
    RAISE EXCEPTION USING ERRCODE = 'CB004', MESSAGE = 'storage.upload_sessions is written by Chelabase storage only';
  END IF;
  IF TG_OP = 'UPDATE' THEN
    IF (NEW.id, NEW.bucket_id, NEW.name, NEW.version_id, NEW.upload_id, NEW.content_type, NEW.upsert, NEW.datoid, NEW.caller_role, NEW.owner, NEW.timeline_id, NEW.created_at)
      IS DISTINCT FROM (OLD.id, OLD.bucket_id, OLD.name, OLD.version_id, OLD.upload_id, OLD.content_type, OLD.upsert, OLD.datoid, OLD.caller_role, OLD.owner, OLD.timeline_id, OLD.created_at)
      OR NEW.expires_at > OLD.expires_at THEN
      RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.upload_sessions: only the parts change, and the expiry only comes sooner';
    END IF;
    RETURN NEW;
  END IF;
  NEW.timeline_id := pg_catalog.current_setting('neon.timeline_id', true);
  IF NEW.timeline_id IS NULL OR NEW.timeline_id = '' THEN
    RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'storage.upload_sessions: this server has no Neon timeline';
  END IF;
  IF NEW.expires_at <= clock_timestamp() OR NEW.expires_at > clock_timestamp() + interval '24 hours' THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.upload_sessions: a session expires within 24 hours';
  END IF;
  RETURN NEW;
END
";

/// `record_part`'s body (m7): merges one part into a live session's `parts`.
pub const RECORD_PART_BODY: &str = "
UPDATE storage.upload_sessions SET parts = parts || jsonb_build_object(p_part::text, jsonb_build_object('size', p_size, 'etag', p_etag))
  WHERE id = p_session AND expires_at > clock_timestamp()
  RETURNING true
";

/// `touch_objects`'s body: every update of an object (an upsert's overwrite included) stamps
/// `updated_at`, which callers can't write themselves.
pub const TOUCH_OBJECTS_BODY: &str = "
BEGIN
  NEW.updated_at := pg_catalog.clock_timestamp();
  RETURN NEW;
END
";

/// `consume_ticket`'s body (`SECURITY DEFINER` as `chelabase_admin`, the tickets' owner):
/// deletes the one matching unexpired ticket of this timeline. Outside a trigger it does
/// nothing, so a direct call can't be used to probe or drop tickets.
pub const CONSUME_TICKET_BODY: &str = "
DECLARE
  consumed uuid;
BEGIN
  IF pg_catalog.pg_trigger_depth() = 0 THEN
    RETURN false;
  END IF;
  DELETE FROM storage.upload_tickets t
    WHERE t.version_id = p_version AND t.bucket_id = p_bucket AND t.name = p_name AND t.size = p_size AND t.etag = p_etag
      AND t.owner IS NOT DISTINCT FROM p_owner AND t.caller_role = p_caller AND t.expires_at > clock_timestamp()
      AND t.timeline_id = pg_catalog.current_setting('neon.timeline_id', true)
    RETURNING t.version_id INTO consumed;
  RETURN consumed IS NOT NULL;
END
";

/// `chelabase_storage`'s oid, as the check reads it.
const STORAGE_ROLE_OID: &str = "(SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_storage')";

/// The install's column defaults, as `table.column=pg_get_expr`.
pub const EXPECTED_DEFAULTS: [&str; 9] = [
    "buckets.created_at=now()",
    "buckets.id=gen_random_uuid()",
    "buckets.public=false",
    "objects.created_at=now()",
    "objects.id=gen_random_uuid()",
    "objects.metadata='{}'::jsonb",
    "objects.updated_at=now()",
    "upload_sessions.created_at=now()",
    "upload_tickets.created_at=now()",
];

/// The install's constraints, as `table.constraint`.
pub const EXPECTED_CONSTRAINTS: [&str; 8] = [
    "buckets.buckets_file_size_limit_check",
    "buckets.buckets_name_key",
    "buckets.buckets_pkey",
    "objects.objects_bucket_id_fkey",
    "objects.objects_bucket_id_name_key",
    "objects.objects_pkey",
    "upload_sessions.upload_sessions_pkey",
    "upload_tickets.upload_tickets_pkey",
];

/// A default as [`EXPECTED_DEFAULTS`] spells it (`c`: its table, `a`: its column, `x`: the
/// `pg_attrdef` row).
const DEFAULT_ENTRY: &str = "c.relname::pg_catalog.text OPERATOR(pg_catalog.||) '.' OPERATOR(pg_catalog.||) a.attname::pg_catalog.text OPERATOR(pg_catalog.||) '=' OPERATOR(pg_catalog.||) pg_catalog.pg_get_expr(x.adbin, x.adrelid)";

/// A constraint as [`EXPECTED_CONSTRAINTS`] spells it (`c`: its table, `x`: the row).
const CONSTRAINT_ENTRY: &str = "c.relname::pg_catalog.text OPERATOR(pg_catalog.||) '.' OPERATOR(pg_catalog.||) x.conname::pg_catalog.text";

/// `items` as a `text[]` literal.
pub fn expected_list(items: &[&str]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|i| format!("'{}'", i.replace('\'', "''")))
        .collect();
    format!("ARRAY[{}]::pg_catalog.text[]", quoted.join(", "))
}

/// Whether the role `role` (an oid expression) has drifted from what the lockdown leaves:
/// any attribute (it logs in, never expires), a password other than exactly `verifier`, a
/// connection limit other than [`STORAGE_CONNECTION_LIMIT`], or settings other than exactly
/// `search_path` and `event_triggers = off`.
pub fn role_drift_sql(role: &str, verifier: &str) -> String {
    let verifier = crate::data_api::quote_literal(verifier);
    format!(
        "(EXISTS (SELECT FROM pg_catalog.pg_authid a WHERE a.oid OPERATOR(pg_catalog.=) {role}
    AND (NOT a.rolcanlogin OR a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolreplication OR NOT a.rolbypassrls OR a.rolinherit
      OR a.rolpassword IS DISTINCT FROM {verifier} OR a.rolconnlimit OPERATOR(pg_catalog.<>) {STORAGE_CONNECTION_LIMIT}
      OR a.rolvaliduntil IS DISTINCT FROM 'infinity'::pg_catalog.timestamptz))
    OR EXISTS (SELECT FROM pg_catalog.pg_db_role_setting s WHERE s.setrole OPERATOR(pg_catalog.=) {role}
      AND (s.setdatabase OPERATOR(pg_catalog.<>) 0::pg_catalog.oid OR s.setconfig OPERATOR(pg_catalog.<>) ARRAY['search_path=pg_catalog, pg_temp', 'event_triggers=off']::pg_catalog.text[])))"
    )
}

/// Whether the stamp of schema `n` names a version above [`STORAGE_SCHEMA_VERSION`] (NULL
/// when it doesn't parse: an unparseable stamp is installed over).
fn newer_than_mine_sql() -> String {
    format!(
        "pg_catalog.substring(pg_catalog.obj_description(n.oid, 'pg_namespace'), '{STAMP_PATTERN}')::pg_catalog.int4 OPERATOR(pg_catalog.>) {STORAGE_SCHEMA_VERSION}"
    )
}

/// Whether the install stopped because the schema is a newer worker's: the warning to log
/// (the error's text after [`NEWER_SCHEMA_MARK`], naming both versions).
pub fn newer_schema(error: &str) -> Option<&str> {
    let marker = format!("ERROR:  {NEWER_SCHEMA_MARK}: ");
    let start = error.find(&marker)? + marker.len();
    Some(error[start..].lines().next().unwrap_or("").trim_end())
}

/// The version a stamp names, as [`STAMP_PATTERN`] reads it in SQL: `None` for anything but
/// exactly `chelabase storage schema <1 to 9 digits>`.
pub fn stamp_version(stamp: &str) -> Option<u32> {
    let digits = stamp.strip_prefix("chelabase storage schema ")?;
    (!digits.is_empty() && digits.len() <= 9 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
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

    /// A branch's `chelabase_storage` verifier, as the bootstrap computes it.
    const VERIFIER: &str = "SCRAM-SHA-256$4096:c2FsdHNhbHRzYWx0c2FsdA==$c3RvcmVk:c2VydmVy";

    fn sql() -> String {
        storage_schema_sql("postgres", VERIFIER, None)
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
            // m5: the storage service's login, with exactly the branch's verifier.
            &format!("ALTER ROLE chelabase_storage LOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION BYPASSRLS NOINHERIT CONNECTION LIMIT {STORAGE_CONNECTION_LIMIT} VALID UNTIL 'infinity' PASSWORD '{VERIFIER}';"),
            "EXECUTE pg_catalog.format('ALTER ROLE chelabase_storage IN DATABASE %I RESET ALL', m.datname);",
            "ALTER ROLE chelabase_storage RESET ALL;",
            "ALTER ROLE chelabase_storage SET search_path = pg_catalog, pg_temp;",
            // No event trigger fires for its sessions (a login trigger included).
            "ALTER ROLE chelabase_storage SET event_triggers = off;",
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
            "IF (has_members OR tampered) AND EXISTS (SELECT FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace\n      JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner\n      WHERE n.nspname OPERATOR(pg_catalog.=) 'storage' AND c.relname OPERATOR(pg_catalog.=) 'upload_tickets' AND r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN\n    DELETE FROM storage.upload_tickets;",
        ] {
            assert!(lockdown.contains(part), "{part}: {lockdown}");
        }
        // Tampered: any attribute or setting beyond the ones it is given here.
        let tampered = between(lockdown, "tampered :=", ";\n");
        for attribute in [
            "NOT a.rolcanlogin",
            "rolsuper",
            "rolcreaterole",
            "rolcreatedb",
            "rolreplication",
            "NOT a.rolbypassrls",
            // Any other password, a changed one included, is drift.
            &format!("a.rolpassword IS DISTINCT FROM '{VERIFIER}'"),
            &format!("rolconnlimit OPERATOR(pg_catalog.<>) {STORAGE_CONNECTION_LIMIT}"),
            "a.rolvaliduntil IS DISTINCT FROM 'infinity'::pg_catalog.timestamptz",
            "pg_db_role_setting",
            "ARRAY['search_path=pg_catalog, pg_temp', 'event_triggers=off']",
        ] {
            assert!(tampered.contains(attribute), "{attribute}: {tampered}");
        }
        // Only a role that could become it (a member) voids tickets, not its own memberships.
        let joined = between(lockdown, "has_members :=", ";\n");
        assert!(
            joined.contains("a.roleid OPERATOR(pg_catalog.=) role)")
                && !joined.contains("a.member"),
            "{joined}"
        );
        // The sessions are ended again once the revokes are committed.
        let after = sql.split_once("COMMIT;\n").unwrap().1;
        assert!(
            after.contains("pg_catalog.current_setting('chelabase.storage_former_members', true)")
                && after.contains("r.rolname OPERATOR(pg_catalog.=) 'chelabase_storage'"),
            "{after}"
        );
        // m5: its own sessions (the storage service's) are ended only when it had drifted or
        // had members either way; its former members' always.
        assert!(lockdown.contains("ended := has_members OR tampered OR EXISTS (SELECT FROM pg_catalog.pg_auth_members a WHERE a.member OPERATOR(pg_catalog.=) role);"), "{lockdown}");
        assert!(lockdown.contains("WHERE ((a.usesysid OPERATOR(pg_catalog.=) role AND ended) OR a.usesysid OPERATOR(pg_catalog.=) ANY (members))"), "{lockdown}");
        assert!(lockdown.contains("PERFORM pg_catalog.set_config('chelabase.storage_end_sessions', ended::pg_catalog.text, false);"), "{lockdown}");
        assert!(
            after.contains("(r.rolname OPERATOR(pg_catalog.=) 'chelabase_storage' AND pg_catalog.current_setting('chelabase.storage_end_sessions', true) OPERATOR(pg_catalog.=) 'true')"),
            "{after}"
        );
        // Two replicas of the storage service, each at the top of the setting's range.
        assert_eq!(STORAGE_CONNECTION_LIMIT, 40);
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
        let reset = "REVOKE ALL ON storage.buckets, storage.objects, storage.upload_tickets, storage.upload_sessions FROM PUBLIC, anon, authenticated, authenticator, chelabase_storage, \"postgres\";";
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
                "GRANT SELECT, INSERT, UPDATE, DELETE ON storage.upload_sessions TO chelabase_storage;",
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
            // Bound to the request it was minted for (re-audit S1).
            "caller_role name NOT NULL",
            "owner uuid",
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
        // It runs as the inserting role, so `current_user` is the caller's own role.
        assert!(
            body.contains(
                "LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp"
            ),
            "{body}"
        );
        assert!(body.contains("IF TG_OP = 'UPDATE' AND NEW.bucket_id IS NOT DISTINCT FROM OLD.bucket_id AND NEW.version_id IS NOT DISTINCT FROM OLD.version_id\n      AND NEW.size IS NOT DISTINCT FROM OLD.size AND NEW.etag IS NOT DISTINCT FROM OLD.etag THEN\n    RETURN NULL;"), "{body}");
        assert!(body.contains(&format!("IF NOT storage.consume_ticket(NEW.version_id, NEW.bucket_id, NEW.name, NEW.size, NEW.etag, NEW.owner, current_user) THEN\n    RAISE EXCEPTION USING ERRCODE = '{TICKET_MISSING}'")), "{body}");
        let consume = between(
            &sql,
            "CREATE OR REPLACE FUNCTION storage.consume_ticket(",
            "$f$;",
        );
        assert!(
            consume.contains(
                "LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp"
            ),
            "{consume}"
        );
        assert!(
            consume.contains("IF pg_catalog.pg_trigger_depth() = 0 THEN\n    RETURN false;"),
            "only from a trigger: {consume}"
        );
        let delete = between(consume, "DELETE FROM storage.upload_tickets t", ";");
        for part in [
            "t.version_id = p_version",
            "t.bucket_id = p_bucket",
            "t.name = p_name",
            "t.size = p_size",
            "t.etag = p_etag",
            "t.owner IS NOT DISTINCT FROM p_owner",
            "t.caller_role = p_caller",
            "t.expires_at > clock_timestamp()",
            "t.timeline_id = pg_catalog.current_setting('neon.timeline_id', true)",
            "RETURNING t.version_id INTO consumed",
        ] {
            assert!(delete.contains(part), "{part}: {delete}");
        }
        assert!(sql.contains("CREATE OR REPLACE TRIGGER protect_objects AFTER INSERT OR UPDATE ON storage.objects\n  FOR EACH ROW EXECUTE FUNCTION storage.protect_objects();"), "{sql}");
        assert!(
            sql.contains("ALTER TABLE storage.objects ENABLE ALWAYS TRIGGER protect_objects;"),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER FUNCTION storage.protect_objects() OWNER TO CURRENT_USER;"),
            "{sql}"
        );
        assert!(sql.contains("ALTER FUNCTION storage.consume_ticket(uuid, uuid, text, bigint, text, uuid, name) OWNER TO chelabase_admin;"), "{sql}");
        assert!(sql.contains("GRANT EXECUTE ON FUNCTION storage.consume_ticket(uuid, uuid, text, bigint, text, uuid, name) TO PUBLIC;"), "{sql}");
    }

    /// The triggers' codes are their own: never RLS's 42501, never the data API's `CB001`
    /// (stale schema version: rest retries) or `CB002` (response too large).
    #[test]
    fn storage_schema_ticket_codes_are_dedicated() {
        assert_eq!((TICKET_MISSING, TICKET_FORGED), ("CB003", "CB004"));
        let data_api = crate::data_api::data_api_sql("postgres", "pw", "public");
        // A forged write is refused with the same code on the tickets and the sessions (m7).
        for (code, uses) in [(TICKET_MISSING, 1), (TICKET_FORGED, 2)] {
            assert_ne!(code, "42501");
            assert!(
                !data_api.contains(&format!("'{code}'")),
                "{code} is the data API's"
            );
            assert_eq!(
                sql().matches(&format!("ERRCODE = '{code}'")).count(),
                uses,
                "{code}"
            );
        }
        // The ticket triggers never raise RLS's code (only the helpers' branch guard does,
        // as the guard itself does).
        for body in [
            PROTECT_OBJECTS_BODY,
            GUARD_TICKETS_BODY,
            CONSUME_TICKET_BODY,
            GUARD_SESSIONS_BODY,
        ] {
            assert!(!body.contains("42501"), "{body}");
        }
        assert_eq!(
            sql().matches("ERRCODE = '42501'").count(),
            2,
            "the two helpers' guard only"
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

    /// Re-audit S4: with event triggers off, the helpers apply the protected-branch guard
    /// themselves: while the guard is installed only an unmarked owner session (or a
    /// superuser) may change policies, so an owner wrapper called by a locked login is refused.
    #[test]
    fn storage_schema_policy_helpers_apply_the_branch_guard() {
        use chelabase_types::table_api::{BRANCH_PROTECTED_HINT, SCHEMA_LOCKED_ROLE};
        let sql = sql();
        for function in ["storage.create_policy(", "storage.drop_policy("] {
            let body = between(
                &sql,
                &format!("CREATE OR REPLACE FUNCTION {function}"),
                "$f$;",
            );
            let guard = between(
                body,
                "IF EXISTS (SELECT FROM pg_catalog.pg_event_trigger e WHERE e.evtname OPERATOR(pg_catalog.=) 'chelabase_schema_guard_ddl')",
                "END IF;",
            );
            assert!(guard.contains("NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) session_user AND rolsuper)"), "{guard}");
            assert!(
                guard.contains("session_user OPERATOR(pg_catalog.<>) 'postgres'"),
                "{guard}"
            );
            assert!(
                guard.contains(&format!(
                    "pg_catalog.pg_has_role(session_user, '{SCHEMA_LOCKED_ROLE}', 'MEMBER')"
                )),
                "{guard}"
            );
            assert!(
                guard.contains(&format!(
                    "ERRCODE = '42501', HINT = '{BRANCH_PROTECTED_HINT}'"
                )),
                "{guard}"
            );
            assert!(
                body.find(guard).unwrap() < body.find("EXECUTE ").unwrap(),
                "the guard comes before the DDL: {body}"
            );
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
        // Re-audit S3: foreign defaults and constraints go, the install's come back.
        assert!(install.contains("EXECUTE pg_catalog.format('ALTER TABLE %s ALTER COLUMN %I DROP DEFAULT', d.rel, d.att);"), "{install}");
        assert!(install.contains("EXECUTE pg_catalog.format('ALTER TABLE %s DROP CONSTRAINT %I CASCADE', k.rel, k.conname);"), "{install}");
        assert!(
            install.contains("ALTER TABLE storage.objects ALTER COLUMN metadata SET DEFAULT '{}';"),
            "{install}"
        );
        assert!(install.contains("    ALTER TABLE storage.objects ADD CONSTRAINT objects_bucket_id_fkey FOREIGN KEY (bucket_id) REFERENCES storage.buckets (id);"), "{install}");
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
            "'storage.consume_ticket(uuid,uuid,text,bigint,text,uuid,name)'",
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

    /// The stamp is the version number; a stamp parses only in exactly that form.
    #[test]
    fn storage_schema_stamp_versions_parse() {
        assert_eq!(
            STORAGE_SCHEMA_STAMP,
            format!("chelabase storage schema {STORAGE_SCHEMA_VERSION}")
        );
        assert_eq!(
            stamp_version(STORAGE_SCHEMA_STAMP),
            Some(STORAGE_SCHEMA_VERSION)
        );
        assert_eq!(stamp_version("chelabase storage schema 3"), Some(3));
        assert_eq!(stamp_version("chelabase storage schema 99"), Some(99));
        for garbage in [
            "",
            "chelabase storage schema",
            "chelabase storage schema x",
            "chelabase storage schema 4 ",
            "chelabase storage schema -1",
            "chelabase storage schema 1234567890",
            "other 4",
        ] {
            assert_eq!(stamp_version(garbage), None, "{garbage:?}");
        }
    }

    /// Mixed versions: a stamp newer than this worker's leaves the install alone (the role's
    /// lockdown, which commits first, still runs) and is healthy for this worker's check; an
    /// older, equal or unparseable stamp is installed over as before.
    #[test]
    fn storage_schema_leaves_a_newer_schema_alone() {
        let sql = sql();
        let newer = format!(
            "pg_catalog.substring(pg_catalog.obj_description(n.oid, 'pg_namespace'), '{STAMP_PATTERN}')::pg_catalog.int4 OPERATOR(pg_catalog.>) {STORAGE_SCHEMA_VERSION}"
        );
        let install = sql.split_once("COMMIT;\n").unwrap().1;
        let check_at = install
            .find(&newer)
            .unwrap_or_else(|| panic!("{newer}: {install}"));
        assert!(
            check_at < install.find("CREATE SCHEMA").unwrap(),
            "before any change: {install}"
        );
        assert!(install.contains(&format!("RAISE EXCEPTION '{NEWER_SCHEMA_MARK}: the storage schema is version %, newer than this worker''s {STORAGE_SCHEMA_VERSION}; it was left alone'")), "{install}");
        let check = storage_check_sql("postgres", Some(VERIFIER));
        assert!(
            check.contains(&format!("OR COALESCE({newer}, false)")),
            "{check}"
        );
        // The role's checks still apply to a newer schema.
        assert!(
            check.rfind("pg_auth_members").unwrap() > check.find(&newer).unwrap(),
            "{check}"
        );
        let error = format!(
            "psql in compute ep-1 exited with 3: ERROR:  {NEWER_SCHEMA_MARK}: the storage schema is version 99, newer than this worker's 4; it was left alone"
        );
        assert_eq!(
            newer_schema(&error),
            Some("the storage schema is version 99, newer than this worker's 4; it was left alone")
        );
        assert_eq!(
            newer_schema("psql in compute ep-1 exited with 3: ERROR:  permission denied"),
            None
        );
        assert!(!foreign_schema(&error));
    }

    /// A newer schema doesn't fail the bootstrap.
    #[tokio::test]
    async fn storage_schema_newer_schema_does_not_fail_the_bootstrap() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.computes.fail_in_with("app", STORAGE_SCHEMA_MARK, &format!("psql in compute ep exited with 3: ERROR:  {NEWER_SCHEMA_MARK}: the storage schema is version 99, newer than this worker's 4; it was left alone"));
        w.bootstrap_data_api(&main, "app").await.unwrap();
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
            sql.find("ALTER ROLE chelabase_storage LOGIN").unwrap()
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
        let check = schema_check_sql("public", Some(VERIFIER));
        let storage = storage_check_sql("postgres", Some(VERIFIER));
        assert!(check.ends_with(&storage), "{check}");
        // A branch with no stored storage login yet is never healthy: its bootstrap makes one.
        assert!(storage_check_sql("postgres", None).ends_with(" AND false"));
        assert!(!storage_check_sql("postgres", None).contains("SCRAM"));
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
            // Re-audit S2: the role's drift or any membership, either way.
            "a.roleid OPERATOR(pg_catalog.=) (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_storage') OR a.member OPERATOR(pg_catalog.=) (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_storage')",
            &role_drift_sql(
                "(SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_storage')",
                VERIFIER,
            ),
            // m5: a changed password fails the check (and so reruns the lockdown, which sets
            // the verifier back).
            &format!("a.rolpassword IS DISTINCT FROM '{VERIFIER}'"),
            // Re-audit S3: the triggers' functions as installed, and only the install's
            // defaults and constraints.
            &format!("$b${PROTECT_OBJECTS_BODY}$b$"),
            &format!("$b${GUARD_TICKETS_BODY}$b$"),
            &format!("$b${CONSUME_TICKET_BODY}$b$"),
            &expected_list(&EXPECTED_DEFAULTS),
            &expected_list(&EXPECTED_CONSTRAINTS),
        ] {
            assert!(storage.contains(part), "{part}: {storage}");
        }
        assert!(
            sql().contains(&format!("$f${PROTECT_OBJECTS_BODY}$f$"))
                && sql().contains(&format!("$f${CONSUME_TICKET_BODY}$f$"))
        );
        assert_eq!(EXPECTED_DEFAULTS.len(), 9);
        assert!(EXPECTED_CONSTRAINTS.contains(&"objects.objects_bucket_id_fkey"));
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
        let verifier = w.storage_verifier(main.id).await.unwrap().expect("a stored login");
        assert_eq!(scripts, [storage_schema_sql("postgres", &verifier, None)]);

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

    /// m5: the bootstrap gives `chelabase_storage` a password of its own per branch: made once
    /// (sealed `Purpose::Storage` for the branch, with a random 16-byte SCRAM salt stored
    /// beside it), set as the verifier of that salt, and kept on every later run; the health
    /// check holds the role to exactly that verifier.
    #[tokio::test]
    async fn storage_login_is_made_once_and_set_as_its_verifier() {
        use chelabase_secrets::{Purpose, open_credential};
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let stored = w.metadata.state().storage_logins.get(&main.id).cloned();
        let (sealed, salt) = stored.expect("the bootstrap stored a storage login");
        assert_eq!(salt.len(), 16);
        let password = open_credential(&w.key, &main.id.to_string(), Purpose::Storage, &sealed)
            .expect("sealed for the branch as storage's");
        assert_eq!(password.len(), 32);
        let verifier = chelabase_neon::scram::verifier_with_salt(&password, &salt, 4096);
        assert_eq!(w.storage_verifier(main.id).await.unwrap().as_deref(), Some(verifier.as_str()));
        let script = |w: &crate::fakes::FakeWorker| -> Vec<String> {
            w.computes
                .sql
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, _, s)| s.starts_with(STORAGE_SCHEMA_MARK))
                .map(|(_, _, s)| s.clone())
                .collect()
        };
        assert!(script(&w)[0].contains(&format!("PASSWORD '{verifier}';")), "{:?}", script(&w));
        // A second run keeps the login: the same sealed password and salt, the same verifier.
        w.computes.sql.lock().unwrap().clear();
        w.bootstrap_data_api(&main, "app").await.unwrap();
        assert_eq!(w.metadata.state().storage_logins.get(&main.id), Some(&(sealed, salt)));
        assert!(script(&w)[0].contains(&format!("PASSWORD '{verifier}';")));
        // The health check asks for exactly that verifier.
        w.computes.sql.lock().unwrap().clear();
        let _ = w.data_api_healthy(main.id, main.endpoint_id.as_deref().unwrap()).await;
        let checks: Vec<String> = w.computes.sql.lock().unwrap().iter().map(|(_, _, s)| s.clone()).collect();
        assert!(
            checks.iter().any(|s| s.contains(&format!("a.rolpassword IS DISTINCT FROM '{verifier}'"))),
            "{checks:?}"
        );
    }

    /// A racing bootstrap that stored a storage login first wins: both use its password.
    #[tokio::test]
    async fn storage_login_yields_to_a_racer() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let first = w.metadata.state().storage_logins.get(&main.id).cloned().unwrap();
        w.metadata.state().storage_logins.remove(&main.id);
        let racer = (chelabase_secrets::seal_credential(&w.key, &main.id.to_string(), chelabase_secrets::Purpose::Storage, "r".repeat(32).as_str()), vec![9u8; 16]);
        let raced = racer.clone();
        w.metadata.state().before_set_storage_password = Some(Box::new(move |s: &mut crate::fakes::FakeState| {
            s.storage_logins.insert(main.id, raced.clone());
        }));
        let main = w.metadata.main(id);
        w.bootstrap_data_api(&main, "app").await.unwrap();
        assert_ne!(first, racer);
        assert_eq!(w.metadata.state().storage_logins.get(&main.id), Some(&racer));
        let verifier = chelabase_neon::scram::verifier_with_salt(&"r".repeat(32), &[9u8; 16], 4096);
        assert_eq!(w.storage_verifier(main.id).await.unwrap(), Some(verifier));
    }


    /// A fresh child's `chelabase_storage` still carries its parent's verifier: its first
    /// provisioning, never drift (no warning, its tickets kept), but only when the role is
    /// otherwise exactly as the lockdown leaves it and nothing is a member either way.
    #[test]
    fn storage_schema_a_childs_inherited_login_is_no_drift() {
        const PARENT: &str = "SCRAM-SHA-256$4096:cGFyZW50cGFyZW50cGFyZQ==$cA==:cA==";
        let sql = storage_schema_sql("postgres", VERIFIER, Some(PARENT));
        let lockdown = between(&sql, "DO $$ DECLARE\n  role pg_catalog.oid", "END $$;");
        let inherited = format!(
            "inherited := NOT has_members AND NOT EXISTS (SELECT FROM pg_catalog.pg_auth_members a WHERE a.member OPERATOR(pg_catalog.=) role) AND NOT {};",
            role_drift_sql("role", PARENT)
        );
        assert!(lockdown.contains(&inherited), "{lockdown}");
        let tampered = between(lockdown, "tampered :=", ";\n");
        assert!(tampered.trim_end().ends_with("AND NOT inherited;"), "{tampered:?}");
        assert!(lockdown.find("inherited :=").unwrap() < lockdown.find("tampered :=").unwrap());
        // Without a parent (main) nothing is inherited.
        let main = storage_schema_sql("postgres", VERIFIER, None);
        assert!(main.contains("inherited := false;"), "{main}");
    }

    /// A child's bootstrap passes its parent's stored verifier; the child gets its own login.
    #[tokio::test]
    async fn storage_login_of_a_child_names_its_parents_verifier() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let parent = w.storage_verifier(main.id).await.unwrap().unwrap();
        let mut child = main.clone();
        child.id = BranchId::new();
        child.parent_branch_id = Some(main.id);
        child.authenticator_password_enc = None;
        w.metadata.state().branches.insert(child.id, child.clone());
        w.computes.sql.lock().unwrap().clear();
        w.bootstrap_data_api(&child, "app").await.unwrap();
        let own = w.storage_verifier(child.id).await.unwrap().expect("the child's own login");
        assert_ne!(own, parent);
        let scripts: Vec<String> = w.computes.sql.lock().unwrap().iter()
            .filter(|(_, _, s)| s.starts_with(STORAGE_SCHEMA_MARK)).map(|(_, _, s)| s.clone()).collect();
        assert_eq!(scripts, [storage_schema_sql("postgres", &own, Some(&parent))]);
    }


    /// m5 phase B: every update of an object (an upsert's overwrite included) advances its
    /// `updated_at`, through a BEFORE UPDATE trigger installed, owned, checked and healed like
    /// the others.
    #[test]
    fn storage_schema_objects_updated_at_advances() {
        let sql = sql();
        for part in [
            "CREATE OR REPLACE FUNCTION storage.touch_objects() RETURNS trigger\n  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS",
            &format!("$f${TOUCH_OBJECTS_BODY}$f$"),
            "ALTER FUNCTION storage.touch_objects() OWNER TO CURRENT_USER;",
            "REVOKE ALL ON FUNCTION storage.touch_objects() FROM PUBLIC;",
            "CREATE OR REPLACE TRIGGER touch_objects BEFORE UPDATE ON storage.objects\n  FOR EACH ROW EXECUTE FUNCTION storage.touch_objects();",
            "ALTER TABLE storage.objects ENABLE ALWAYS TRIGGER touch_objects;",
        ] {
            assert!(sql.contains(part), "{part}");
        }
        assert!(TOUCH_OBJECTS_BODY.contains("NEW.updated_at := pg_catalog.clock_timestamp();"));
        let check = storage_check_sql("postgres", Some(VERIFIER));
        assert!(check.contains(&format!("(p.proname OPERATOR(pg_catalog.=) 'touch_objects' AND NOT p.prosecdef AND p.prosrc OPERATOR(pg_catalog.=) $b${TOUCH_OBJECTS_BODY}$b$)")), "{check}");
        let foreign = foreign_objects_sql();
        assert!(foreign.contains("'storage.touch_objects()'"), "{foreign}");
        assert!(foreign.contains("(t.tgname OPERATOR(pg_catalog.=) 'touch_objects' AND c.relname OPERATOR(pg_catalog.=) 'objects')"), "{foreign}");
        assert!(acl_entries_sql().contains("ARRAY['protect_objects', 'guard_tickets', 'touch_objects'"));
    }

    /// m7: resumable upload sessions, version 6. `chelabase_admin`'s table, written by the
    /// storage role only (`guard_sessions`, as the tickets' guard: `pg_write_all_data` writes
    /// nothing), stamped with the timeline it was opened on, at most 24 hours long; an update
    /// changes only its parts and may only bring its expiry closer. Checked, healed and voided
    /// with the tickets when the role was tampered with.
    #[test]
    fn storage_schema_upload_sessions_are_storages_alone() {
        assert_eq!((STORAGE_SCHEMA_VERSION, STORAGE_SCHEMA_STAMP), (6, "chelabase storage schema 6"));
        let sql = sql();
        let table = between(&sql, "CREATE TABLE IF NOT EXISTS storage.upload_sessions (", ");");
        for column in [
            "id uuid PRIMARY KEY",
            "bucket_id uuid NOT NULL",
            "name text NOT NULL",
            "version_id uuid NOT NULL",
            "upload_id text NOT NULL",
            "content_type text NOT NULL",
            "upsert boolean NOT NULL",
            "datoid bigint NOT NULL",
            "caller_role name NOT NULL",
            "owner uuid",
            "parts jsonb NOT NULL",
            "timeline_id text NOT NULL",
            "expires_at timestamptz NOT NULL",
            "created_at timestamptz NOT NULL DEFAULT now()",
        ] {
            assert!(table.contains(column), "{column}: {table}");
        }
        for part in [
            "ALTER TABLE storage.upload_sessions OWNER TO chelabase_admin;",
            "ALTER TABLE storage.upload_sessions DISABLE ROW LEVEL SECURITY;",
            "ALTER TABLE storage.upload_sessions ENABLE TRIGGER ALL;",
            "GRANT SELECT, INSERT, UPDATE, DELETE ON storage.upload_sessions TO chelabase_storage;",
            "CREATE OR REPLACE TRIGGER guard_sessions BEFORE INSERT OR UPDATE ON storage.upload_sessions\n  FOR EACH ROW EXECUTE FUNCTION storage.guard_sessions();",
            "ALTER TABLE storage.upload_sessions ENABLE ALWAYS TRIGGER guard_sessions;",
            "ALTER FUNCTION storage.guard_sessions() OWNER TO CURRENT_USER;",
            "REVOKE ALL ON FUNCTION storage.guard_sessions() FROM PUBLIC;",
            &format!("$f${GUARD_SESSIONS_BODY}$f$"),
        ] {
            assert!(sql.contains(part), "{part}");
        }
        assert!(sql.contains("REVOKE ALL ON storage.buckets, storage.objects, storage.upload_tickets, storage.upload_sessions FROM PUBLIC"), "{sql}");
        for part in [
            &format!("IF current_user <> 'chelabase_storage' THEN\n    RAISE EXCEPTION USING ERRCODE = '{TICKET_FORGED}'"),
            "NEW.timeline_id := pg_catalog.current_setting('neon.timeline_id', true);",
            "NEW.expires_at > clock_timestamp() + interval '24 hours'",
            "NEW.expires_at > OLD.expires_at",
            "(NEW.id, NEW.bucket_id, NEW.name, NEW.version_id, NEW.upload_id, NEW.content_type, NEW.upsert, NEW.datoid, NEW.caller_role, NEW.owner, NEW.timeline_id, NEW.created_at)\n      IS DISTINCT FROM (OLD.id, OLD.bucket_id, OLD.name, OLD.version_id, OLD.upload_id, OLD.content_type, OLD.upsert, OLD.datoid, OLD.caller_role, OLD.owner, OLD.timeline_id, OLD.created_at)",
        ] {
            assert!(GUARD_SESSIONS_BODY.contains(part), "{part}: {GUARD_SESSIONS_BODY}");
        }
        // Voided with the tickets when the role was tampered with (the table may not exist yet).
        let lockdown = between(&sql, "DO $$ DECLARE\n  role pg_catalog.oid", "END $$;");
        assert!(lockdown.contains("IF pg_catalog.to_regclass('storage.upload_sessions') IS NOT NULL THEN\n      DELETE FROM storage.upload_sessions;"), "{lockdown}");
        // Checked and healed like the tickets.
        let check = storage_check_sql("postgres", Some(VERIFIER));
        assert!(check.contains(&format!("(p.proname OPERATOR(pg_catalog.=) 'guard_sessions' AND NOT p.prosecdef AND p.prosrc OPERATOR(pg_catalog.=) $b${GUARD_SESSIONS_BODY}$b$)")), "{check}");
        assert!(check.contains("AND NOT t.tgisinternal) OPERATOR(pg_catalog.=) 4"), "four triggers: {check}");
        let foreign = foreign_objects_sql();
        for part in [
            "'storage.guard_sessions()'",
            "'upload_sessions'",
            "'upload_sessions_pkey'",
            "(t.tgname OPERATOR(pg_catalog.=) 'guard_sessions' AND c.relname OPERATOR(pg_catalog.=) 'upload_sessions')",
            "pg_catalog.to_regclass('storage.upload_sessions')",
        ] {
            assert!(foreign.contains(part), "{part}: {foreign}");
        }
        let acl = acl_entries_sql();
        assert!(acl.contains("c.relname OPERATOR(pg_catalog.=) ANY (ARRAY['upload_tickets', 'upload_sessions']::pg_catalog.name[]) THEN ARRAY['chelabase_storage']"), "{acl}");
        assert!(acl.contains("ARRAY['protect_objects', 'guard_tickets', 'touch_objects', 'guard_sessions']"), "{acl}");
        assert!(EXPECTED_DEFAULTS.contains(&"upload_sessions.created_at=now()"));
        assert!(EXPECTED_CONSTRAINTS.contains(&"upload_sessions.upload_sessions_pkey"));
    }

    /// m7: a part is recorded through `storage.record_part` (the storage service's statements
    /// never say `SET`): `SECURITY INVOKER`, so the session's guard sees the storage role, and
    /// executable by it alone; checked and healed like the others.
    #[test]
    fn storage_schema_parts_are_recorded_by_the_storage_role() {
        let sql = sql();
        for part in [
            "CREATE OR REPLACE FUNCTION storage.record_part(p_session uuid, p_part integer, p_size bigint, p_etag text) RETURNS boolean\n  LANGUAGE sql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS",
            &format!("$f${RECORD_PART_BODY}$f$"),
            "ALTER FUNCTION storage.record_part(uuid, integer, bigint, text) OWNER TO CURRENT_USER;",
            "REVOKE ALL ON FUNCTION storage.record_part(uuid, integer, bigint, text) FROM PUBLIC;",
            "GRANT EXECUTE ON FUNCTION storage.record_part(uuid, integer, bigint, text) TO chelabase_storage;",
        ] {
            assert!(sql.contains(part), "{part}");
        }
        for part in [
            "UPDATE storage.upload_sessions SET parts = parts || jsonb_build_object(p_part::text, jsonb_build_object('size', p_size, 'etag', p_etag))",
            "WHERE id = p_session AND expires_at > clock_timestamp()",
            "RETURNING true",
        ] {
            assert!(RECORD_PART_BODY.contains(part), "{part}: {RECORD_PART_BODY}");
        }
        let check = storage_check_sql("postgres", Some(VERIFIER));
        assert!(check.contains(&format!("(p.proname OPERATOR(pg_catalog.=) 'record_part' AND NOT p.prosecdef AND p.prosrc OPERATOR(pg_catalog.=) $b${RECORD_PART_BODY}$b$)")), "{check}");
        assert!(check.contains("$b$))) OPERATOR(pg_catalog.=) 6"), "six checked functions: {check}");
        assert!(foreign_objects_sql().contains("'storage.record_part(uuid,integer,bigint,text)'"));
        assert!(acl_entries_sql().contains("WHEN p.proname OPERATOR(pg_catalog.=) 'record_part' THEN ARRAY['chelabase_storage']"));
    }

}
