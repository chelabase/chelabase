//! The data API's bootstrap inside a branch's database (the `chelabase` machinery schema, the
//! `authenticator` login, default privileges and the branch's public key), and the check a
//! secret key's role must pass.

use anyhow::{Context, bail};
use chelabase_metadata::{
    BranchDatabase, BranchKind, BranchRecord, BranchStatus, DataApiState, DatabaseOrigin,
    MetadataError, NewDataKey, ServedDatabase,
};
use chelabase_nats::{CheckDataRoleReply, CheckDataRoleRequest, DataApiDatabaseRequest};
use chelabase_neon::ComputeBackend;
use chelabase_secrets::{Purpose, open_credential, seal_credential};
use chelabase_types::BranchId;
use chelabase_types::data_key::{self, DataKeyKind};
use tracing::{info, warn};
use uuid::Uuid;

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::{OWNER_ROLE, Worker, credentials};

/// The login role the data API connects as; never a secret key's role.
const AUTHENTICATOR: &str = "authenticator";
/// Whether a branch's data API machinery is intact (psql prints `t` or `f`): the `chelabase`
/// schema, `authenticator` able to log in with `USAGE` on it and `SELECT` on
/// `schema_version`, the three event triggers present and enabled, and the guard functions
/// `assert_schema_version(uuid)` and `cap_response(text,bigint)`. Anything else is
/// what the bootstrap repairs. Operators are schema-qualified (it runs as the superuser in
/// a database the owner controls). Reconcile runs it through [`schema_check_sql`], which
/// adds the database's default schema.
pub const SCHEMA_CHECK_SQL: &str = "SELECT to_regclass('chelabase.schema_version') IS NOT NULL \
     AND EXISTS (SELECT FROM pg_catalog.pg_roles r \
       JOIN pg_catalog.pg_namespace n ON n.nspname OPERATOR(pg_catalog.=) 'chelabase' \
       JOIN pg_catalog.pg_class c ON c.relnamespace OPERATOR(pg_catalog.=) n.oid \
         AND c.relname OPERATOR(pg_catalog.=) 'schema_version' \
       WHERE r.rolname OPERATOR(pg_catalog.=) 'authenticator' AND r.rolcanlogin \
         AND pg_catalog.has_schema_privilege(r.oid, n.oid, 'USAGE') \
         AND pg_catalog.has_table_privilege(r.oid, c.oid, 'SELECT')) \
     AND (SELECT pg_catalog.count(*) FROM pg_catalog.pg_event_trigger \
       WHERE evtname OPERATOR(pg_catalog.=) ANY (ARRAY['chelabase_schema_version_ddl', \
         'chelabase_schema_version_drop', 'chelabase_extension_functions']::pg_catalog.name[]) \
         AND evtenabled OPERATOR(pg_catalog.<>) 'D') OPERATOR(pg_catalog.=) 3 \
     AND pg_catalog.to_regprocedure('chelabase.assert_schema_version(uuid)') IS NOT NULL \
     AND pg_catalog.to_regprocedure('chelabase.cap_response(text,bigint)') IS NOT NULL";

/// [`SCHEMA_CHECK_SQL`] for a database whose default schema is `schema`: also
/// `authenticator`'s `USAGE` on it. The data API prepares its statements as `authenticator`
/// (NOINHERIT) before it switches role, and Postgres checks `USAGE` on every schema a
/// statement names while parsing it, so without it every pipelined request falls back to
/// the sequential path. A default schema that doesn't exist passes: the bootstrap skips it
/// too. The literal is the quoted identifier (`to_regnamespace` parses identifier syntax).
/// The search path is pinned first (`cloud_admin` is a superuser; `to_regclass` is unqualified).
/// `storage_verifier`: the branch's `chelabase_storage` verifier ([`crate::storage_schema::storage_check_sql`]).
pub fn schema_check_sql(schema: &str, storage_verifier: Option<&str>) -> String {
    let schema_literal = quote_literal(&quote_ident(schema));
    let storage = crate::storage_schema::storage_check_sql(crate::OWNER_ROLE, storage_verifier);
    format!(
        "SET search_path = pg_catalog, pg_temp; {SCHEMA_CHECK_SQL} \
     AND NOT EXISTS (SELECT FROM pg_catalog.pg_namespace n \
       JOIN pg_catalog.pg_roles r ON r.rolname OPERATOR(pg_catalog.=) 'authenticator' \
       WHERE n.oid OPERATOR(pg_catalog.=) pg_catalog.to_regnamespace({schema_literal})::pg_catalog.oid \
         AND NOT pg_catalog.has_schema_privilege(r.oid, n.oid, 'USAGE')){storage}"
    )
}

/// The key of the transaction-scoped advisory lock the bootstrap takes first, so two
/// bootstraps of one database serialize (advisory locks are per database). The bytes of
/// "chelabase" followed by 0x01 (the bootstrap); other Chelabase advisory locks must use
/// another key.
pub const BOOTSTRAP_LOCK_KEY: i64 = 0x6368_656c_6164_6201;

/// How long either script waits for a lock (the advisory lock above, or a table lock such as
/// `ALTER TABLE … OWNER` on `chelabase.schema_version`) before it fails, in seconds. It sits
/// under the 30 s `ADMIN_SQL_TIMEOUT` of `admin_sql`: when the caller gives up, dropping its
/// future doesn't kill psql, so a script still queued behind a stuck lock would run later with
/// stale content (an old `authenticator` password). Failing first, with the transaction
/// rolled back, leaves 10 s of the budget for the script's own work and never leaves an
/// AccessExclusive lock waiting or held for long. `lock_timeout` bounds every lock wait,
/// advisory locks included (they go through the same lock manager as table locks).
/// [`REMOVE_DATA_API_SQL`] is a `const` and can't format this in, so its text repeats the
/// value; a test keeps the two in step.
pub const LOCK_TIMEOUT_SECS: u32 = 20;

/// Removes the data API machinery from a database (`DELETE …/data-api?remove=true`, P7), only
/// when the `chelabase` schema is the one the bootstrap made (owned by `chelabase_admin`): the
/// three event triggers, the two functions and the two tables by name, then the schema with
/// `RESTRICT`, so anything else someone put there makes it fail rather than go with it. Before
/// its own functions it drops what app users left (the protect event triggers, the sync
/// functions, the config table) and the claim helpers, each before the one it calls; a policy
/// still using a helper makes it fail. `chelabase.users_guard()` is left to `RESTRICT`: a guard
/// trigger still using it means app users are on, which the caller refuses first. A
/// `chelabase` schema with another owner is left alone (a warning). The `chelabase_admin` role
/// stays (it's cluster-wide), and so do the default privileges and grants. The first line is a
/// comment so the script is told apart from the bootstrap's. It takes [`BOOTSTRAP_LOCK_KEY`]
/// first (bootstrap and remove lock the same objects in opposite orders, so without it they
/// could deadlock), bounded by [`LOCK_TIMEOUT_SECS`]; the key's value is repeated here.
pub const REMOVE_DATA_API_SQL: &str = "-- remove the data API machinery
SET search_path = pg_catalog, pg_temp;
DO $$ BEGIN
  SET LOCAL lock_timeout = '20s';
  PERFORM pg_catalog.pg_advisory_xact_lock(7163086723497615873);
  IF EXISTS (SELECT FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner
      WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase' AND r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN
    DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_ddl;
    DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_drop;
    DROP EVENT TRIGGER IF EXISTS chelabase_extension_functions;
    DROP EVENT TRIGGER IF EXISTS chelabase_users_protect_drop;
    DROP EVENT TRIGGER IF EXISTS chelabase_users_protect_ddl;
    DROP FUNCTION IF EXISTS chelabase.users_protect();
    DROP FUNCTION IF EXISTS chelabase.app_users_upsert(uuid, text, boolean, timestamptz, jsonb);
    DROP FUNCTION IF EXISTS chelabase.app_users_gone(uuid, boolean);
    DROP FUNCTION IF EXISTS chelabase.app_users_rows();
    DROP TABLE IF EXISTS chelabase.app_users_table;
    DROP FUNCTION IF EXISTS chelabase.user_email();
    DROP FUNCTION IF EXISTS chelabase.user_id();
    DROP FUNCTION IF EXISTS chelabase.claims();
    DROP FUNCTION IF EXISTS chelabase.bump_schema_version();
    DROP FUNCTION IF EXISTS chelabase.assert_schema_version(uuid);
    DROP FUNCTION IF EXISTS chelabase.cap_response(text, bigint);
    DROP FUNCTION IF EXISTS chelabase.grant_extension_functions();
    DROP TABLE IF EXISTS chelabase.schema_version;
    DROP TABLE IF EXISTS chelabase.bootstrap;
    DROP SCHEMA chelabase RESTRICT;
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_namespace WHERE nspname OPERATOR(pg_catalog.=) 'chelabase') THEN
    RAISE WARNING 'chelabase: the chelabase schema is not owned by chelabase_admin; nothing was removed';
  END IF;
END $$;
";

/// Postgres' refusal to drop something another object still uses (2BP01: a policy or view on
/// a claim helper, P29), from psql's output: the `ERROR` line and its `DETAIL`, without the
/// prefixes. `None` for any other failure.
fn dependency_refusal(output: &str) -> Option<String> {
    // `run_exec` puts psql's stderr after its own prefix on the first line, so `ERROR:` may
    // start mid-line.
    let mut lines = output.lines().map(str::trim);
    let error = lines
        .by_ref()
        .find_map(|l| l.split_once("ERROR:").map(|(_, e)| e))
        .map(str::trim)
        .filter(|e| e.contains("because other objects depend on it"))?;
    match lines.next().and_then(|l| l.strip_prefix("DETAIL:")) {
        Some(detail) => Some(format!("{error} ({})", detail.trim())),
        None => Some(error.to_string()),
    }
}

/// Where a data API request applies: the branch's endpoint and the database's row.
struct DataApiTarget {
    ep: String,
    row: BranchDatabase,
}

/// What [`Worker::data_api_target`] found.
enum Lookup {
    /// The branch isn't ready or has no compute.
    NotReady,
    /// The branch has no such database.
    Unknown,
    Found(DataApiTarget),
}

/// SCRAM's iteration count for the storage login's verifier (Postgres's default).
const STORAGE_SCRAM_ITERATIONS: u32 = 4096;

/// The `chelabase_storage` verifier of `password` with its stored `salt`: computed here, so
/// the health check can require exactly it.
fn storage_verifier(password: &str, salt: &[u8]) -> String {
    chelabase_neon::scram::verifier_with_salt(password, salt, STORAGE_SCRAM_ITERATIONS)
}

/// `s` as an SQL string literal (`quote_literal`): single quotes doubled.
pub(crate) fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `s` as a quoted SQL identifier (`quote_ident`, always quoted): double quotes doubled.
pub(crate) fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `^[A-Za-z_][A-Za-z0-9_$]{0,62}$`.
pub fn valid_role_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.split_first() {
        Some((first, rest)) => {
            rest.len() <= 62
                && (first.is_ascii_alphabetic() || *first == b'_')
                && rest
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$')
        }
        None => false,
    }
}

/// The idempotent script run as cloud_admin in a branch's database after every compute
/// (re)creation and from reconcile: the `chelabase` machinery schema and its event trigger,
/// `authenticator` made `LOGIN` with the branch's password, the owner's default privileges in
/// the default schema and `USAGE` on it for `anon`, `authenticated` and `authenticator` (the
/// data API parses its statements as `authenticator`), the claim helpers ([`crate::app_users_sql::claim_helpers_body`]), and (on the
/// database's first bootstrap only) the same grants on what exists. The tamper check allows
/// what app users adds to `chelabase` (P4).
///
/// The script is one transaction (`BEGIN … COMMIT`) that first takes
/// [`BOOTSTRAP_LOCK_KEY`] (after `SET LOCAL lock_timeout`, [`LOCK_TIMEOUT_SECS`]), so a reader
/// never sees the event triggers dropped and not yet recreated, and concurrent bootstraps
/// serialize. A failure (psql runs with `ON_ERROR_STOP`; the closed connection aborts the open
/// transaction) rolls back every change in it, but not `pg_terminate_backend`: backends the
/// script already terminated stay terminated. Nothing in
/// it may be a statement Postgres refuses inside a transaction block.
///
/// The event-trigger functions are owned by the superuser running the script (Neon refuses
/// to let a superuser run a function a non-superuser owns, and the triggers fire for
/// cloud_admin's DDL too), `SECURITY DEFINER` with every name schema-qualified. The owner's
/// default privilege revokes `EXECUTE` from PUBLIC on every function it creates, including an
/// extension's; `grant_extension_functions` restores Postgres's default for the functions a
/// `CREATE`/`ALTER EXTENSION` adds (the data API never exposes them as calls).
///
/// `grant_extension_functions` runs as the invoker and grants only on functions owned by
/// `owner_role` (never a superuser's), so a superuser's `CREATE EXTENSION` keeps the
/// extension's own revokes. It's dormant on Neon today: a trusted extension's script runs
/// as the bootstrap superuser, so its functions are owned by cloud_admin and keep
/// Postgres's PUBLIC `EXECUTE`. It's kept for a Neon where owners own their extensions'
/// functions, when the owner's default privilege would strip PUBLIC from them.
pub fn data_api_sql(owner_role: &str, authenticator_password: &str, schema: &str) -> String {
    data_api_script(owner_role, authenticator_password, schema, false)
}

/// [`data_api_sql`] for a rebootstrap that was asked to grant the schema's existing objects
/// (`grant_existing`): the first-grants statements run whether or not `chelabase.bootstrap`
/// has its row (still only when the schema exists). Grants only: no `REVOKE`, so a role never
/// loses a privilege to it.
pub fn data_api_sql_granting_existing(owner_role: &str, authenticator_password: &str, schema: &str) -> String {
    data_api_script(owner_role, authenticator_password, schema, true)
}

fn data_api_script(owner_role: &str, authenticator_password: &str, schema: &str, grant_existing: bool) -> String {
    // The asked-for variant only adds grants: no REVOKE, so it never narrows what a role has.
    let (first_grants_guard, record_grant, revoke_public) = if grant_existing {
        ("", " ON CONFLICT DO NOTHING", "")
    } else {
        (
            " AND NOT EXISTS (SELECT FROM chelabase.bootstrap)",
            "",
            "      EXECUTE pg_catalog.format('REVOKE EXECUTE ON ROUTINE %s FROM PUBLIC', r.f);\n",
        )
    };
    let password = quote_literal(authenticator_password);
    let schema_ident = quote_ident(schema);
    // `regnamespace` and `to_regnamespace` parse identifier syntax, so the literal is the
    // quoted identifier.
    let schema_literal = quote_literal(&schema_ident);
    let owner = quote_literal(owner_role);
    let claim_helpers = crate::app_users_sql::claim_helpers_body();
    // A reserved default schema gets neither default privileges nor grants.
    let schema_present = if reserved_default_schema(schema) {
        "false".to_string()
    } else {
        format!("pg_catalog.to_regnamespace({schema_literal}) IS NOT NULL")
    };
    format!(
        "BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN CREATE ROLE chelabase_admin NOLOGIN; END IF;
END $$;
DO $$ DECLARE
  admin pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_admin');
  trusted pg_catalog.oid[] := ARRAY[admin, (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) CURRENT_USER)];
  ns pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname OPERATOR(pg_catalog.=) 'chelabase');
  members pg_catalog.oid[] := ARRAY(SELECT member FROM pg_catalog.pg_auth_members WHERE roleid OPERATOR(pg_catalog.=) admin);
  joined boolean := pg_catalog.cardinality(members) OPERATOR(pg_catalog.>) 0;
  tampered boolean := EXISTS (SELECT FROM pg_catalog.pg_authid a WHERE a.oid OPERATOR(pg_catalog.=) admin
    AND (a.rolcanlogin OR a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolreplication OR a.rolbypassrls
      OR a.rolpassword IS NOT NULL OR a.rolconnlimit OPERATOR(pg_catalog.<>) ALL (ARRAY[-1, 0])))
    OR EXISTS (SELECT FROM pg_catalog.pg_db_role_setting WHERE setrole OPERATOR(pg_catalog.=) admin);
  m record;
  aside pg_catalog.text;
BEGIN
  ALTER ROLE chelabase_admin NOLOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION NOBYPASSRLS NOINHERIT PASSWORD NULL CONNECTION LIMIT 0;
  FOR m IN SELECT d.datname FROM pg_catalog.pg_db_role_setting s JOIN pg_catalog.pg_database d ON d.oid OPERATOR(pg_catalog.=) s.setdatabase
    WHERE s.setrole OPERATOR(pg_catalog.=) admin
  LOOP
    EXECUTE pg_catalog.format('ALTER ROLE chelabase_admin IN DATABASE %I RESET ALL', m.datname);
  END LOOP;
  ALTER ROLE chelabase_admin RESET ALL;
  FOR m IN SELECT r.rolname AS member, g.rolname AS grantor FROM pg_catalog.pg_auth_members a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.member JOIN pg_catalog.pg_roles g ON g.oid OPERATOR(pg_catalog.=) a.grantor
    WHERE a.roleid OPERATOR(pg_catalog.=) admin
  LOOP
    EXECUTE pg_catalog.format('REVOKE chelabase_admin FROM %I GRANTED BY %I CASCADE', m.member, m.grantor);
  END LOOP;
  PERFORM pg_catalog.set_config('chelabase.former_members', pg_catalog.array_to_string(members, ','), false);
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (a.usesysid OPERATOR(pg_catalog.=) admin OR a.usesysid OPERATOR(pg_catalog.=) ANY (members)) AND NOT r.rolsuper
      AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
  IF ns IS NOT NULL AND (joined OR tampered
    OR (SELECT nspowner FROM pg_catalog.pg_namespace WHERE oid OPERATOR(pg_catalog.=) ns) OPERATOR(pg_catalog.<>) admin
    OR EXISTS (SELECT FROM pg_catalog.pg_class c WHERE c.relnamespace OPERATOR(pg_catalog.=) ns
      AND (c.relowner OPERATOR(pg_catalog.<>) ALL (trusted) OR c.reloftype OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
        OR c.relkind OPERATOR(pg_catalog.<>) ALL (ARRAY['r', 'i']::pg_catalog.\"char\"[])
        OR c.relname OPERATOR(pg_catalog.<>) ALL (ARRAY['schema_version', 'schema_version_pkey', 'bootstrap', 'bootstrap_pkey', 'app_users_table', 'app_users_table_pkey']::pg_catalog.name[])))
    OR EXISTS (SELECT FROM pg_catalog.pg_proc p WHERE p.pronamespace OPERATOR(pg_catalog.=) ns
      AND (p.proowner OPERATOR(pg_catalog.<>) ALL (trusted)
        OR p.proname OPERATOR(pg_catalog.<>) ALL (ARRAY['bump_schema_version', 'grant_extension_functions', 'claims', 'user_id', 'user_email', 'users_guard', 'users_protect', 'app_users_upsert', 'app_users_gone', 'app_users_rows', 'assert_schema_version', 'cap_response']::pg_catalog.name[])))
    OR EXISTS (SELECT FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) t.tgrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND NOT t.tgisinternal)
    OR EXISTS (SELECT FROM pg_catalog.pg_rewrite w JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) w.ev_class
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns)
    OR EXISTS (SELECT FROM pg_catalog.pg_policy y JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) y.polrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns)
    OR EXISTS (SELECT FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) a.attrelid
      JOIN pg_catalog.pg_type ty ON ty.oid OPERATOR(pg_catalog.=) a.atttypid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND a.attnum OPERATOR(pg_catalog.>) 0
        AND ty.typnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace)
    OR EXISTS (SELECT FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) d.adrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND pg_catalog.pg_get_expr(d.adbin, d.adrelid) OPERATOR(pg_catalog.<>) 'true')
    OR EXISTS (SELECT FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) k.conrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND k.contype OPERATOR(pg_catalog.<>) ALL (ARRAY['p', 'n']::pg_catalog.\"char\"[])
        AND pg_catalog.pg_get_constraintdef(k.oid) OPERATOR(pg_catalog.<>) 'CHECK (one)')
    OR EXISTS (SELECT FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) i.indrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND (i.indexprs IS NOT NULL OR i.indpred IS NOT NULL
        OR EXISTS (SELECT FROM pg_catalog.pg_opclass o WHERE o.oid OPERATOR(pg_catalog.=) ANY (i.indclass)
          AND o.opcnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace))))
  THEN
    DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_ddl;
    DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_drop;
    DROP EVENT TRIGGER IF EXISTS chelabase_extension_functions;
    aside := pg_catalog.format('chelabase_quarantine_%s', pg_catalog.floor(pg_catalog.date_part('epoch', pg_catalog.clock_timestamp()) * 1000000)::pg_catalog.int8);
    EXECUTE pg_catalog.format('ALTER SCHEMA chelabase RENAME TO %I', aside);
    PERFORM pg_catalog.set_config('chelabase.quarantined', aside, false);
  END IF;
END $$;
DO $$ BEGIN
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin' OR a.usesysid OPERATOR(pg_catalog.=) ANY
        (pg_catalog.string_to_array(pg_catalog.current_setting('chelabase.former_members', true), ',')::pg_catalog.oid[]))
      AND NOT r.rolsuper AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
END $$;
CREATE SCHEMA IF NOT EXISTS chelabase AUTHORIZATION chelabase_admin;
REVOKE ALL ON SCHEMA chelabase FROM PUBLIC;
GRANT USAGE ON SCHEMA chelabase TO authenticator;
CREATE TABLE IF NOT EXISTS chelabase.schema_version (one boolean PRIMARY KEY DEFAULT true CHECK (one), version uuid NOT NULL);
INSERT INTO chelabase.schema_version (version) VALUES (pg_catalog.gen_random_uuid()) ON CONFLICT DO NOTHING;
CREATE TABLE IF NOT EXISTS chelabase.bootstrap (one boolean PRIMARY KEY DEFAULT true CHECK (one), existing_granted boolean NOT NULL);
ALTER TABLE chelabase.schema_version OWNER TO chelabase_admin;
ALTER TABLE chelabase.bootstrap OWNER TO chelabase_admin;
REVOKE ALL ON chelabase.schema_version, chelabase.bootstrap FROM PUBLIC;
GRANT SELECT ON chelabase.schema_version TO authenticator;
GRANT USAGE ON SCHEMA chelabase TO {owner_role};
GRANT SELECT ON chelabase.schema_version TO {owner_role};
{claim_helpers}CREATE OR REPLACE FUNCTION chelabase.bump_schema_version() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
  $f$ BEGIN UPDATE chelabase.schema_version SET version = pg_catalog.gen_random_uuid(); END $f$;
ALTER FUNCTION chelabase.bump_schema_version() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION chelabase.bump_schema_version() FROM PUBLIC;
CREATE OR REPLACE FUNCTION chelabase.assert_schema_version(expected uuid) RETURNS uuid
  LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = '' AS
  $f$ DECLARE v uuid; BEGIN
    SELECT version INTO v FROM chelabase.schema_version;
    IF v IS DISTINCT FROM expected THEN
      RAISE EXCEPTION USING ERRCODE = 'CB001', MESSAGE = 'chelabase: stale schema version';
    END IF;
    RETURN v; END $f$;
CREATE OR REPLACE FUNCTION chelabase.cap_response(result text, budget bigint) RETURNS text
  LANGUAGE plpgsql VOLATILE SET search_path = '' AS
  $f$ DECLARE n bigint := coalesce(pg_catalog.octet_length(result), 0); total bigint; BEGIN
    IF n OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    total := coalesce(nullif(pg_catalog.current_setting('chelabase.response_bytes', true), '')::bigint, 0) OPERATOR(pg_catalog.+) n;
    IF total OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    PERFORM pg_catalog.set_config('chelabase.response_bytes', total::text, true);
    RETURN result; END $f$;
ALTER FUNCTION chelabase.assert_schema_version(uuid) OWNER TO CURRENT_USER;
ALTER FUNCTION chelabase.cap_response(text, bigint) OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION chelabase.assert_schema_version(uuid), chelabase.cap_response(text, bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION chelabase.assert_schema_version(uuid), chelabase.cap_response(text, bigint) TO PUBLIC;
CREATE OR REPLACE FUNCTION chelabase.grant_extension_functions() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
  $f$ DECLARE f pg_catalog.regprocedure; BEGIN
    FOR f IN SELECT d.objid::pg_catalog.regprocedure FROM pg_catalog.pg_event_trigger_ddl_commands() c
      JOIN pg_catalog.pg_depend d ON d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass AND d.refobjid OPERATOR(pg_catalog.=) c.objid
      JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) d.objid
      WHERE c.object_type OPERATOR(pg_catalog.=) 'extension' AND d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass AND d.deptype OPERATOR(pg_catalog.=) 'e'
        AND p.proowner OPERATOR(pg_catalog.=) (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) {owner} AND NOT r.rolsuper)
    LOOP
      BEGIN
        EXECUTE pg_catalog.format('GRANT EXECUTE ON ROUTINE %s TO PUBLIC', f);
      EXCEPTION WHEN OTHERS THEN
        RAISE WARNING 'chelabase: EXECUTE on % was not granted to PUBLIC: %', f, SQLERRM;
      END;
    END LOOP;
  EXCEPTION WHEN OTHERS THEN
    RAISE WARNING 'chelabase: extension functions were not granted to PUBLIC: %', SQLERRM;
  END $f$;
ALTER FUNCTION chelabase.grant_extension_functions() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION chelabase.grant_extension_functions() FROM PUBLIC;
DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_ddl;
DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_drop;
DROP EVENT TRIGGER IF EXISTS chelabase_extension_functions;
CREATE EVENT TRIGGER chelabase_schema_version_ddl ON ddl_command_end EXECUTE FUNCTION chelabase.bump_schema_version();
CREATE EVENT TRIGGER chelabase_schema_version_drop ON sql_drop EXECUTE FUNCTION chelabase.bump_schema_version();
CREATE EVENT TRIGGER chelabase_extension_functions ON ddl_command_end WHEN TAG IN ('CREATE EXTENSION', 'ALTER EXTENSION') EXECUTE FUNCTION chelabase.grant_extension_functions();
ALTER ROLE authenticator LOGIN NOINHERIT PASSWORD {password};
ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
DO $$ BEGIN
  IF {schema_present} THEN
    ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} IN SCHEMA {schema_ident} GRANT SELECT ON TABLES TO anon;
    ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} IN SCHEMA {schema_ident} GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO authenticated;
    ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} IN SCHEMA {schema_ident} GRANT USAGE, SELECT ON SEQUENCES TO authenticated;
    ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} IN SCHEMA {schema_ident} GRANT EXECUTE ON FUNCTIONS TO authenticated;
    GRANT USAGE ON SCHEMA {schema_ident} TO anon, authenticated, authenticator;
  END IF;
END $$;
DO $$ DECLARE r record; BEGIN
  IF {schema_present}{first_grants_guard} THEN
    GRANT SELECT ON ALL TABLES IN SCHEMA {schema_ident} TO anon;
    GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema_ident} TO authenticated;
    GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {schema_ident} TO authenticated;
    FOR r IN SELECT p.oid::pg_catalog.regprocedure AS f FROM pg_catalog.pg_proc p
      WHERE p.pronamespace OPERATOR(pg_catalog.=) {schema_literal}::pg_catalog.regnamespace
        AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend d WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass
          AND d.objid OPERATOR(pg_catalog.=) p.oid AND d.deptype OPERATOR(pg_catalog.=) 'e')
    LOOP
{revoke_public}      EXECUTE pg_catalog.format('GRANT EXECUTE ON ROUTINE %s TO authenticated', r.f);
    END LOOP;
    INSERT INTO chelabase.bootstrap (existing_granted) VALUES (true){record_grant};
  END IF;
END $$;
SELECT 'chelabase_missing_schema' WHERE pg_catalog.to_regnamespace({schema_literal}) IS NULL;
SELECT pg_catalog.current_setting('chelabase.quarantined', true);
COMMIT;
"
    )
}

/// Hard cap on one enable job (the NATS handler's and reconcile's).
pub const ENABLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether the script skipped the default schema's statements (the schema doesn't exist).
pub(crate) fn missing_schema(output: &str) -> bool {
    output.lines().any(|l| l.trim() == "chelabase_missing_schema")
}

/// The schema the bootstrap renamed aside, from its output (the script's last line).
pub fn quarantined(output: &str) -> Option<&str> {
    output
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .filter(|l| l.starts_with("chelabase_quarantine_"))
}

/// Roles a secret key's role must not be a member of, at any depth (besides superusers and
/// `neon_superuser`, answered on their own): they reach the compute itself or Chelabase's own
/// machinery. The shared list rest's per-request gate refuses too.
fn system_roles() -> String {
    chelabase_types::data_key::KEY_ROLE_REFUSED_GROUPS
        .iter()
        .filter(|g| **g != "neon_superuser")
        .map(|g| quote_literal(g))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One row for an existing role: `superuser`, `personal_login` (it carries the personal-login
/// marker, the round-4 predicate; MH-12), `neon_superuser` (a member, at any depth),
/// `owner_level` (CREATEROLE, CREATEDB or REPLICATION: with the two before, E33's owner-level
/// definition, as rest's `OWNER_LEVEL_SQL`), `system` (a member of a superuser role or of
/// [`system_roles`]) or `ok <oid> <t|f>` (the flag: BYPASSRLS, which a key's role may have).
/// No row when it doesn't exist. Membership goes by oid over `pg_roles`, so a system role
/// missing from the image matches nothing instead of failing.
fn role_check_sql(role: &str) -> String {
    format!(
        "SET search_path = pg_catalog, pg_temp; SELECT CASE WHEN r.rolsuper THEN 'superuser' \
         WHEN EXISTS (SELECT FROM pg_db_role_setting s WHERE s.setrole = r.oid \
         AND s.setdatabase = 0 AND EXISTS (SELECT FROM unnest(s.setconfig) AS c \
         WHERE starts_with(c, '{marker}='))) THEN 'personal_login' \
         WHEN EXISTS (SELECT FROM pg_roles s WHERE s.rolname = 'neon_superuser' \
         AND pg_has_role(r.oid, s.oid, 'MEMBER')) THEN 'neon_superuser' \
         WHEN r.rolcreaterole OR r.rolcreatedb OR r.rolreplication \
         THEN 'owner_level' \
         WHEN EXISTS (SELECT FROM pg_roles s WHERE (s.rolsuper OR s.rolname IN ({})) \
         AND pg_has_role(r.oid, s.oid, 'MEMBER')) THEN 'system' \
         ELSE 'ok ' || r.oid::text || CASE WHEN r.rolbypassrls THEN ' t' ELSE ' f' END END \
         FROM pg_roles r WHERE r.rolname = {};",
        system_roles(),
        quote_literal(role),
        marker = chelabase_types::role_name::PERSONAL_LOGIN_MARKER,
    )
}

/// What a refused owner-level role may do instead (E33).
const OWNER_LEVEL_HINT: &str = "owner-level roles can't back a secret key; use a role without \
     CREATEROLE, CREATEDB, REPLICATION or neon_superuser: BYPASSRLS is allowed";

/// The `ok` answer of [`role_check_sql`]: `(oid, bypass_rls)`; a line without the flag (an
/// older check) is `false`.
fn parse_role_ok(answer: &str) -> Option<(i64, bool)> {
    let mut fields = answer.strip_prefix("ok ")?.split(' ');
    let oid = fields.next()?.parse::<i64>().ok().filter(|oid| *oid > 0)?;
    let bypass = match fields.next() {
        None | Some("f") => false,
        Some("t") => true,
        Some(_) => return None,
    };
    fields.next().is_none().then_some((oid, bypass))
}

/// A schema the data API never serves, so never granted as a default schema (S3): object
/// storage's, Chelabase's own (`chelabase…`), Postgres's (`pg_…`, `information_schema`). The
/// orchestrator refuses them when one is set; a value stored before they were reserved is
/// skipped here.
pub fn reserved_default_schema(schema: &str) -> bool {
    schema == crate::storage_schema::STORAGE_SCHEMA
        || schema.starts_with("chelabase")
        || schema.starts_with("pg_")
        || schema == "information_schema"
}

/// Roles Chelabase owns; never a secret key's.
const RESERVED_ROLES: [&str; 5] = [
    AUTHENTICATOR,
    "chelabase_admin",
    crate::storage_schema::STORAGE_ROLE,
    crate::app_users_sql::USERS_SYNC_ROLE,
    crate::realtime::REALTIME_ROLE,
];

/// What [`Worker::check_data_role`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleCheck {
    /// Allowed, and granted to `authenticator`: the role's OID (a secret key is bound to it)
    /// and whether it has BYPASSRLS (E33: allowed, and the key is marked).
    Ok(i64, bool),
    /// The caller's fault (422): the message says why.
    Refused(String),
    /// The branch isn't ready, or its compute can't be reached (409).
    NotReady,
    /// Something broke on our side (500): logged; the message is for logs.
    Failed(String),
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// After any compute (re)creation and from reconcile. Idempotent. The record gives the
    /// branch and its endpoint; the stored password is read afresh. Bootstraps every served
    /// database; `database_name` is the main one.
    pub async fn bootstrap_data_api(
        &self,
        branch: &BranchRecord,
        database_name: &str,
    ) -> anyhow::Result<()> {
        let ep = branch
            .endpoint_id
            .as_deref()
            .with_context(|| format!("branch {} has no endpoint", branch.id))?;
        self.bootstrap_served_on(branch.id, ep, database_name, &[])
            .await
            .map(|_| ())
    }

    /// The databases the data API serves on `branch`, main first. A branch without rows (none
    /// yet) serves its main database with `public`.
    pub(crate) async fn served_or_main(
        &self,
        branch: BranchId,
        ep: &str,
    ) -> anyhow::Result<Vec<ServedDatabase>> {
        let served = self
            .metadata
            .served_databases(branch)
            .await
            .context("listing the served databases")?;
        if !served.is_empty() {
            return Ok(served);
        }
        Ok(vec![ServedDatabase {
            name: self.database_of(ep).await?,
            default_schema: "public".to_string(),
        }])
    }

    /// The bootstrap of every served database of `branch` on `ep` (`main_database` is the
    /// main one), then the secret keys' roles (cluster-wide, so through the main database
    /// only) and the public key. The main database's failure fails the call; another
    /// database's is logged (reconcile heals it), so a broken database can't keep the compute
    /// from starting. `done` lists databases (with the schema) an earlier call already
    /// bootstrapped: they're skipped, and the keys are left to that call. Returns the databases
    /// bootstrapped now.
    pub(crate) async fn bootstrap_served_on(
        &self,
        branch: BranchId,
        ep: &str,
        main_database: &str,
        done: &[ServedDatabase],
    ) -> anyhow::Result<Vec<ServedDatabase>> {
        let mut served = self.metadata.served_databases(branch).await.context("listing the served databases")?;
        if served.is_empty() {
            served.push(ServedDatabase {
                name: main_database.to_string(),
                default_schema: "public".to_string(),
            });
        }
        let mut bootstrapped = Vec::new();
        for database in served {
            if done.contains(&database) {
                continue;
            }
            let result = self
                .bootstrap_data_api_on(branch, ep, &database.name, &database.default_schema)
                .await;
            match result {
                Ok(()) => bootstrapped.push(database),
                Err(e) if database.name == main_database => return Err(e),
                Err(e) => {
                    warn!(branch_id = %branch, %ep, database = %database.name, "bootstrapping a served database; reconcile retries: {e:#}");
                }
            }
        }
        if done.is_empty() {
            self.regrant_secret_key_roles(branch, ep, main_database).await?;
            self.ensure_public_key(branch).await?;
        }
        Ok(bootstrapped)
    }

    /// The data API script in one database of `ep`, with `schema` as its default schema: the
    /// authenticator password (generated and stored first when there's none) and the script.
    /// Neither the keys nor the roles' grants (see [`Self::bootstrap_served_on`]).
    pub(crate) async fn bootstrap_data_api_on(
        &self,
        branch: BranchId,
        ep: &str,
        database_name: &str,
        schema: &str,
    ) -> anyhow::Result<()> {
        self.bootstrap_data_api_with(branch, ep, database_name, schema, false).await
    }

    /// [`Self::bootstrap_data_api_on`], granting the schema's existing objects too when
    /// `grant_existing` (the rebootstrap a caller asked for).
    async fn bootstrap_data_api_with(
        &self,
        branch: BranchId,
        ep: &str,
        database_name: &str,
        schema: &str,
        grant_existing: bool,
    ) -> anyhow::Result<()> {
        if reserved_default_schema(schema) {
            warn!(branch_id = %branch, %ep, database = %database_name, schema, "the stored default schema is reserved; the data API grants nothing on it");
        }
        let password = self.authenticator_password(branch).await?;
        let script = data_api_script(OWNER_ROLE, &password, schema, grant_existing);
        let output = self.computes.admin_sql(ep, database_name, &script).await?;
        if missing_schema(&output) {
            warn!(branch_id = %branch, %ep, database = %database_name, schema, "the default schema doesn't exist; its default privileges and grants were skipped (the machinery is installed)");
        }
        if let Some(aside) = quarantined(&output) {
            warn!(branch_id = %branch, %ep, database = %database_name, "a chelabase schema Chelabase didn't make (or one tampered with) was renamed to {aside}; a fresh one was installed");
        }
        let (storage_password, salt) = self.storage_login(branch).await?;
        let parent = self
            .metadata
            .get_branch(branch)
            .await
            .context("reading the branch")?
            .and_then(|b| b.parent_branch_id);
        let parent_verifier = match parent {
            Some(parent) => self.storage_verifier(parent).await?,
            None => None,
        };
        let storage = crate::storage_schema::storage_schema_sql(
            OWNER_ROLE,
            &storage_verifier(&storage_password, &salt),
            parent_verifier.as_deref(),
        );
        match self.computes.admin_sql(ep, database_name, &storage).await {
            Ok(_) => Ok(()),
            Err(e) if let Some(why) = crate::storage_schema::newer_schema(&format!("{e:#}")) => {
                warn!(branch_id = %branch, %ep, database = %database_name, "{why}");
                Ok(())
            }
            Err(e) if crate::storage_schema::foreign_schema(&format!("{e:#}")) => {
                warn!(branch_id = %branch, %ep, database = %database_name, "the storage schema isn't Chelabase's (a user's own); it was left alone and storage isn't installed here");
                Ok(())
            }
            Err(e) => Err(e).context("installing the storage schema"),
        }
    }

    /// A reset or restore rewinds `GRANT <role> TO authenticator`, but secret keys survive
    /// both: grant each live secret key's role again. A role that's gone or now fails the
    /// check is skipped with a warning (its key stops working until it's fixed).
    async fn regrant_secret_key_roles(
        &self,
        branch: BranchId,
        ep: &str,
        database_name: &str,
    ) -> anyhow::Result<()> {
        let mut roles: Vec<(String, Option<i64>)> = self
            .metadata
            .list_data_keys(branch)
            .await
            .context("listing the secret keys")?
            .into_iter()
            .filter(|k| k.kind == DataKeyKind::Secret)
            .map(|k| (k.role, k.role_oid))
            .collect();
        roles.sort();
        roles.dedup();
        for (role, role_oid) in roles {
            match self.check_and_grant(branch, ep, database_name, &role, role_oid).await {
                RoleCheck::Ok(..) => {}
                other => {
                    warn!(branch_id = %branch, %role, "not granting a secret key's role to {AUTHENTICATOR}: {other:?}");
                }
            }
        }
        Ok(())
    }

    /// The branch's `authenticator` password; a new one is sealed (AAD = branch id) and
    /// stored when it has none. The store only fills an empty slot, so the password is read
    /// back: a concurrent run that stored first wins, and both use its password.
    async fn authenticator_password(&self, branch: BranchId) -> anyhow::Result<String> {
        if let Some(password) = self.stored_authenticator_password(branch).await? {
            return Ok(password);
        }
        let password = credentials::generate_password();
        let sealed = seal_credential(
            &self.key,
            &branch.to_string(),
            Purpose::Authenticator,
            &password,
        );
        self.metadata
            .set_authenticator_password(branch, &sealed)
            .await
            .context("storing the authenticator password")?;
        self.stored_authenticator_password(branch)
            .await?
            .context("the authenticator password wasn't stored (is the branch being deleted?)")
    }

    async fn stored_authenticator_password(
        &self,
        branch: BranchId,
    ) -> anyhow::Result<Option<String>> {
        let row = self
            .metadata
            .branch_connection(branch)
            .await
            .context("reading the authenticator password")?;
        row.and_then(|r| r.authenticator_password_enc)
            .map(|sealed| {
                open_credential(&self.key, &branch.to_string(), Purpose::Authenticator, &sealed)
                    .context("opening the authenticator password")
            })
            .transpose()
    }

    /// The branch's `chelabase_storage` login (the storage service's own): its password and
    /// the SCRAM salt of its verifier. A new one (sealed `Purpose::Storage`, AAD = branch id,
    /// with a random 16-byte salt) is stored when the branch has none; like
    /// [`Self::authenticator_password`] it is read back, so a racing run's wins.
    async fn storage_login(&self, branch: BranchId) -> anyhow::Result<(String, Vec<u8>)> {
        if let Some(login) = self.stored_storage_login(branch).await? {
            return Ok(login);
        }
        let password = credentials::generate_password();
        let salt: [u8; 16] = rand::random();
        let sealed = seal_credential(&self.key, &branch.to_string(), Purpose::Storage, &password);
        self.metadata
            .set_storage_password(branch, &sealed, &salt)
            .await
            .context("storing the storage login's password")?;
        info!(branch_id = %branch, "made the branch's chelabase_storage login");
        self.stored_storage_login(branch)
            .await?
            .context("the storage login's password wasn't stored (is the branch being deleted?)")
    }

    async fn stored_storage_login(
        &self,
        branch: BranchId,
    ) -> anyhow::Result<Option<(String, Vec<u8>)>> {
        let row = self
            .metadata
            .branch_connection(branch)
            .await
            .context("reading the storage login's password")?;
        row.and_then(|r| r.storage_password_enc.zip(r.storage_password_salt))
            .map(|(sealed, salt)| {
                open_credential(&self.key, &branch.to_string(), Purpose::Storage, &sealed)
                    .context("opening the storage login's password")
                    .map(|password| (password, salt))
            })
            .transpose()
    }

    /// The verifier the branch's `chelabase_storage` must carry (`None`: no login stored yet).
    pub async fn storage_verifier(&self, branch: BranchId) -> anyhow::Result<Option<String>> {
        Ok(self
            .stored_storage_login(branch)
            .await?
            .map(|(password, salt)| storage_verifier(&password, &salt)))
    }

    /// The branch's public key (role `anon`, name `public`, sealed with AAD = key id), unless
    /// it has a live one, or it's a merge's rehearsal (never served, so it gets none). Losing a
    /// race with a concurrent run is fine.
    async fn ensure_public_key(&self, branch: BranchId) -> anyhow::Result<()> {
        if self
            .metadata
            .has_live_public_key(branch)
            .await
            .context("checking the public key")?
        {
            return Ok(());
        }
        let kind = self
            .metadata
            .get_branch(branch)
            .await
            .context("reading the branch")?
            .map(|b| b.kind);
        if kind == Some(BranchKind::Rehearsal) {
            return Ok(());
        }
        let generated = data_key::generate(DataKeyKind::Public);
        let id = Uuid::new_v4();
        let key = NewDataKey {
            id,
            branch_id: branch,
            kind: DataKeyKind::Public,
            role: "anon".to_string(),
            name: "public".to_string(),
            prefix: generated.prefix,
            sha256: generated.sha256,
            sealed: Some(seal_credential(
                &self.key,
                &id.to_string(),
                Purpose::DataKey,
                &generated.key,
            )),
            databases: None,
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: false,
        };
        match self.metadata.insert_data_key(&key).await {
            Ok(()) => {
                info!(branch_id = %branch, key_id = %id, "created the branch's public key");
                Ok(())
            }
            Err(MetadataError::Conflict) => {
                info!(branch_id = %branch, "a concurrent run created the public key");
                Ok(())
            }
            Err(e) => Err(e).context("creating the public key"),
        }
    }

    /// Reconcile's `BootstrapDataApi`: only a ready `normal` branch with an endpoint. With a
    /// `database`, only that one (if it's still served); without, all of them.
    pub(crate) async fn reconcile_data_api(
        &self,
        id: BranchId,
        database: Option<&str>,
    ) -> Result<(), String> {
        let branch = self
            .metadata
            .get_branch(id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(branch) =
            branch.filter(|b| b.status == BranchStatus::Ready && b.kind == BranchKind::Normal)
        else {
            return Ok(());
        };
        let Some(ep) = branch.endpoint_id.as_deref() else {
            return Ok(());
        };
        let main_database = self.database_of(ep).await.map_err(|e| format!("{e:#}"))?;
        let result = match database {
            None => self.bootstrap_data_api(&branch, &main_database).await,
            Some(name) => {
                let served = self
                    .served_or_main(id, ep)
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                match served.iter().find(|d| d.name == name) {
                    Some(d) => {
                        self.bootstrap_data_api_on(id, ep, &d.name, &d.default_schema)
                            .await
                    }
                    None => return Ok(()),
                }
            }
        };
        result.map_err(|e| format!("bootstrapping the data API: {e:#}"))
    }

    /// The main database inside `ep`'s compute.
    pub(crate) async fn database_of(&self, ep: &str) -> anyhow::Result<String> {
        self.metadata
            .neon_databases(ep)
            .await
            .context("reading the endpoint's database")?
            .into_iter()
            .next()
            .map(|d| d.name)
            .with_context(|| format!("endpoint {ep} has no database"))
    }

    /// Whether `ep`'s main database has its data API machinery intact ([`SCHEMA_CHECK_SQL`]).
    pub async fn data_api_healthy(&self, branch: BranchId, ep: &str) -> anyhow::Result<bool> {
        let database_name = self.database_of(ep).await?;
        let schema = self
            .served_or_main(branch, ep)
            .await?
            .into_iter()
            .find(|d| d.name == database_name)
            .map_or_else(|| "public".to_owned(), |d| d.default_schema);
        self.data_api_healthy_in(branch, ep, &database_name, &schema).await
    }

    /// [`Self::data_api_healthy`] for any database of `ep`, whose default schema is `schema`
    /// ([`schema_check_sql`]).
    pub async fn data_api_healthy_in(
        &self,
        branch: BranchId,
        ep: &str,
        database: &str,
        schema: &str,
    ) -> anyhow::Result<bool> {
        let verifier = self.storage_verifier(branch).await?;
        let out = self
            .computes
            .admin_sql(ep, database, &schema_check_sql(schema, verifier.as_deref()))
            .await?;
        match out.trim() {
            "t" => Ok(true),
            "f" => Ok(false),
            other => bail!("unexpected answer {other:?}"),
        }
    }

    /// The enable job: bootstraps a database the orchestrator set `enabling`, with its
    /// default schema, then records `on` (or `error` with the reason). A row that isn't
    /// `enabling` (switched off meanwhile) is left alone; a branch that isn't ready leaves it
    /// `enabling`, for reconcile to retry.
    pub async fn enable_data_api(&self, request: &DataApiDatabaseRequest) {
        let (branch, name) = (request.branch_id, request.name.as_str());
        let target = match self.data_api_target(branch, name).await {
            Ok(Lookup::Found(target)) => target,
            Ok(Lookup::NotReady) => {
                info!(branch_id = %branch, database = name, "enabling the data API: the branch isn't ready; reconcile retries");
                return;
            }
            Ok(Lookup::Unknown) => return,
            Err(why) => {
                warn!(branch_id = %branch, database = name, "enabling the data API: {why}");
                return;
            }
        };
        if target.row.data_api != DataApiState::Enabling {
            return;
        }
        let outcome = self
            .bootstrap_data_api_on(branch, &target.ep, name, &target.row.default_schema)
            .await
            .map_err(|e| {
                warn!(branch_id = %branch, database = name, "enabling the data API: {e:#}");
                format!("{e:#}")
            });
        let succeeded = outcome.is_ok();
        if succeeded {
            // W21: realtime can open the database once it's served.
            self.grant_realtime_connect(&target.ep, &[name]).await;
        }
        match self.metadata.finish_enable_data_api(branch, name, outcome).await {
            Ok(true) if succeeded => {
                self.rebootstrap_if_schema_changed(branch, name, &target.row.default_schema).await;
            }
            Ok(_) => {}
            Err(e) => warn!(branch_id = %branch, database = name, "recording the enable: {e}"),
        }
    }

    /// After an enable made the row `on`: a `default_schema` stored while its bootstrap ran
    /// (the PATCH saw `enabling` and ran nothing) gets its own bootstrap now. A failure is
    /// logged; the database stays served with the old schema's default privileges until the
    /// next PATCH or rebootstrap.
    async fn rebootstrap_if_schema_changed(&self, branch: BranchId, name: &str, bootstrapped: &str) {
        let target = match self.data_api_target(branch, name).await {
            Ok(Lookup::Found(target)) => target,
            Ok(_) => return,
            Err(why) => {
                warn!(branch_id = %branch, database = name, "re-reading the enabled database: {why}");
                return;
            }
        };
        if target.row.data_api != DataApiState::On || target.row.default_schema == bootstrapped {
            return;
        }
        info!(branch_id = %branch, database = name, schema = %target.row.default_schema, "the default schema changed while enabling; bootstrapping it");
        if let Err(e) = self
            .bootstrap_data_api_on(branch, &target.ep, name, &target.row.default_schema)
            .await
        {
            warn!(branch_id = %branch, database = name, "bootstrapping the new default schema: {e:#}");
        }
    }

    /// The disable request: with `remove`, drops the machinery first ([`REMOVE_DATA_API_SQL`]),
    /// then records `off`. Main's can't be switched off.
    pub async fn disable_data_api(&self, request: &DataApiDatabaseRequest) -> CheckDataRoleReply {
        let (branch, name) = (request.branch_id, request.name.as_str());
        let target = match self.data_api_target(branch, name).await {
            Ok(Lookup::Found(target)) => target,
            Ok(Lookup::NotReady) => return CheckDataRoleReply::not_ready(),
            Ok(Lookup::Unknown) => {
                return CheckDataRoleReply::refused(format!("the branch has no database {name}"));
            }
            Err(why) => return CheckDataRoleReply::failed(why),
        };
        if target.row.origin == DatabaseOrigin::Main {
            return CheckDataRoleReply::refused("the main database's data API can't be switched off");
        }
        if request.remove
            && let Err(e) = self
                .computes
                .admin_sql(&target.ep, name, REMOVE_DATA_API_SQL)
                .await
        {
            let output = format!("{e:#}");
            warn!(branch_id = %branch, database = name, "removing the data API machinery: {output}");
            if let Some(message) = dependency_refusal(&output) {
                return CheckDataRoleReply::refused(format!(
                    "removing the data API machinery: {message}"
                ));
            }
            return CheckDataRoleReply::failed(format!(
                "removing the data API machinery: {output}"
            ));
        }
        match self.metadata.set_data_api_off(branch, name).await {
            Ok(_) => CheckDataRoleReply::ok(),
            Err(e) => CheckDataRoleReply::failed(format!("recording the database as off: {e}")),
        }
    }

    /// The rebootstrap request, after the database's `default_schema` changed: the script
    /// again, with the stored schema, in a served database.
    pub async fn rebootstrap_data_api(&self, request: &DataApiDatabaseRequest) -> CheckDataRoleReply {
        let (branch, name) = (request.branch_id, request.name.as_str());
        let target = match self.data_api_target(branch, name).await {
            Ok(Lookup::Found(target)) => target,
            Ok(Lookup::NotReady) => return CheckDataRoleReply::not_ready(),
            Ok(Lookup::Unknown) => {
                return CheckDataRoleReply::refused(format!("the branch has no database {name}"));
            }
            Err(why) => return CheckDataRoleReply::failed(why),
        };
        if target.row.data_api != DataApiState::On {
            return CheckDataRoleReply::refused("the database isn't served by the data API");
        }
        match self
            .bootstrap_data_api_with(
                branch,
                &target.ep,
                name,
                &target.row.default_schema,
                request.grant_existing,
            )
            .await
        {
            Ok(()) => CheckDataRoleReply::ok(),
            Err(e) => {
                warn!(branch_id = %branch, database = name, "rebootstrapping the data API: {e:#}");
                CheckDataRoleReply::failed(format!("{e:#}"))
            }
        }
    }

    /// The branch's endpoint and the database's row, for the three requests above. `Err` is a
    /// read that failed.
    async fn data_api_target(&self, branch: BranchId, name: &str) -> Result<Lookup, String> {
        let row = self
            .metadata
            .branch_connection(branch)
            .await
            .map_err(|e| format!("reading the branch: {e}"))?;
        let Some(ep) = row
            .filter(|r| r.status == BranchStatus::Ready)
            .and_then(|r| r.endpoint_id)
        else {
            return Ok(Lookup::NotReady);
        };
        let databases = self
            .metadata
            .branch_databases(branch)
            .await
            .map_err(|e| format!("reading the branch's databases: {e}"))?;
        match databases.into_iter().find(|d| d.name == name) {
            Some(row) => Ok(Lookup::Found(DataApiTarget { ep, row })),
            None => Ok(Lookup::Unknown),
        }
    }

    /// Whether `role` may back a secret key on `branch`: a valid name, not reserved or
    /// built-in (`pg_*`), existing, not a superuser, without CREATEROLE, CREATEDB or
    /// REPLICATION, and not a member (at any depth) of a superuser role, `neon_superuser` or
    /// the other system roles. BYPASSRLS is allowed (E33) and reported. On success it's
    /// granted to `authenticator` (`set_config('role', …)` needs the membership).
    pub async fn check_data_role(&self, branch: BranchId, role: &str) -> RoleCheck {
        if let Some(refusal) = refuse_by_name(role) {
            return refusal;
        }
        let row = match self.metadata.branch_connection(branch).await {
            Ok(row) => row,
            Err(e) => {
                warn!(branch_id = %branch, "reading the branch to check a role: {e}");
                return RoleCheck::Failed(format!("reading the branch: {e}"));
            }
        };
        let Some((ep, database_name)) = row
            .filter(|r| r.status == BranchStatus::Ready)
            .and_then(|r| Some((r.endpoint_id?, r.database_name?)))
        else {
            return RoleCheck::NotReady;
        };
        self.check_and_grant(branch, &ep, &database_name, role, None)
            .await
    }

    /// [`Self::check_data_role`] as the RPC's reply, with the timeline the OID was read on
    /// (E26). The branch's timeline is read before and after the check: a switch landing
    /// between them (the OID may be either timeline's) is not ready. A switch stops the compute
    /// before it moves the timeline, so a check between two equal readings ran on that one.
    pub async fn check_data_role_reply(&self, request: &CheckDataRoleRequest) -> CheckDataRoleReply {
        let branch = request.branch_id;
        let timeline = |b: Option<chelabase_metadata::BranchRecord>| b.and_then(|b| b.timeline_id);
        let before = match self.metadata.get_branch(branch).await {
            Ok(b) => timeline(b),
            Err(e) => return CheckDataRoleReply::failed(format!("reading the branch: {e}")),
        };
        match self.check_data_role(branch, &request.role).await {
            RoleCheck::Ok(oid, bypass_rls) => match self.metadata.get_branch(branch).await {
                Err(e) => CheckDataRoleReply::failed(format!("reading the branch: {e}")),
                Ok(after) => match timeline_verdict(before, timeline(after)) {
                    Ok(timeline) => {
                        CheckDataRoleReply::ok_with_oid(oid, &timeline).bypassing_rls(bypass_rls)
                    }
                    Err(why) => {
                        warn!(%branch, role = %request.role, "{why}");
                        CheckDataRoleReply::not_ready()
                    }
                },
            },
            RoleCheck::Refused(message) => CheckDataRoleReply::refused(message),
            RoleCheck::NotReady => CheckDataRoleReply::not_ready(),
            RoleCheck::Failed(message) => CheckDataRoleReply::failed(message),
        }
    }

    /// The name rules, the check query on `ep`, then the grant.
    async fn check_and_grant(
        &self,
        branch: BranchId,
        ep: &str,
        database_name: &str,
        role: &str,
        expected_oid: Option<i64>,
    ) -> RoleCheck {
        if let Some(refusal) = refuse_by_name(role) {
            return refusal;
        }
        let answer = match self
            .computes
            .admin_sql(ep, database_name, &role_check_sql(role))
            .await
        {
            Ok(answer) => answer,
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, "checking role {role}: {e:#}");
                return RoleCheck::NotReady;
            }
        };
        let refused = |why: &str| RoleCheck::Refused(format!("the role {role} {why}"));
        let answer = answer.trim();
        // `ok <oid> <t|f>`: allowed, the identity the key is bound to, and BYPASSRLS.
        let Some((oid, bypass_rls)) = parse_role_ok(answer) else {
            return match answer {
                "" => refused("does not exist"),
                "superuser" => refused("is a superuser"),
                // MH-12: a personal login never backs a key.
                "personal_login" => refused("is a personal login"),
                "neon_superuser" => refused(&format!(
                    "is a member of neon_superuser ({OWNER_LEVEL_HINT})"
                )),
                "owner_level" => refused(&format!(
                    "has CREATEROLE, CREATEDB or REPLICATION ({OWNER_LEVEL_HINT})"
                )),
                "system" => refused("has system privileges"),
                other => {
                    warn!(branch_id = %branch, "checking role {role}: unexpected answer {other:?}");
                    RoleCheck::Failed(format!(
                        "checking the role {role} gave an unexpected answer {other:?}"
                    ))
                }
            };
        };
        // A key bound to an OID: only that role may be granted, never another now holding
        // the name (the grant would outlive the key).
        if let Some(expected) = expected_oid
            && expected != oid
        {
            return refused("is now a different role than the key was bound to");
        }
        let grant = format!("GRANT {} TO {AUTHENTICATOR};", quote_ident(role));
        if let Err(e) = self.computes.admin_sql(ep, database_name, &grant).await {
            warn!(branch_id = %branch, endpoint_id = %ep, "granting {role} to {AUTHENTICATOR}: {e:#}");
            return RoleCheck::Failed(format!(
                "granting the role {role} to {AUTHENTICATOR}: {e:#}"
            ));
        }
        info!(branch_id = %branch, %role, bypass_rls, "granted to {AUTHENTICATOR}");
        RoleCheck::Ok(oid, bypass_rls)
    }
}

/// The timeline a role check ran on, from the branch's timeline before and after it: the
/// timeline when both readings agree; otherwise why the check can't be trusted.
fn timeline_verdict(before: Option<String>, after: Option<String>) -> Result<String, &'static str> {
    match (before, after) {
        (None, _) => Err("the branch has no timeline to read"),
        (Some(before), Some(after)) if before == after => Ok(before),
        (Some(_), _) => Err("the branch's timeline moved during the role check"),
    }
}

/// The refusals that need no SQL: an invalid name, a reserved role, a built-in `pg_*` role
/// (matched case-insensitively).
fn refuse_by_name(role: &str) -> Option<RoleCheck> {
    if !valid_role_name(role) {
        return Some(RoleCheck::Refused(format!(
            "{role:?} is not a valid role name"
        )));
    }
    if RESERVED_ROLES.contains(&role) {
        return Some(RoleCheck::Refused(format!("the role {role} is reserved")));
    }
    if role.len() >= 3 && role[..3].eq_ignore_ascii_case("pg_") {
        return Some(RoleCheck::Refused(format!(
            "the role {role} is a built-in role"
        )));
    }
    None
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{BranchStatus, DatabaseStatus, NewDataKey};
    use chelabase_secrets::{Purpose, open_credential};
    use chelabase_types::data_key::{self, DataKeyKind};

    use super::*;
    use crate::fakes::{
        FakeWorker, Journal, entries, position, ready_branch, ready_database, worker,
    };

    /// The data API script's first line, as the fake backend journals it (the transaction's `BEGIN;`).
    const DATA_API: &str = "BEGIN;";

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn count(journal: &Journal, prefix: &str) -> usize {
        entries(journal)
            .iter()
            .filter(|e| e.starts_with(prefix))
            .count()
    }

    /// Every data API script that ran on `ep`.
    fn data_api_scripts(w: &FakeWorker, ep: &str) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            // The resync's catalog read starts the same way.
            .filter(|(e, _, s)| {
                e == ep && s.starts_with(&format!("{DATA_API}\n")) && !s.contains("SELECT 'r',")
            })
            .map(|(_, _, s)| s.clone())
            .collect()
    }

    fn authenticator_password(w: &FakeWorker, branch: &BranchRecord) -> String {
        open_credential(
            &w.key,
            &branch.id.to_string(),
            Purpose::Authenticator,
            branch
                .authenticator_password_enc
                .as_deref()
                .expect("an authenticator password"),
        )
        .unwrap()
    }

    fn keys_of(w: &FakeWorker, branch: BranchId) -> Vec<NewDataKey> {
        w.metadata
            .state()
            .data_keys
            .iter()
            .filter(|k| k.branch_id == branch)
            .cloned()
            .collect()
    }

    #[test]
    fn data_api_sql_is_exact() {
        let expected = "BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '20s';
SELECT pg_catalog.pg_advisory_xact_lock(7163086723497615873);
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN CREATE ROLE chelabase_admin NOLOGIN; END IF;
END $$;
DO $$ DECLARE
  admin pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_admin');
  trusted pg_catalog.oid[] := ARRAY[admin, (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) CURRENT_USER)];
  ns pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname OPERATOR(pg_catalog.=) 'chelabase');
  members pg_catalog.oid[] := ARRAY(SELECT member FROM pg_catalog.pg_auth_members WHERE roleid OPERATOR(pg_catalog.=) admin);
  joined boolean := pg_catalog.cardinality(members) OPERATOR(pg_catalog.>) 0;
  tampered boolean := EXISTS (SELECT FROM pg_catalog.pg_authid a WHERE a.oid OPERATOR(pg_catalog.=) admin
    AND (a.rolcanlogin OR a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolreplication OR a.rolbypassrls
      OR a.rolpassword IS NOT NULL OR a.rolconnlimit OPERATOR(pg_catalog.<>) ALL (ARRAY[-1, 0])))
    OR EXISTS (SELECT FROM pg_catalog.pg_db_role_setting WHERE setrole OPERATOR(pg_catalog.=) admin);
  m record;
  aside pg_catalog.text;
BEGIN
  ALTER ROLE chelabase_admin NOLOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION NOBYPASSRLS NOINHERIT PASSWORD NULL CONNECTION LIMIT 0;
  FOR m IN SELECT d.datname FROM pg_catalog.pg_db_role_setting s JOIN pg_catalog.pg_database d ON d.oid OPERATOR(pg_catalog.=) s.setdatabase
    WHERE s.setrole OPERATOR(pg_catalog.=) admin
  LOOP
    EXECUTE pg_catalog.format('ALTER ROLE chelabase_admin IN DATABASE %I RESET ALL', m.datname);
  END LOOP;
  ALTER ROLE chelabase_admin RESET ALL;
  FOR m IN SELECT r.rolname AS member, g.rolname AS grantor FROM pg_catalog.pg_auth_members a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.member JOIN pg_catalog.pg_roles g ON g.oid OPERATOR(pg_catalog.=) a.grantor
    WHERE a.roleid OPERATOR(pg_catalog.=) admin
  LOOP
    EXECUTE pg_catalog.format('REVOKE chelabase_admin FROM %I GRANTED BY %I CASCADE', m.member, m.grantor);
  END LOOP;
  PERFORM pg_catalog.set_config('chelabase.former_members', pg_catalog.array_to_string(members, ','), false);
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (a.usesysid OPERATOR(pg_catalog.=) admin OR a.usesysid OPERATOR(pg_catalog.=) ANY (members)) AND NOT r.rolsuper
      AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
  IF ns IS NOT NULL AND (joined OR tampered
    OR (SELECT nspowner FROM pg_catalog.pg_namespace WHERE oid OPERATOR(pg_catalog.=) ns) OPERATOR(pg_catalog.<>) admin
    OR EXISTS (SELECT FROM pg_catalog.pg_class c WHERE c.relnamespace OPERATOR(pg_catalog.=) ns
      AND (c.relowner OPERATOR(pg_catalog.<>) ALL (trusted) OR c.reloftype OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
        OR c.relkind OPERATOR(pg_catalog.<>) ALL (ARRAY['r', 'i']::pg_catalog.\"char\"[])
        OR c.relname OPERATOR(pg_catalog.<>) ALL (ARRAY['schema_version', 'schema_version_pkey', 'bootstrap', 'bootstrap_pkey', 'app_users_table', 'app_users_table_pkey']::pg_catalog.name[])))
    OR EXISTS (SELECT FROM pg_catalog.pg_proc p WHERE p.pronamespace OPERATOR(pg_catalog.=) ns
      AND (p.proowner OPERATOR(pg_catalog.<>) ALL (trusted)
        OR p.proname OPERATOR(pg_catalog.<>) ALL (ARRAY['bump_schema_version', 'grant_extension_functions', 'claims', 'user_id', 'user_email', 'users_guard', 'users_protect', 'app_users_upsert', 'app_users_gone', 'app_users_rows', 'assert_schema_version', 'cap_response']::pg_catalog.name[])))
    OR EXISTS (SELECT FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) t.tgrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND NOT t.tgisinternal)
    OR EXISTS (SELECT FROM pg_catalog.pg_rewrite w JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) w.ev_class
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns)
    OR EXISTS (SELECT FROM pg_catalog.pg_policy y JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) y.polrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns)
    OR EXISTS (SELECT FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) a.attrelid
      JOIN pg_catalog.pg_type ty ON ty.oid OPERATOR(pg_catalog.=) a.atttypid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND a.attnum OPERATOR(pg_catalog.>) 0
        AND ty.typnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace)
    OR EXISTS (SELECT FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) d.adrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND pg_catalog.pg_get_expr(d.adbin, d.adrelid) OPERATOR(pg_catalog.<>) 'true')
    OR EXISTS (SELECT FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) k.conrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND k.contype OPERATOR(pg_catalog.<>) ALL (ARRAY['p', 'n']::pg_catalog.\"char\"[])
        AND pg_catalog.pg_get_constraintdef(k.oid) OPERATOR(pg_catalog.<>) 'CHECK (one)')
    OR EXISTS (SELECT FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) i.indrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND (i.indexprs IS NOT NULL OR i.indpred IS NOT NULL
        OR EXISTS (SELECT FROM pg_catalog.pg_opclass o WHERE o.oid OPERATOR(pg_catalog.=) ANY (i.indclass)
          AND o.opcnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace))))
  THEN
    DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_ddl;
    DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_drop;
    DROP EVENT TRIGGER IF EXISTS chelabase_extension_functions;
    aside := pg_catalog.format('chelabase_quarantine_%s', pg_catalog.floor(pg_catalog.date_part('epoch', pg_catalog.clock_timestamp()) * 1000000)::pg_catalog.int8);
    EXECUTE pg_catalog.format('ALTER SCHEMA chelabase RENAME TO %I', aside);
    PERFORM pg_catalog.set_config('chelabase.quarantined', aside, false);
  END IF;
END $$;
DO $$ BEGIN
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin' OR a.usesysid OPERATOR(pg_catalog.=) ANY
        (pg_catalog.string_to_array(pg_catalog.current_setting('chelabase.former_members', true), ',')::pg_catalog.oid[]))
      AND NOT r.rolsuper AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
END $$;
CREATE SCHEMA IF NOT EXISTS chelabase AUTHORIZATION chelabase_admin;
REVOKE ALL ON SCHEMA chelabase FROM PUBLIC;
GRANT USAGE ON SCHEMA chelabase TO authenticator;
CREATE TABLE IF NOT EXISTS chelabase.schema_version (one boolean PRIMARY KEY DEFAULT true CHECK (one), version uuid NOT NULL);
INSERT INTO chelabase.schema_version (version) VALUES (pg_catalog.gen_random_uuid()) ON CONFLICT DO NOTHING;
CREATE TABLE IF NOT EXISTS chelabase.bootstrap (one boolean PRIMARY KEY DEFAULT true CHECK (one), existing_granted boolean NOT NULL);
ALTER TABLE chelabase.schema_version OWNER TO chelabase_admin;
ALTER TABLE chelabase.bootstrap OWNER TO chelabase_admin;
REVOKE ALL ON chelabase.schema_version, chelabase.bootstrap FROM PUBLIC;
GRANT SELECT ON chelabase.schema_version TO authenticator;
GRANT USAGE ON SCHEMA chelabase TO postgres;
GRANT SELECT ON chelabase.schema_version TO postgres;
GRANT USAGE ON SCHEMA chelabase TO PUBLIC;
CREATE OR REPLACE FUNCTION chelabase.claims() RETURNS jsonb LANGUAGE sql STABLE PARALLEL SAFE
BEGIN ATOMIC
  SELECT COALESCE(NULLIF(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb, '{}'::jsonb);
END;
CREATE OR REPLACE FUNCTION chelabase.user_id() RETURNS uuid LANGUAGE sql STABLE PARALLEL SAFE
BEGIN ATOMIC
  SELECT (chelabase.claims() OPERATOR(pg_catalog.->>) 'sub')::uuid;
END;
CREATE OR REPLACE FUNCTION chelabase.user_email() RETURNS text LANGUAGE sql STABLE PARALLEL SAFE
BEGIN ATOMIC
  SELECT chelabase.claims() OPERATOR(pg_catalog.->>) 'email';
END;
ALTER FUNCTION chelabase.claims() OWNER TO CURRENT_USER;
ALTER FUNCTION chelabase.user_id() OWNER TO CURRENT_USER;
ALTER FUNCTION chelabase.user_email() OWNER TO CURRENT_USER;
GRANT EXECUTE ON FUNCTION chelabase.claims(), chelabase.user_id(), chelabase.user_email() TO PUBLIC;
CREATE OR REPLACE FUNCTION chelabase.bump_schema_version() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
  $f$ BEGIN UPDATE chelabase.schema_version SET version = pg_catalog.gen_random_uuid(); END $f$;
ALTER FUNCTION chelabase.bump_schema_version() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION chelabase.bump_schema_version() FROM PUBLIC;
CREATE OR REPLACE FUNCTION chelabase.assert_schema_version(expected uuid) RETURNS uuid
  LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = '' AS
  $f$ DECLARE v uuid; BEGIN
    SELECT version INTO v FROM chelabase.schema_version;
    IF v IS DISTINCT FROM expected THEN
      RAISE EXCEPTION USING ERRCODE = 'CB001', MESSAGE = 'chelabase: stale schema version';
    END IF;
    RETURN v; END $f$;
CREATE OR REPLACE FUNCTION chelabase.cap_response(result text, budget bigint) RETURNS text
  LANGUAGE plpgsql VOLATILE SET search_path = '' AS
  $f$ DECLARE n bigint := coalesce(pg_catalog.octet_length(result), 0); total bigint; BEGIN
    IF n OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    total := coalesce(nullif(pg_catalog.current_setting('chelabase.response_bytes', true), '')::bigint, 0) OPERATOR(pg_catalog.+) n;
    IF total OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    PERFORM pg_catalog.set_config('chelabase.response_bytes', total::text, true);
    RETURN result; END $f$;
ALTER FUNCTION chelabase.assert_schema_version(uuid) OWNER TO CURRENT_USER;
ALTER FUNCTION chelabase.cap_response(text, bigint) OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION chelabase.assert_schema_version(uuid), chelabase.cap_response(text, bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION chelabase.assert_schema_version(uuid), chelabase.cap_response(text, bigint) TO PUBLIC;
CREATE OR REPLACE FUNCTION chelabase.grant_extension_functions() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
  $f$ DECLARE f pg_catalog.regprocedure; BEGIN
    FOR f IN SELECT d.objid::pg_catalog.regprocedure FROM pg_catalog.pg_event_trigger_ddl_commands() c
      JOIN pg_catalog.pg_depend d ON d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass AND d.refobjid OPERATOR(pg_catalog.=) c.objid
      JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) d.objid
      WHERE c.object_type OPERATOR(pg_catalog.=) 'extension' AND d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass AND d.deptype OPERATOR(pg_catalog.=) 'e'
        AND p.proowner OPERATOR(pg_catalog.=) (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) 'postgres' AND NOT r.rolsuper)
    LOOP
      BEGIN
        EXECUTE pg_catalog.format('GRANT EXECUTE ON ROUTINE %s TO PUBLIC', f);
      EXCEPTION WHEN OTHERS THEN
        RAISE WARNING 'chelabase: EXECUTE on % was not granted to PUBLIC: %', f, SQLERRM;
      END;
    END LOOP;
  EXCEPTION WHEN OTHERS THEN
    RAISE WARNING 'chelabase: extension functions were not granted to PUBLIC: %', SQLERRM;
  END $f$;
ALTER FUNCTION chelabase.grant_extension_functions() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION chelabase.grant_extension_functions() FROM PUBLIC;
DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_ddl;
DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_drop;
DROP EVENT TRIGGER IF EXISTS chelabase_extension_functions;
CREATE EVENT TRIGGER chelabase_schema_version_ddl ON ddl_command_end EXECUTE FUNCTION chelabase.bump_schema_version();
CREATE EVENT TRIGGER chelabase_schema_version_drop ON sql_drop EXECUTE FUNCTION chelabase.bump_schema_version();
CREATE EVENT TRIGGER chelabase_extension_functions ON ddl_command_end WHEN TAG IN ('CREATE EXTENSION', 'ALTER EXTENSION') EXECUTE FUNCTION chelabase.grant_extension_functions();
ALTER ROLE authenticator LOGIN NOINHERIT PASSWORD 's3cretPassw0rd';
ALTER DEFAULT PRIVILEGES FOR ROLE postgres REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
DO $$ BEGIN
  IF pg_catalog.to_regnamespace('\"public\"') IS NOT NULL THEN
    ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA \"public\" GRANT SELECT ON TABLES TO anon;
    ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA \"public\" GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO authenticated;
    ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA \"public\" GRANT USAGE, SELECT ON SEQUENCES TO authenticated;
    ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA \"public\" GRANT EXECUTE ON FUNCTIONS TO authenticated;
    GRANT USAGE ON SCHEMA \"public\" TO anon, authenticated, authenticator;
  END IF;
END $$;
DO $$ DECLARE r record; BEGIN
  IF pg_catalog.to_regnamespace('\"public\"') IS NOT NULL AND NOT EXISTS (SELECT FROM chelabase.bootstrap) THEN
    GRANT SELECT ON ALL TABLES IN SCHEMA \"public\" TO anon;
    GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA \"public\" TO authenticated;
    GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA \"public\" TO authenticated;
    FOR r IN SELECT p.oid::pg_catalog.regprocedure AS f FROM pg_catalog.pg_proc p
      WHERE p.pronamespace OPERATOR(pg_catalog.=) '\"public\"'::pg_catalog.regnamespace
        AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend d WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass
          AND d.objid OPERATOR(pg_catalog.=) p.oid AND d.deptype OPERATOR(pg_catalog.=) 'e')
    LOOP
      EXECUTE pg_catalog.format('REVOKE EXECUTE ON ROUTINE %s FROM PUBLIC', r.f);
      EXECUTE pg_catalog.format('GRANT EXECUTE ON ROUTINE %s TO authenticated', r.f);
    END LOOP;
    INSERT INTO chelabase.bootstrap (existing_granted) VALUES (true);
  END IF;
END $$;
SELECT 'chelabase_missing_schema' WHERE pg_catalog.to_regnamespace('\"public\"') IS NULL;
SELECT pg_catalog.current_setting('chelabase.quarantined', true);
COMMIT;
";
        assert_eq!(data_api_sql("postgres", "s3cretPassw0rd", "public"), expected);
        // A quoted literal, quotes doubled.
        assert!(data_api_sql("postgres", "it's", "public").contains("PASSWORD 'it''s';\n"));
    }

    /// The whole script is one transaction under a transaction-scoped advisory lock: a reader
    /// (a health check, another bootstrap) never sees the event triggers dropped but not yet
    /// recreated, a failure rolls everything back (psql runs with ON_ERROR_STOP and the
    /// connection closing aborts the open transaction), and concurrent bootstraps serialize.
    #[test]
    fn the_bootstrap_script_runs_in_one_transaction_under_an_advisory_lock() {
        for sql in [
            data_api_sql("postgres", "pw", "public"),
            data_api_sql_granting_existing("postgres", "pw", "public"),
        ] {
            assert!(sql.starts_with("BEGIN;\nSET search_path = pg_catalog, pg_temp;\n"), "{sql}");
            assert!(sql.ends_with("COMMIT;\n"), "{sql}");
            let lock = format!("SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});\n");
            let lock_at = sql.find(&lock).expect("the advisory lock");
            let timeout_at = sql.find("SET LOCAL lock_timeout = '20s';\n").expect("the lock timeout");
            assert!(sql.find("BEGIN;").unwrap() < timeout_at && timeout_at < lock_at, "lock_timeout before the lock");
            assert_eq!(sql.matches("pg_advisory").count(), 1);
            let first_change = sql.find("CREATE ROLE chelabase_admin").unwrap();
            assert!(lock_at < first_change, "the lock comes before anything changes");
            // Exactly one transaction, and nothing in it that can't run inside one.
            assert_eq!(sql.matches("BEGIN;").count(), 1);
            assert_eq!(sql.matches("COMMIT;").count(), 1);
            let drop = sql.find("DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_ddl;\n").unwrap();
            assert!(sql.find("BEGIN;").unwrap() < lock_at && lock_at < drop && drop < sql.rfind("COMMIT;").unwrap());
            for forbidden in ["CREATE DATABASE", "DROP DATABASE", "VACUUM", "CONCURRENTLY", "ROLLBACK"] {
                assert!(!sql.contains(forbidden), "{forbidden}");
            }
        }
    }

    /// The script runs as a superuser on a database its owner controls: nothing it calls
    /// may resolve through the owner's schemas, and a `chelabase` schema it didn't make (or
    /// one tampered with) is renamed aside before anything touches it.
    #[test]
    fn the_script_pins_the_search_path_and_vets_chelabase_first() {
        let sql = data_api_sql("postgres", "pw", "public");
        assert!(sql.starts_with("BEGIN;\nSET search_path = pg_catalog, pg_temp;\n"), "{sql}");
        let rename = sql.find("ALTER SCHEMA chelabase RENAME TO %I").expect("the rename aside");
        let create = sql.find("CREATE SCHEMA IF NOT EXISTS chelabase").unwrap();
        assert!(rename < create);
        assert!(sql.contains("REVOKE chelabase_admin FROM %I GRANTED BY %I CASCADE"));
        for check in ["pg_trigger", "pg_rewrite", "pg_policy", "pg_attrdef", "pg_opclass", "relowner", "proowner"] {
            assert!(sql.contains(check), "{check}");
        }
        assert_eq!(sql.matches("SET search_path = pg_catalog, pg_temp AS").count(), 2);
        assert!(!sql.contains(" format("), "every function qualified");
        assert!(sql.ends_with("SELECT pg_catalog.current_setting('chelabase.quarantined', true);\nCOMMIT;\n"));
    }

    /// `chelabase_admin` owns the schema: an owner who made it first (with a login, a
    /// password or settings) could log in as it, so it's stripped every run, its sessions
    /// (and those of anyone who held it) are ended, and a tampered role means the schema
    /// is set aside.
    #[test]
    fn chelabase_admin_is_locked_down_before_the_schema_is_trusted() {
        let sql = data_api_sql("postgres", "pw", "public");
        let locked = sql
            .find("ALTER ROLE chelabase_admin NOLOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION NOBYPASSRLS NOINHERIT PASSWORD NULL CONNECTION LIMIT 0;")
            .expect("the role is locked down");
        assert!(sql.contains("ALTER ROLE chelabase_admin RESET ALL;"));
        assert!(sql.contains("ALTER ROLE chelabase_admin IN DATABASE %I RESET ALL"));
        let ended = sql.find("pg_catalog.pg_terminate_backend(a.pid)").expect("sessions ended");
        assert!(sql.contains("a.rolcanlogin OR a.rolsuper"), "a tampered role is detected");
        assert!(sql.contains("a.rolpassword IS NOT NULL"));
        assert!(sql.contains("IF ns IS NOT NULL AND (joined OR tampered"));
        let create = sql.find("CREATE SCHEMA IF NOT EXISTS chelabase").unwrap();
        assert!(locked < create && ended < create);
    }

    /// As superuser, the GRANT (DDL) would fire the owner's own event triggers with a
    /// superuser current_user: it runs as whoever ran CREATE EXTENSION, grants only what
    /// they own, and never fails their command.
    #[test]
    fn grant_extension_functions_runs_as_the_invoker_and_never_fails() {
        let sql = data_api_sql("postgres", "pw", "public");
        let start = sql.find("CREATE OR REPLACE FUNCTION chelabase.grant_extension_functions()").unwrap();
        let end = sql[start..].find("ALTER FUNCTION chelabase.grant_extension_functions()").unwrap();
        let body = &sql[start..start + end];
        assert!(body.contains("SECURITY INVOKER"), "{body}");
        assert!(!body.contains("SECURITY DEFINER"), "{body}");
        assert!(!body.contains("CURRENT_USER"), "{body}");
        assert_eq!(body.matches("EXCEPTION WHEN OTHERS THEN").count(), 2);
        assert!(body.contains("RAISE WARNING"));
    }

    /// A superuser's CREATE EXTENSION (compute_ctl, Neon's privileged path) owns every
    /// function it adds: re-granting those would undo the extension's own revokes. Only
    /// the database owner's functions (the one role whose default privilege strips PUBLIC)
    /// are granted, and never a superuser's.
    #[test]
    fn grant_extension_functions_only_touches_the_owner_roles_functions() {
        let sql = data_api_sql("app_owner", "pw", "public");
        assert!(sql.contains(
            "AND p.proowner OPERATOR(pg_catalog.=) (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) 'app_owner' AND NOT r.rolsuper)"
        ));
        assert!(data_api_sql("it's", "pw", "public").contains("OPERATOR(pg_catalog.=) 'it''s' AND NOT r.rolsuper"));
    }

    /// A session of a former member that started before the REVOKE committed could still
    /// SET ROLE chelabase_admin: sessions are ended again once it has.
    #[test]
    fn former_members_sessions_are_ended_again_after_the_revoke_commits() {
        let sql = data_api_sql("postgres", "pw", "public");
        let vet_end = sql.find("PERFORM pg_catalog.set_config('chelabase.quarantined', aside, false);").unwrap();
        let again = sql
            .find("current_setting('chelabase.former_members', true)")
            .expect("a second termination");
        let create = sql.find("CREATE SCHEMA IF NOT EXISTS chelabase").unwrap();
        assert!(vet_end < again && again < create);
        assert_eq!(sql.matches("pg_catalog.pg_terminate_backend(a.pid)").count(), 2);
    }

    #[test]
    fn quarantined_schema_is_read_from_the_output() {
        assert_eq!(
            quarantined("chelabase_quarantine_1700000000\n"),
            Some("chelabase_quarantine_1700000000")
        );
        assert_eq!(quarantined("\n"), None);
        assert_eq!(quarantined(""), None);
        assert_eq!(quarantined("something else\n"), None);
    }

    #[tokio::test]
    async fn bootstrap_survives_a_quarantine() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.computes.answers.lock().unwrap().push((
            "chelabase.quarantined".into(),
            "chelabase_quarantine_1700000000\n".into(),
        ));
        w.bootstrap_data_api(&main, "app").await.unwrap();
    }

    #[test]
    fn role_check_sql_pins_the_search_path() {
        assert!(role_check_sql("app").starts_with("SET search_path = pg_catalog, pg_temp; SELECT "));
    }

    /// Neon refuses to let a superuser run a function a non-superuser owns, and the event
    /// triggers fire for cloud_admin's own DDL: every trigger function must be owned by the
    /// superuser that runs the bootstrap (an earlier bootstrap's owner is replaced), never by
    /// chelabase_admin.
    #[test]
    fn event_trigger_functions_are_owned_by_the_bootstrap_superuser() {
        let sql = data_api_sql("postgres", "pw", "public");
        for function in [
            "chelabase.bump_schema_version()",
            "chelabase.grant_extension_functions()",
        ] {
            assert!(
                sql.contains(&format!("ALTER FUNCTION {function} OWNER TO CURRENT_USER;\n")),
                "{function} is handed to the bootstrap's superuser"
            );
            assert!(
                !sql.contains(&format!("ALTER FUNCTION {function} OWNER TO chelabase_admin")),
                "{function} must not be owned by chelabase_admin"
            );
        }
    }

    /// P5: the two guard functions the data API pipeline calls are owned by the bootstrap
    /// user and callable by every role (`cap_response` runs after the switch to a key's role,
    /// `assert_schema_version` as `authenticator`); the default `PUBLIC` grant is revoked
    /// first so the grant is explicit.
    #[test]
    fn guard_functions_are_granted_to_public_and_owned_by_the_bootstrap_user() {
        let sql = data_api_sql("postgres", "pw", "public");
        let both = "chelabase.assert_schema_version(uuid), chelabase.cap_response(text, bigint)";
        for function in ["chelabase.assert_schema_version(uuid)", "chelabase.cap_response(text, bigint)"] {
            assert!(sql.contains(&format!("ALTER FUNCTION {function} OWNER TO CURRENT_USER;\n")), "{function}");
            assert!(!sql.contains(&format!("ALTER FUNCTION {function} OWNER TO chelabase_admin")), "{function}");
        }
        let revoke = sql.find(&format!("REVOKE ALL ON FUNCTION {both} FROM PUBLIC;\n")).expect("revoke");
        let grant = sql.find(&format!("GRANT EXECUTE ON FUNCTION {both} TO PUBLIC;\n")).expect("grant");
        assert!(revoke < grant);
        assert!(sql.contains("SECURITY DEFINER SET search_path = '' AS"));
    }

    /// COALESCE, NULLIF, GREATEST and LEAST are SQL syntax, not catalog functions:
    /// `pg_catalog.nullif(...)` fails at run time (plpgsql parses bodies lazily).
    #[test]
    fn script_does_not_qualify_syntax_constructs() {
        let sql = data_api_sql("postgres", "pw", "public");
        for construct in ["nullif", "coalesce", "greatest", "least"] {
            assert!(!sql.contains(&format!("pg_catalog.{construct}")), "pg_catalog.{construct}");
        }
    }

    #[tokio::test]
    async fn bootstrap_generates_and_seals_a_password_once() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        // Provision generated, sealed (AAD = branch id) and used one.
        let password = authenticator_password(&w, &main);
        assert_eq!(password.len(), 32);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));
        let scripts = data_api_scripts(&w, &ep);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0], data_api_sql("postgres", &password, "public"));
        assert!(
            pos(&journal, "metadata.set_authenticator_password")
                < pos(&journal, &format!("compute.sql {ep} app {DATA_API}")),
            "stored before it's used: {:?}",
            entries(&journal)
        );

        // A second run reuses it.
        journal.lock().unwrap().clear();
        w.bootstrap_data_api(&main, "app").await.unwrap();
        assert_eq!(count(&journal, "metadata.set_authenticator_password"), 0);
        assert_eq!(
            w.metadata.main(id).authenticator_password_enc,
            main.authenticator_password_enc
        );
        let scripts = data_api_scripts(&w, &ep);
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[1], scripts[0]);

        // A branch without one gets a new one, once.
        w.metadata
            .state()
            .branches
            .get_mut(&main.id)
            .unwrap()
            .authenticator_password_enc = None;
        let bare = w.metadata.main(id);
        w.bootstrap_data_api(&bare, "app").await.unwrap();
        let fresh = w.metadata.main(id);
        let new_password = authenticator_password(&w, &fresh);
        assert_ne!(new_password, password);
        assert_eq!(
            data_api_scripts(&w, &ep).last().unwrap(),
            &data_api_sql("postgres", &new_password, "public")
        );
        w.bootstrap_data_api(&fresh, "app").await.unwrap();
        assert_eq!(
            w.metadata.main(id).authenticator_password_enc,
            fresh.authenticator_password_enc
        );
        assert_eq!(count(&journal, "metadata.set_authenticator_password"), 1);
    }

    /// Two runs that both find no password: the one that stores second uses the first's,
    /// so the compute and the stored password never disagree.
    #[tokio::test]
    async fn a_concurrent_password_write_wins_and_is_used() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        w.metadata
            .state()
            .branches
            .get_mut(&main.id)
            .unwrap()
            .authenticator_password_enc = None;
        let theirs = seal_credential(
            &w.key,
            &main.id.to_string(),
            Purpose::Authenticator,
            "theirs-password",
        );
        let racing = theirs.clone();
        let branch = main.id;
        w.metadata.state().before_set_authenticator_password = Some(Box::new(move |state| {
            state
                .branches
                .get_mut(&branch)
                .unwrap()
                .authenticator_password_enc = Some(racing);
        }));
        w.bootstrap_data_api(&main, "app").await.unwrap();
        assert_eq!(
            w.metadata.main(id).authenticator_password_enc.as_deref(),
            Some(theirs.as_str())
        );
        assert_eq!(
            data_api_scripts(&w, &ep).last().unwrap(),
            &data_api_sql("postgres", "theirs-password", "public")
        );
    }

    /// Reconcile's check fails on anything the bootstrap repairs: the schema, the login, the
    /// grants that let `authenticator` read the version, and the event triggers.
    #[test]
    fn the_health_check_covers_the_login_grants_and_triggers() {
        for part in [
            "to_regclass('chelabase.schema_version')",
            "rolcanlogin",
            "has_schema_privilege",
            "'USAGE'",
            "has_table_privilege",
            "'SELECT'",
            "evtenabled",
            "chelabase_schema_version_ddl",
            "chelabase_schema_version_drop",
            "chelabase_extension_functions",
            "to_regprocedure('chelabase.assert_schema_version(uuid)')",
            "to_regprocedure('chelabase.cap_response(text,bigint)')",
        ] {
            assert!(SCHEMA_CHECK_SQL.contains(part), "{part}: {SCHEMA_CHECK_SQL}");
        }
    }

    /// The check of a database also asks for `authenticator`'s `USAGE` on its default schema
    /// (the pipeline parses its statements as `authenticator`), the schema quoted as the
    /// bootstrap quotes it; a default schema that doesn't exist passes (the bootstrap skips
    /// it too, so it can't repair it).
    #[test]
    fn the_health_check_asks_for_authenticator_usage_on_the_default_schema() {
        let sql = schema_check_sql("my\"schema", Some("v"));
        // Fix round 1 (I3): the superuser session searches only pg_catalog.
        let sql = sql
            .strip_prefix("SET search_path = pg_catalog, pg_temp; ")
            .unwrap_or_else(|| panic!("the search path is pinned first: {sql}"));
        assert!(sql.starts_with(SCHEMA_CHECK_SQL), "{sql}");
        let rest = &sql[SCHEMA_CHECK_SQL.len()..];
        for part in [
            "pg_catalog.to_regnamespace('\"my\"\"schema\"')",
            "r.rolname OPERATOR(pg_catalog.=) 'authenticator'",
            "NOT pg_catalog.has_schema_privilege(r.oid, n.oid, 'USAGE')",
        ] {
            assert!(rest.contains(part), "{part}: {rest}");
        }
        assert!(schema_check_sql("public", Some("v")).contains("to_regnamespace('\"public\"')"));
    }

    #[tokio::test]
    async fn bootstrap_creates_one_public_key() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let keys = keys_of(&w, main.id);
        assert_eq!(keys.len(), 1, "{keys:?}");
        let key = &keys[0];
        assert_eq!(key.kind, DataKeyKind::Public);
        assert_eq!(key.role, "anon");
        assert_eq!(key.name, "public");
        // Sealed with the key id as AAD, so Studio can show it again.
        let shown = open_credential(
            &w.key,
            &key.id.to_string(),
            Purpose::DataKey,
            key.sealed.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(data_key::parse_kind(&shown), Some(DataKeyKind::Public));
        assert_eq!(key.prefix, shown[..12]);
        assert_eq!(key.sha256, data_key::hash(&shown));

        // A second run creates none.
        journal.lock().unwrap().clear();
        w.bootstrap_data_api(&main, "app").await.unwrap();
        assert_eq!(keys_of(&w, main.id).len(), 1);
        assert_eq!(count(&journal, "metadata.insert_data_key"), 0);

        // A concurrent run that wins the insert is fine.
        w.metadata.state().data_keys.clear();
        let theirs = NewDataKey {
            id: uuid::Uuid::new_v4(),
            ..key.clone()
        };
        let racing = theirs.clone();
        w.metadata.state().before_insert_data_key =
            Some(Box::new(move |state| state.data_keys.push(racing)));
        w.bootstrap_data_api(&main, "app").await.unwrap();
        let keys = keys_of(&w, main.id);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].id, theirs.id);
        assert_eq!(count(&journal, "metadata.insert_data_key"), 1);
    }

    #[tokio::test]
    async fn provision_runs_the_data_api_bootstrap_after_roles() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        let order = [
            pos(&journal, &format!("compute.sql {ep} app SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} app SET search_path = pg_catalog, pg_temp; DO $$")),
            pos(&journal, "metadata.set_authenticator_password"),
            pos(&journal, &format!("compute.sql {ep} app {DATA_API}")),
            pos(&journal, "metadata.insert_data_key"),
            pos(&journal, "metadata.set_branch_ready"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        // The roles' script is the first `DO $$`.
        assert!(order[1] < order[3]);
    }

    #[tokio::test]
    async fn branch_create_runs_it() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        journal.lock().unwrap().clear();
        let dev = ready_branch(&w, id, "dev", main.id).await;
        let dev = w.metadata.branch(dev);
        let ep = dev.endpoint_id.clone().unwrap();
        let order = [
            pos(&journal, &format!("compute.sql {ep} app SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} app SET search_path = pg_catalog, pg_temp; DO $$")),
            pos(&journal, &format!("compute.sql {ep} app {DATA_API}")),
            pos(&journal, "metadata.insert_data_key"),
            pos(&journal, "metadata.set_branch_ready"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        // Its own password and its own public key.
        let password = authenticator_password(&w, &dev);
        assert_ne!(password, authenticator_password(&w, &main));
        assert_eq!(
            data_api_scripts(&w, &ep),
            [data_api_sql("postgres", &password, "public")]
        );
        assert_eq!(keys_of(&w, dev.id).len(), 1);
        assert_eq!(keys_of(&w, main.id).len(), 1);
    }

    #[tokio::test]
    async fn recreate_compute_runs_role_and_data_api_bootstrap() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let dev = ready_branch(&w, id, "dev", main.id).await;
        let dev = w.metadata.branch(dev);
        let ep = dev.endpoint_id.clone().unwrap();
        w.computes.remove(&ep);
        journal.lock().unwrap().clear();

        w.recreate_compute(dev.id, &ep).await.unwrap();

        let order = [
            pos(&journal, &format!("compute.create {ep}")),
            pos(&journal, &format!("compute.sql {ep} app SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} app SET search_path = pg_catalog, pg_temp; DO $$")),
            pos(&journal, &format!("compute.sql {ep} app {DATA_API}")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        assert!(order[2] < order[3], "roles first: {:?}", entries(&journal));
        // With the password the branch already has.
        let password = authenticator_password(&w, &dev);
        assert_eq!(
            data_api_scripts(&w, &ep).last().unwrap(),
            &data_api_sql("postgres", &password, "public")
        );
        assert_eq!(count(&journal, "metadata.set_authenticator_password"), 0);
    }

    #[tokio::test]
    async fn a_bootstrap_failure_fails_the_flow_with_the_prefix() {
        const PREFIX: &str = "bootstrapping the data API: ";
        let fail = |w: &FakeWorker| {
            *w.computes.fail_sql_containing.lock().unwrap() =
                Some(("chelabase_admin".into(), "permission denied".into()));
        };

        // Provision.
        let (w, _journal) = worker();
        fail(&w);
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        let db = w.metadata.database(id);
        assert_eq!(db.status, DatabaseStatus::Failed);
        let error = db.error.unwrap();
        assert!(error.starts_with(PREFIX), "{error:?}");
        assert!(error.ends_with("permission denied"), "{error:?}");
        assert!(w.computes.list().await.unwrap().is_empty(), "torn down");

        // Branch create.
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        fail(&w);
        let dev = crate::fakes::pending_branch(&w, id, "dev", main.id);
        w.handle_branch_create(dev, None).await.unwrap();
        let dev = w.metadata.branch(dev);
        assert_eq!(dev.status, BranchStatus::Failed);
        let error = dev.error.unwrap();
        assert!(error.starts_with(PREFIX), "{error:?}");

        // Recreating a compute.
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        w.computes.remove(&ep);
        fail(&w);
        let err = w.recreate_compute(main.id, &ep).await.unwrap_err();
        assert!(err.starts_with(PREFIX), "{err:?}");
    }

    // --- Secret-key roles after a rewind ---------------------------------------------------

    fn secret_key(branch: BranchId, role: &str) -> NewDataKey {
        let generated = data_key::generate(DataKeyKind::Secret);
        NewDataKey {
            id: uuid::Uuid::new_v4(),
            branch_id: branch,
            kind: DataKeyKind::Secret,
            role: role.to_string(),
            name: format!("{role} key"),
            prefix: generated.prefix,
            sha256: generated.sha256,
            sealed: None,
            databases: None,
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: false,
        }
    }

    #[tokio::test]
    async fn bootstrap_regrants_the_roles_of_live_secret_keys() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        // The provision's own SQL (its resync reads the catalog in `postgres`) isn't checked.
        w.computes.sql.lock().unwrap().clear();
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        {
            let mut state = w.metadata.state();
            // Two keys for one role, granted once.
            state.data_keys.push(secret_key(main.id, "reporting"));
            state.data_keys.push(secret_key(main.id, "reporting"));
            state.data_keys.push(secret_key(main.id, "gone"));
            state.data_keys.push(secret_key(main.id, "now_super"));
            let revoked = secret_key(main.id, "old_role");
            state.revoked_keys.insert(revoked.id);
            state.data_keys.push(revoked);
            // Another branch's key.
            state
                .data_keys
                .push(secret_key(BranchId::new(), "elsewhere"));
        }
        // `gone` was dropped, `now_super` became a superuser: skipped, not fatal.
        w.computes.answer("rolname = 'gone'", "");
        w.computes.answer("rolname = 'now_super'", "superuser\n");
        answer_check(&w, "ok 16384\n");
        journal.lock().unwrap().clear();

        w.bootstrap_data_api(&main, "app").await.unwrap();

        assert_eq!(grants(&w), ["GRANT \"reporting\" TO authenticator;"]);
        let sql = w.computes.sql.lock().unwrap().clone();
        let script = sql
            .iter()
            .position(|(_, _, s)| s.starts_with(DATA_API))
            .unwrap();
        let grant = sql
            .iter()
            .position(|(_, _, s)| s.starts_with("GRANT"))
            .unwrap();
        assert!(script < grant, "after the script");
        assert!(sql.iter().all(|(e, d, _)| e == &ep && d == "app"));
        // Revoked keys and other branches' keys aren't even checked.
        assert!(
            !sql.iter()
                .any(|(_, _, s)| s.contains("old_role") || s.contains("elsewhere"))
        );
        assert!(position(&journal, "compute.sql").is_some());
    }

    // A switch regrants by name before the key sync: a different role (another OID) now
    // holding a bound key's name must not get granted to authenticator.
    #[tokio::test]
    async fn bootstrap_does_not_regrant_a_role_with_another_oid() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        w.computes.sql.lock().unwrap().clear();
        let main = w.metadata.main(id);
        {
            let mut state = w.metadata.state();
            let mut moved = secret_key(main.id, "impostor");
            moved.role_oid = Some(16384);
            moved.role_timeline = Some("11111111111111111111111111111111".into());
            state.data_keys.push(moved);
            let mut same = secret_key(main.id, "same");
            same.role_oid = Some(16385);
            same.role_timeline = Some("11111111111111111111111111111111".into());
            state.data_keys.push(same);
            // An unbound key (made before E26) is granted by name as before.
            state.data_keys.push(secret_key(main.id, "unbound"));
        }
        w.computes.answer("rolname = 'impostor'", "ok 16999\n");
        w.computes.answer("rolname = 'same'", "ok 16385\n");
        w.computes.answer("rolname = 'unbound'", "ok 17000\n");

        w.bootstrap_data_api(&main, "app").await.unwrap();

        let mut granted = grants(&w);
        granted.sort();
        assert_eq!(
            granted,
            ["GRANT \"same\" TO authenticator;", "GRANT \"unbound\" TO authenticator;"]
        );
    }

    // A branch with no timeline to read is not "a timeline that moved": say which.
    #[tokio::test]
    async fn check_role_reply_without_a_timeline_is_not_ready_and_says_why() {
        assert_eq!(timeline_verdict(None, None), Err("the branch has no timeline to read"));
        assert_eq!(timeline_verdict(None, Some("a".into())), Err("the branch has no timeline to read"));
        assert_eq!(
            timeline_verdict(Some("a".into()), Some("b".into())),
            Err("the branch's timeline moved during the role check")
        );
        assert_eq!(
            timeline_verdict(Some("a".into()), None),
            Err("the branch's timeline moved during the role check")
        );
        assert_eq!(timeline_verdict(Some("a".into()), Some("a".into())), Ok("a".to_string()));

        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = BranchId::from(id);
        answer_check(&w, "ok 16390\n");
        w.metadata.state().branches.get_mut(&main).unwrap().timeline_id = None;
        let reply = w
            .check_data_role_reply(&chelabase_nats::CheckDataRoleRequest { branch_id: main, role: "reader".into() })
            .await;
        assert_eq!(reply, CheckDataRoleReply::not_ready());
    }

    // --- The secret-key role check ---------------------------------------------------------

    /// The check query's answer, as psql prints it.
    fn answer_check(w: &FakeWorker, stdout: &str) {
        w.computes.answer("pg_has_role", stdout);
    }

    fn grants(w: &FakeWorker) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.starts_with("GRANT"))
            .map(|(_, _, s)| s.clone())
            .collect()
    }

    fn refused(message: &str) -> RoleCheck {
        RoleCheck::Refused(message.to_string())
    }

    #[tokio::test]
    async fn check_role_refusals() {
        let cases = [
            ("ghost", "", "the role ghost does not exist"),
            ("boss", "superuser\n", "the role boss is a superuser"),
            (
                "owner",
                "neon_superuser\n",
                "the role owner is a member of neon_superuser (owner-level roles can't \
                 back a secret key; use a role without CREATEROLE, CREATEDB, REPLICATION or \
                 neon_superuser: BYPASSRLS is allowed)",
            ),
            (
                "shell",
                "system\n",
                "the role shell has system privileges",
            ),
            (
                "wide",
                "owner_level\n",
                "the role wide has CREATEROLE, CREATEDB or REPLICATION (owner-level roles can't \
                 back a secret key; use a role without CREATEROLE, CREATEDB, REPLICATION or \
                 neon_superuser: BYPASSRLS is allowed)",
            ),
        ];
        for (role, stdout, message) in cases {
            let (w, _journal) = worker();
            let id = ready_database(&w).await;
            // The provision's own SQL (its resync reads the catalog in `postgres`).
            w.computes.sql.lock().unwrap().clear();
            answer_check(&w, stdout);
            let main = BranchId::from(id);
            assert_eq!(
                w.check_data_role(main, role).await,
                refused(message),
                "{role}"
            );
            assert!(grants(&w).is_empty(), "{role}");
            // The check ran on the branch's compute and database.
            let sql = w.computes.sql.lock().unwrap().clone();
            let check = sql
                .iter()
                .find(|(_, _, s)| s.contains("pg_has_role"))
                .expect("the check ran");
            assert_eq!(check.0, w.metadata.main(id).endpoint_id.unwrap());
            assert_eq!(check.1, "app");
            assert_eq!(check.2, role_check_sql(role));
        }
    }

    // The check answers the role's OID: the key is bound to that identity, not to the name.
    #[tokio::test]
    async fn check_role_returns_the_roles_oid() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        answer_check(&w, "ok 16390 f\n");
        let main = BranchId::from(id);
        assert_eq!(w.check_data_role(main, "reader").await, RoleCheck::Ok(16390, false));
        let reply = w
            .check_data_role_reply(&chelabase_nats::CheckDataRoleRequest { branch_id: main, role: "reader".into() })
            .await;
        // With the timeline the OID was read on (E26).
        let timeline = w.metadata.branch(main).timeline_id.unwrap();
        assert_eq!(reply, CheckDataRoleReply::ok_with_oid(16390, &timeline).bypassing_rls(false));
        assert_eq!(reply.role_timeline.as_deref(), Some(timeline.as_str()));
        // A switch landing during the check: the OID may be either timeline's, so not ready.
        w.metadata.state().move_timeline_at_get_branch(main, 2, "33333333333333333333333333333333");
        let reply = w
            .check_data_role_reply(&chelabase_nats::CheckDataRoleRequest { branch_id: main, role: "reader".into() })
            .await;
        assert_eq!(reply, CheckDataRoleReply::not_ready());
        assert!(role_check_sql("reader").contains("'ok ' || r.oid::text"));
        // An `ok` without an OID is an answer it doesn't know; so is a flag it doesn't know.
        for unknown in ["ok\n", "ok 16390 maybe\n", "ok 16390 t x\n"] {
            w.computes.answers.lock().unwrap().clear();
            answer_check(&w, unknown);
            assert!(matches!(w.check_data_role(main, "reader").await, RoleCheck::Failed(_)), "{unknown:?}");
        }
    }

    // T6 (MH-12, spec Part 6 "Hidden elsewhere"): the worker's key-role check refuses a role
    // carrying the personal-login marker (the round-4 predicate, read by the check query
    // itself), answering `personal_login`, and grants nothing to `authenticator`.
    #[tokio::test]
    async fn a_key_cannot_use_a_marked_role() {
        let sql = role_check_sql("alice");
        assert!(
            sql.contains("chelabase.managed_role=") && sql.contains("pg_db_role_setting"),
            "the check reads the marker: {sql}"
        );
        assert!(sql.contains("'personal_login'"), "{sql}");
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = BranchId::from(id);
        w.computes.sql.lock().unwrap().clear();
        answer_check(&w, "personal_login\n");
        assert_eq!(
            w.check_data_role(main, "alice").await,
            refused("the role alice is a personal login")
        );
        assert!(grants(&w).is_empty(), "{:?}", grants(&w));
        let reply = w
            .check_data_role_reply(&chelabase_nats::CheckDataRoleRequest { branch_id: main, role: "alice".into() })
            .await;
        assert_eq!(reply.kind.as_deref(), Some("refused"));
    }

    // E33: a role with BYPASSRLS may back a secret key: the check passes, grants it to
    // `authenticator`, and says so, so the key is marked "bypasses RLS".
    #[tokio::test]
    async fn a_bypassrls_role_may_back_a_key_and_is_marked() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = BranchId::from(id);
        w.computes.sql.lock().unwrap().clear();
        answer_check(&w, "ok 16390 t\n");
        assert_eq!(w.check_data_role(main, "service").await, RoleCheck::Ok(16390, true));
        assert_eq!(grants(&w), ["GRANT \"service\" TO authenticator;"]);
        let reply = w
            .check_data_role_reply(&chelabase_nats::CheckDataRoleRequest { branch_id: main, role: "service".into() })
            .await;
        assert_eq!(reply.role_bypass_rls, Some(true));
        assert!(reply.ok);
    }

    // T14 (state-machine slice M12, in this file): a merge's rehearsal is never served, so
    // its data API bootstrap makes no public key; a normal branch still gets one.
    #[tokio::test]
    async fn a_rehearsal_gets_no_public_key() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        for (name, kind) in [
            ("_rehearsal-0123abcd", chelabase_metadata::BranchKind::Rehearsal),
            ("dev", chelabase_metadata::BranchKind::Normal),
        ] {
            let branch = crate::fakes::pending_branch(&w, id, name, main.id);
            w.metadata.state().branches.get_mut(&branch).unwrap().kind = kind;
            w.handle_branch_create(branch, None).await.unwrap();
            let b = w.metadata.branch(branch);
            assert_eq!(b.status, BranchStatus::Ready, "{name}: {:?}", b.error);
            let keys = w.metadata.list_data_keys(branch).await.unwrap();
            assert_eq!(
                keys.iter().any(|k| k.kind == DataKeyKind::Public),
                kind == chelabase_metadata::BranchKind::Normal,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn check_role_refusal_names_plain_roles() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        answer_check(&w, "neon_superuser\n");

        assert_eq!(
            w.check_data_role(BranchId::from(id), "admin").await,
            refused(
                "the role admin is a member of neon_superuser (owner-level roles can't back \
                 a secret key; use a role without CREATEROLE, CREATEDB, REPLICATION or \
                 neon_superuser: BYPASSRLS is allowed)"
            )
        );
    }

    #[test]
    fn the_check_sql_covers_every_system_role_by_oid() {
        let sql = role_check_sql("app");
        assert!(
            sql.contains("FROM pg_roles r WHERE r.rolname = 'app'"),
            "{sql}"
        );
        // Membership at any depth, by oid: a role missing from the image just matches nothing.
        assert!(sql.contains("pg_has_role(r.oid, s.oid, 'MEMBER')"), "{sql}");
        assert!(
            !sql.contains("pg_has_role(r.oid, 'neon_superuser'"),
            "{sql}"
        );
        assert!(sql.contains("s.rolsuper"), "{sql}");
        // The same set rest refuses on every request (F2 fix round M7).
        for group in chelabase_types::data_key::KEY_ROLE_REFUSED_GROUPS {
            assert!(sql.contains(&format!("'{group}'")), "{group} in {sql}");
        }
        assert!(sql.contains("WHEN r.rolsuper THEN 'superuser'"), "{sql}");
    }

    // E33's owner-level definition, as rest's: CREATEROLE, CREATEDB or REPLICATION on the role
    // itself makes it owner-level (attributes aren't inherited; neon_superuser counts at any
    // depth). BYPASSRLS doesn't: it's answered with the OID instead.
    #[test]
    fn the_check_sql_refuses_each_owner_level_attribute() {
        let sql = role_check_sql("app");
        let (_, owner_level) = sql
            .split_once("WHEN r.rolcreaterole")
            .map(|(a, b)| (a, format!("r.rolcreaterole{b}")))
            .expect("an owner-level branch");
        let owner_level = owner_level
            .split_once("THEN 'owner_level'")
            .expect("answered owner_level")
            .0;
        for attribute in ["r.rolcreaterole", "r.rolcreatedb", "r.rolreplication"] {
            assert!(owner_level.contains(attribute), "{attribute} in {sql}");
        }
        assert!(!owner_level.contains("rolbypassrls"), "{sql}");
        assert!(
            sql.contains("'ok ' || r.oid::text || CASE WHEN r.rolbypassrls THEN ' t' ELSE ' f' END"),
            "{sql}"
        );
        // After superuser and neon_superuser, before the system roles.
        let at = |needle: &str| {
            sql.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {sql}"))
        };
        assert!(at("THEN 'superuser'") < at("THEN 'neon_superuser'"));
        assert!(at("THEN 'neon_superuser'") < at("THEN 'owner_level'"));
        assert!(at("THEN 'owner_level'") < at("THEN 'system'"));
    }

    #[tokio::test]
    async fn check_role_refuses_reserved_and_built_in_roles_before_sql() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        answer_check(&w, "ok 16384\n");
        journal.lock().unwrap().clear();
        let main = BranchId::from(id);
        for (role, message) in [
            ("authenticator", "the role authenticator is reserved"),
            ("chelabase_admin", "the role chelabase_admin is reserved"),
            (
                "pg_execute_server_program",
                "the role pg_execute_server_program is a built-in role",
            ),
            ("pg_monitor", "the role pg_monitor is a built-in role"),
            ("PG_Custom", "the role PG_Custom is a built-in role"),
        ] {
            assert_eq!(
                w.check_data_role(main, role).await,
                refused(message),
                "{role}"
            );
        }
        assert_eq!(count(&journal, "compute.sql"), 0, "{:?}", entries(&journal));
    }

    #[tokio::test]
    async fn check_data_role_rpc_replies() {
        use chelabase_nats::{CheckDataRoleReply, CheckDataRoleRequest};
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = BranchId::from(id);
        let ask = |role: &str, branch| CheckDataRoleRequest { branch_id: branch, role: role.into() };

        answer_check(&w, "ok 16384 f\n");
        let timeline = w.metadata.branch(main).timeline_id.unwrap();
        assert_eq!(
            w.check_data_role_reply(&ask("reporting", main)).await,
            CheckDataRoleReply::ok_with_oid(16384, &timeline).bypassing_rls(false)
        );

        w.computes.answers.lock().unwrap().clear();
        answer_check(&w, "superuser\n");
        let reply = w.check_data_role_reply(&ask("boss", main)).await;
        assert!(!reply.ok);
        assert_eq!(reply.kind.as_deref(), Some("refused"));
        assert_eq!(reply.error.as_deref(), Some("the role boss is a superuser"));

        let reply = w.check_data_role_reply(&ask("authenticator", main)).await;
        assert_eq!(reply.kind.as_deref(), Some("refused"));

        let reply = w.check_data_role_reply(&ask("anon", BranchId::new())).await;
        assert!(!reply.ok);
        assert_eq!(reply.kind.as_deref(), Some("not_ready"));

        w.metadata.state().fail = Some("branch_connection");
        let reply = w.check_data_role_reply(&ask("anon", main)).await;
        assert_eq!(reply.kind.as_deref(), Some("failed"));
    }

    #[tokio::test]
    async fn check_role_not_ready_and_failures() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = BranchId::from(id);
        journal.lock().unwrap().clear();
        // Resetting, or unknown: not ready, before any SQL.
        w.metadata.state().branches.get_mut(&main).unwrap().status = BranchStatus::Resetting;
        assert_eq!(w.check_data_role(main, "anon").await, RoleCheck::NotReady);
        assert_eq!(
            w.check_data_role(BranchId::new(), "anon").await,
            RoleCheck::NotReady
        );
        assert_eq!(count(&journal, "compute.sql"), 0);
        w.metadata.state().branches.get_mut(&main).unwrap().status = BranchStatus::Ready;
        // Its compute unreachable.
        *w.computes.sql_failures_left.lock().unwrap() = 1;
        assert_eq!(w.check_data_role(main, "anon").await, RoleCheck::NotReady);
        // Metadata trouble.
        w.metadata.state().fail = Some("branch_connection");
        assert!(matches!(
            w.check_data_role(main, "anon").await,
            RoleCheck::Failed(_)
        ));
        w.metadata.state().fail = None;
        // An answer it doesn't know.
        answer_check(&w, "maybe\n");
        assert!(matches!(
            w.check_data_role(main, "anon").await,
            RoleCheck::Failed(_)
        ));
        // A failed GRANT.
        w.computes.answers.lock().unwrap().clear();
        answer_check(&w, "ok 16384\n");
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("GRANT".into(), "circular membership".into()));
        assert!(matches!(
            w.check_data_role(main, "anon").await,
            RoleCheck::Failed(_)
        ));
    }

    #[tokio::test]
    async fn check_role_rejects_invalid_names_before_sql() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        answer_check(&w, "ok 16384\n");
        journal.lock().unwrap().clear();
        let too_long = format!("a{}", "b".repeat(63));
        for bad in [
            "",
            "1abc",
            "a-b",
            "a b",
            "a\"b",
            "a'b",
            "x;DROP ROLE y",
            "é",
            "$a",
            too_long.as_str(),
        ] {
            let RoleCheck::Refused(err) = w.check_data_role(BranchId::from(id), bad).await else {
                panic!("{bad:?} not refused");
            };
            assert!(err.contains("not a valid role name"), "{bad:?}: {err}");
        }
        assert_eq!(count(&journal, "compute.sql"), 0, "{:?}", entries(&journal));
        // The edges that are fine.
        let longest = format!("a{}", "b".repeat(62));
        for good in ["_", "a$1", "Ab_9", longest.as_str()] {
            assert_eq!(
                w.check_data_role(BranchId::from(id), good).await,
                RoleCheck::Ok(16384, false)
            );
        }
    }

    #[tokio::test]
    async fn check_role_grants_to_authenticator() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        answer_check(&w, "ok 16384\n");
        for role in ["app_user", "AppUser", "anon", "authenticated"] {
            assert_eq!(
                w.check_data_role(BranchId::from(id), role).await,
                RoleCheck::Ok(16384, false),
                "{role}"
            );
        }
        assert_eq!(
            grants(&w),
            [
                "GRANT \"app_user\" TO authenticator;",
                "GRANT \"AppUser\" TO authenticator;",
                "GRANT \"anon\" TO authenticator;",
                "GRANT \"authenticated\" TO authenticator;",
            ]
        );
        let sql = w.computes.sql.lock().unwrap().clone();
        assert!(
            sql.iter()
                .filter(|(_, _, s)| s.starts_with("GRANT"))
                .all(|(e, d, _)| e == &ep && d == "app")
        );
    }

    // --- Databases beyond main (6c) -----------------------------------------------------

    use chelabase_metadata::DataApiState;
    use chelabase_nats::DataApiDatabaseRequest;

    use crate::fakes::FakeDbRow;

    fn request(branch: BranchId, name: &str, remove: bool) -> DataApiDatabaseRequest {
        DataApiDatabaseRequest { branch_id: branch, name: name.to_string(), remove, grant_existing: false }
    }

    fn add_row(w: &FakeWorker, branch: BranchId, name: &str, state: DataApiState, schema: &str) {
        w.metadata
            .state()
            .db_rows
            .entry(branch)
            .or_default()
            .push(FakeDbRow::new(name, state, schema));
    }

    fn row(w: &FakeWorker, branch: BranchId, name: &str) -> FakeDbRow {
        w.metadata.state().db_rows[&branch]
            .iter()
            .find(|r| r.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no row {name}"))
    }

    /// The bootstrap scripts that ran in one database of `ep`.
    fn scripts_in(w: &FakeWorker, ep: &str, database: &str) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, d, s)| {
                e == ep && d == database && s.starts_with(&format!("{DATA_API}\n")) && !s.contains("SELECT 'r',")
            })
            .map(|(_, _, s)| s.clone())
            .collect()
    }

    #[tokio::test]
    async fn enable_bootstraps_that_database_with_its_schema() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        add_row(&w, main.id, "analytics", DataApiState::Enabling, "reporting");
        let password = authenticator_password(&w, &main);

        w.enable_data_api(&request(main.id, "analytics", false)).await;

        // Its own script, with its schema, only in that database; the row is `on`.
        assert_eq!(
            scripts_in(&w, &ep, "analytics"),
            [data_api_sql("postgres", &password, "reporting")]
        );
        assert_eq!(scripts_in(&w, &ep, "app").len(), 1, "main is not bootstrapped again");
        let r = row(&w, main.id, "analytics");
        assert_eq!((r.state, r.error), (DataApiState::On, None));
        // A row that isn't `enabling` (switched off meanwhile) is left alone.
        add_row(&w, main.id, "gone", DataApiState::Off, "public");
        w.enable_data_api(&request(main.id, "gone", false)).await;
        assert!(scripts_in(&w, &ep, "gone").is_empty());
        assert_eq!(row(&w, main.id, "gone").state, DataApiState::Off);
    }

    /// A `default_schema` stored while the enable's bootstrap ran (the PATCH saw `enabling`
    /// and ran nothing): the enable reads the row again once it's `on` and bootstraps the new
    /// schema too.
    #[tokio::test]
    async fn enable_rebootstraps_a_schema_changed_while_it_ran() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        add_row(&w, main.id, "analytics", DataApiState::Enabling, "reporting");
        let password = authenticator_password(&w, &main);
        let branch = main.id;
        w.metadata.state().before_finish_enable = Some(Box::new(move |state| {
            let row = state.db_rows.get_mut(&branch).unwrap().iter_mut().find(|r| r.name == "analytics").unwrap();
            row.schema = "stats".into();
        }));

        w.enable_data_api(&request(main.id, "analytics", false)).await;

        assert_eq!(
            scripts_in(&w, &ep, "analytics"),
            [data_api_sql("postgres", &password, "reporting"), data_api_sql("postgres", &password, "stats")]
        );
        let r = row(&w, main.id, "analytics");
        assert_eq!((r.state, r.schema.as_str()), (DataApiState::On, "stats"));
        // Unchanged: one bootstrap.
        add_row(&w, main.id, "plain", DataApiState::Enabling, "public");
        w.enable_data_api(&request(main.id, "plain", false)).await;
        assert_eq!(scripts_in(&w, &ep, "plain").len(), 1);
    }

    #[tokio::test]
    async fn enable_failure_records_error() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        add_row(&w, main.id, "analytics", DataApiState::Enabling, "nosuch");
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("chelabase_admin".into(), "schema \"nosuch\" does not exist".into()));

        w.enable_data_api(&request(main.id, "analytics", false)).await;

        let r = row(&w, main.id, "analytics");
        assert_eq!(r.state, DataApiState::Error);
        assert!(r.error.unwrap().contains("does not exist"));
        // A branch that isn't ready leaves the row `enabling` for reconcile.
        add_row(&w, main.id, "later", DataApiState::Enabling, "public");
        w.metadata.state().branches.get_mut(&main.id).unwrap().status = BranchStatus::Resetting;
        w.enable_data_api(&request(main.id, "later", false)).await;
        assert_eq!(row(&w, main.id, "later").state, DataApiState::Enabling);
    }

    #[tokio::test]
    async fn disable_with_remove_drops_the_machinery() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        add_row(&w, main.id, "analytics", DataApiState::On, "public");
        add_row(&w, main.id, "keep", DataApiState::On, "public");

        // Without `remove`: only the row changes.
        let reply = w.disable_data_api(&request(main.id, "keep", false)).await;
        assert_eq!(reply, CheckDataRoleReply::ok());
        assert_eq!(row(&w, main.id, "keep").state, DataApiState::Off);
        assert_eq!(count(&journal, &format!("compute.sql {ep} keep")), 0);

        // With it: the three triggers and the schema, then the row.
        journal.lock().unwrap().clear();
        let reply = w.disable_data_api(&request(main.id, "analytics", true)).await;
        assert_eq!(reply, CheckDataRoleReply::ok());
        let sql: Vec<String> = w
            .computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, d, _)| e == &ep && d == "analytics")
            .map(|(_, _, s)| s.clone())
            .collect();
        let drop = sql.iter().find(|s| s.contains("DROP SCHEMA")).expect("the drop script");
        assert_eq!(drop, REMOVE_DATA_API_SQL);
        assert_eq!(row(&w, main.id, "analytics").state, DataApiState::Off);
        assert!(pos(&journal, &format!("compute.sql {ep} analytics")) < pos(&journal, "metadata.set_data_api_off"));

        // Main and unknown names are refused; a branch that isn't ready is not ready.
        let reply = w.disable_data_api(&request(main.id, "app", true)).await;
        assert_eq!(reply.kind.as_deref(), Some("refused"));
        let reply = w.disable_data_api(&request(main.id, "missing", false)).await;
        assert_eq!(reply.kind.as_deref(), Some("refused"));
        // A failing drop keeps the database on and says `failed`.
        add_row(&w, main.id, "stuck", DataApiState::On, "public");
        *w.computes.fail_sql_containing.lock().unwrap() = Some(("DROP SCHEMA".into(), "boom".into()));
        let reply = w.disable_data_api(&request(main.id, "stuck", true)).await;
        assert_eq!(reply.kind.as_deref(), Some("failed"));
        assert_eq!(row(&w, main.id, "stuck").state, DataApiState::On);
        w.metadata.state().branches.get_mut(&main.id).unwrap().status = BranchStatus::Resetting;
        let reply = w.disable_data_api(&request(main.id, "stuck", false)).await;
        assert_eq!(reply.kind.as_deref(), Some("not_ready"));
    }

    /// A remove that Postgres refuses because something still uses the machinery (a policy on
    /// `chelabase.user_id()`, P29) is `refused` with Postgres' message, so the API can show it;
    /// the database stays on.
    #[tokio::test]
    async fn remove_blocked_by_a_dependency_is_refused_with_the_message() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        add_row(&w, main.id, "extra", DataApiState::On, "public");
        let output = "NOTICE:  event trigger \"chelabase_users_protect_drop\" does not exist, skipping\n\
                      ERROR:  cannot drop function chelabase.user_id() because other objects depend on it\n\
                      DETAIL:  policy mine_own on table mine depends on function chelabase.user_id()\n\
                      HINT:  Use DROP ... CASCADE to drop the dependent objects too.";
        *w.computes.fail_sql_containing.lock().unwrap() = Some(("DROP SCHEMA".into(), output.into()));
        let reply = w.disable_data_api(&request(main.id, "extra", true)).await;
        assert_eq!(reply.kind.as_deref(), Some("refused"), "{reply:?}");
        let message = reply.error.unwrap();
        assert!(
            message.contains("cannot drop function chelabase.user_id() because other objects depend on it")
                && message.contains("policy mine_own on table mine depends on function chelabase.user_id()"),
            "{message}"
        );
        assert!(!message.contains("NOTICE"), "{message}");
        assert_eq!(row(&w, main.id, "extra").state, DataApiState::On);
    }

    /// N3: psql's stderr follows `run_exec`'s prefix on the same line when no NOTICE comes
    /// first, and terse verbosity prints no DETAIL.
    #[test]
    fn dependency_refusal_is_found_anywhere_in_a_line() {
        let message = "cannot drop function chelabase.user_id() because other objects depend on it";
        assert_eq!(
            dependency_refusal(&format!("psql in compute ep-1 exited with 3: ERROR:  {message}")),
            Some(message.to_string())
        );
        assert_eq!(
            dependency_refusal(&format!("psql in compute ep-1 exited with 3: ERROR:  {message}\nDETAIL:  policy p on table t depends on function chelabase.user_id()")),
            Some(format!("{message} (policy p on table t depends on function chelabase.user_id())"))
        );
        assert_eq!(
            dependency_refusal("psql in compute ep-1 exited with 3: ERROR:  permission denied for schema chelabase"),
            None
        );
        assert_eq!(dependency_refusal("connection refused"), None);
    }

    /// Only what the bootstrap made is dropped, by name, and only from a `chelabase` schema owned
    /// by `chelabase_admin`; `RESTRICT` refuses to take anything else with it. A `chelabase`
    /// schema someone else owns is left alone with a warning. The cluster-wide role stays.
    #[test]
    fn remove_sql_drops_only_the_machinery_of_our_schema() {
        assert_eq!(
            REMOVE_DATA_API_SQL,
            "-- remove the data API machinery
SET search_path = pg_catalog, pg_temp;
DO $$ BEGIN
  SET LOCAL lock_timeout = '20s';
  PERFORM pg_catalog.pg_advisory_xact_lock(7163086723497615873);
  IF EXISTS (SELECT FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner
      WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase' AND r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN
    DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_ddl;
    DROP EVENT TRIGGER IF EXISTS chelabase_schema_version_drop;
    DROP EVENT TRIGGER IF EXISTS chelabase_extension_functions;
    DROP EVENT TRIGGER IF EXISTS chelabase_users_protect_drop;
    DROP EVENT TRIGGER IF EXISTS chelabase_users_protect_ddl;
    DROP FUNCTION IF EXISTS chelabase.users_protect();
    DROP FUNCTION IF EXISTS chelabase.app_users_upsert(uuid, text, boolean, timestamptz, jsonb);
    DROP FUNCTION IF EXISTS chelabase.app_users_gone(uuid, boolean);
    DROP FUNCTION IF EXISTS chelabase.app_users_rows();
    DROP TABLE IF EXISTS chelabase.app_users_table;
    DROP FUNCTION IF EXISTS chelabase.user_email();
    DROP FUNCTION IF EXISTS chelabase.user_id();
    DROP FUNCTION IF EXISTS chelabase.claims();
    DROP FUNCTION IF EXISTS chelabase.bump_schema_version();
    DROP FUNCTION IF EXISTS chelabase.assert_schema_version(uuid);
    DROP FUNCTION IF EXISTS chelabase.cap_response(text, bigint);
    DROP FUNCTION IF EXISTS chelabase.grant_extension_functions();
    DROP TABLE IF EXISTS chelabase.schema_version;
    DROP TABLE IF EXISTS chelabase.bootstrap;
    DROP SCHEMA chelabase RESTRICT;
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_namespace WHERE nspname OPERATOR(pg_catalog.=) 'chelabase') THEN
    RAISE WARNING 'chelabase: the chelabase schema is not owned by chelabase_admin; nothing was removed';
  END IF;
END $$;
"
        );
        let timeout_at = REMOVE_DATA_API_SQL.find("SET LOCAL lock_timeout = '20s';").unwrap();
        let lock_at = REMOVE_DATA_API_SQL.find("pg_advisory_xact_lock").unwrap();
        let first_drop = REMOVE_DATA_API_SQL.find("DROP ").unwrap();
        assert!(timeout_at < lock_at && lock_at < first_drop, "timeout, then lock, then the drops");
        assert!(REMOVE_DATA_API_SQL.contains(&format!("pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});")));
        assert!(REMOVE_DATA_API_SQL.contains(&format!("SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';")));
        assert!(!REMOVE_DATA_API_SQL.contains("CASCADE"));
        assert!(!REMOVE_DATA_API_SQL.contains("DROP ROLE"), "the cluster-wide role stays");
    }

    /// P4: what the users bootstrap and the claim helpers add to `chelabase` doesn't make the
    /// data API bootstrap set the schema aside.
    #[test]
    fn tamper_check_allows_app_users_objects() {
        let sql = data_api_sql("postgres", "pw", "public");
        let array = |column: &str| {
            let start = sql.find(&format!("{column} OPERATOR(pg_catalog.<>) ALL (ARRAY[")).unwrap();
            sql[start..start + sql[start..].find("]::").unwrap()].to_string()
        };
        let (relnames, pronames) = (array("c.relname"), array("p.proname"));
        for name in ["app_users_table", "app_users_table_pkey"] {
            assert!(relnames.contains(&format!("'{name}'")), "{name}");
        }
        for name in [
            "claims", "user_id", "user_email", "users_guard", "users_protect", "app_users_upsert", "app_users_gone",
            "app_users_rows", "assert_schema_version", "cap_response",
        ] {
            assert!(pronames.contains(&format!("'{name}'")), "{name}");
        }
        // The claim helpers come with every bootstrap, right after the owner's grants.
        let helpers = format!("GRANT SELECT ON chelabase.schema_version TO postgres;\n{}", crate::app_users_sql::claim_helpers_body());
        assert!(sql.contains(&helpers));
        assert!(data_api_sql_granting_existing("postgres", "pw", "public").contains(&helpers));
    }

    /// C1: the claim helpers (and the app users objects) go before `DROP SCHEMA … RESTRICT`,
    /// each function before the one it calls; `users_guard` is left to `RESTRICT`.
    #[test]
    fn remove_data_api_sql_drops_helpers_dependents_first() {
        let sql = REMOVE_DATA_API_SQL;
        let at = |s: &str| sql.find(s).unwrap_or_else(|| panic!("{s}"));
        let schema = at("DROP SCHEMA chelabase RESTRICT;");
        let (email, id, claims) = (
            at("DROP FUNCTION IF EXISTS chelabase.user_email();"),
            at("DROP FUNCTION IF EXISTS chelabase.user_id();"),
            at("DROP FUNCTION IF EXISTS chelabase.claims();"),
        );
        assert!(email < claims && id < claims && claims < schema);
        let protect = at("DROP EVENT TRIGGER IF EXISTS chelabase_users_protect_drop;");
        assert!(protect < at("DROP FUNCTION IF EXISTS chelabase.users_protect();"));
        assert!(at("DROP TABLE IF EXISTS chelabase.app_users_table;") < schema);
        assert!(at("DROP FUNCTION IF EXISTS chelabase.app_users_rows();") < schema);
        assert!(at("DROP FUNCTION IF EXISTS chelabase.assert_schema_version(uuid);") < schema);
        assert!(at("DROP FUNCTION IF EXISTS chelabase.cap_response(text, bigint);") < schema);
        assert!(!sql.contains("users_guard"), "left to RESTRICT");
        // Inside the ownership guard.
        assert!(at("r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN") < protect);
    }

    #[test]
    fn reserved_roles_include_the_sync_role() {
        assert!(RESERVED_ROLES.contains(&crate::app_users_sql::USERS_SYNC_ROLE));
        assert_eq!(
            refuse_by_name("chelabase_auth_sync"),
            Some(RoleCheck::Refused("the role chelabase_auth_sync is reserved".into()))
        );
    }

    /// The app users scripts start with their own marker line, so the data API script
    /// counters (and the journal's first-line entries) never take them for a bootstrap.
    #[tokio::test]
    async fn data_api_script_counters_skip_app_users_scripts() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        w.computes.sql.lock().unwrap().clear();
        for script in [
            crate::app_users_sql::users_bootstrap_sql("public", "users", "pw", "postgres"),
            crate::app_users_sql::USERS_TEARDOWN_SQL.to_string(),
            crate::app_users_sql::claim_helpers_sql(),
        ] {
            w.computes.sql.lock().unwrap().push((ep.clone(), "app".into(), script));
        }
        assert!(data_api_scripts(&w, &ep).is_empty());
        assert!(scripts_in(&w, &ep, "app").is_empty());
    }

    #[tokio::test]
    async fn rebootstrap_runs_the_script_with_the_stored_schema() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        add_row(&w, main.id, "analytics", DataApiState::On, "reporting");
        add_row(&w, main.id, "off", DataApiState::Off, "public");
        let password = authenticator_password(&w, &main);

        let reply = w.rebootstrap_data_api(&request(main.id, "analytics", false)).await;

        assert_eq!(reply, CheckDataRoleReply::ok());
        assert_eq!(scripts_in(&w, &ep, "analytics"), [data_api_sql("postgres", &password, "reporting")]);
        let reply = w.rebootstrap_data_api(&request(main.id, "off", false)).await;
        assert_eq!(reply.kind.as_deref(), Some("refused"));
        *w.computes.fail_sql_containing.lock().unwrap() = Some(("chelabase_admin".into(), "boom".into()));
        let reply = w.rebootstrap_data_api(&request(main.id, "analytics", false)).await;
        assert_eq!(reply.kind.as_deref(), Some("failed"));
    }

    #[tokio::test]
    async fn rebootstrap_grants_existing_only_when_asked() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        add_row(&w, main.id, "analytics", DataApiState::On, "reporting");
        let password = authenticator_password(&w, &main);

        // Without the flag: the first-grants statements stay behind `chelabase.bootstrap`.
        assert_eq!(w.rebootstrap_data_api(&request(main.id, "analytics", false)).await, CheckDataRoleReply::ok());
        // With it: the same script, but the first-grants run whether or not a first grant ran.
        let mut asked = request(main.id, "analytics", false);
        asked.grant_existing = true;
        assert_eq!(w.rebootstrap_data_api(&asked).await, CheckDataRoleReply::ok());

        let scripts = scripts_in(&w, &ep, "analytics");
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0], data_api_sql("postgres", &password, "reporting"));
        assert_eq!(scripts[1], data_api_sql_granting_existing("postgres", &password, "reporting"));
        let grants = "GRANT SELECT ON ALL TABLES IN SCHEMA \"reporting\" TO anon;";
        assert!(scripts[0].contains(grants) && scripts[1].contains(grants));
        let guard = "IF pg_catalog.to_regnamespace('\"reporting\"') IS NOT NULL AND NOT EXISTS (SELECT FROM chelabase.bootstrap) THEN";
        assert!(scripts[0].contains(guard) && !scripts[1].contains(guard));
        // Still guarded by the schema's existence, never revoking anything from a role, and
        // an existing bootstrap row isn't an error.
        assert!(scripts[1].contains("IF pg_catalog.to_regnamespace('\"reporting\"') IS NOT NULL THEN\n    GRANT SELECT ON ALL TABLES"));
        assert!(scripts[1].contains("INSERT INTO chelabase.bootstrap (existing_granted) VALUES (true) ON CONFLICT DO NOTHING;"));
        assert_eq!(scripts[1].matches("GRANT SELECT ON ALL TABLES").count(), 1);
        // Grants only (U2): the first-grants block of the granting variant has no REVOKE and
        // still carries the same GRANTs; the first enable's block keeps its revoke.
        let block = |s: &str| {
            let start = s.find("DO $$ DECLARE r record; BEGIN").unwrap();
            s[start..start + s[start..].find("END $$;").unwrap()].to_string()
        };
        let (first, asked) = (block(&scripts[0]), block(&scripts[1]));
        assert!(first.contains("REVOKE EXECUTE ON ROUTINE"));
        assert!(!asked.contains("REVOKE"), "{asked}");
        for grant in [
            "GRANT SELECT ON ALL TABLES IN SCHEMA \"reporting\" TO anon;",
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA \"reporting\" TO authenticated;",
            "GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA \"reporting\" TO authenticated;",
            "GRANT EXECUTE ON ROUTINE %s TO authenticated",
        ] {
            assert!(asked.contains(grant), "{grant}");
        }
    }

    #[tokio::test]
    async fn recreate_bootstraps_every_served_database() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        add_row(&w, main.id, "analytics", DataApiState::On, "reporting");
        add_row(&w, main.id, "off", DataApiState::Off, "public");
        add_row(&w, main.id, "broken", DataApiState::Enabling, "public");
        w.computes.remove(&ep);
        w.computes.sql.lock().unwrap().clear();
        let password = authenticator_password(&w, &main);

        w.recreate_compute(main.id, &ep).await.unwrap();

        assert_eq!(scripts_in(&w, &ep, "app"), [data_api_sql("postgres", &password, "public")]);
        assert_eq!(
            scripts_in(&w, &ep, "analytics"),
            [data_api_sql("postgres", &password, "reporting")]
        );
        assert!(scripts_in(&w, &ep, "off").is_empty() && scripts_in(&w, &ep, "broken").is_empty());

        // A second database that can't be bootstrapped doesn't fail the re-creation; main's can.
        w.computes.remove(&ep);
        w.computes.sql.lock().unwrap().clear();
        *w.computes.fail_sql_containing.lock().unwrap() = Some(("reporting".into(), "gone".into()));
        w.recreate_compute(main.id, &ep).await.unwrap();
        assert_eq!(scripts_in(&w, &ep, "app").len(), 1);
    }

    #[test]
    fn bootstrap_grants_the_owner_the_version_table() {
        let sql = data_api_sql("postgres", "pw", "public");
        let version = sql.find("GRANT SELECT ON chelabase.schema_version TO postgres;\n").expect("P5 table grant");
        let usage = sql.find("GRANT USAGE ON SCHEMA chelabase TO postgres;\n").expect("P5 schema grant");
        let created = sql.find("CREATE TABLE IF NOT EXISTS chelabase.schema_version").unwrap();
        assert!(created < usage && usage < version);
        // The schema is quoted wherever the script names it.
        let sql = data_api_sql("postgres", "pw", "my\"schema");
        assert!(sql.contains("IN SCHEMA \"my\"\"schema\" GRANT SELECT ON TABLES TO anon;"));
        assert!(sql.contains("GRANT USAGE ON SCHEMA \"my\"\"schema\" TO anon, authenticated, authenticator;"));
        // `regnamespace` and `to_regnamespace` parse identifier syntax: the literal is the quoted
        // identifier, so a name with a quote or capitals means the schema it names.
        assert!(sql.contains("'\"my\"\"schema\"'::pg_catalog.regnamespace"));
        assert!(sql.contains("pg_catalog.to_regnamespace('\"my\"\"schema\"') IS NOT NULL"));
        assert!(!sql.contains("SCHEMA public"));
    }

    /// Review Focus 5: a child of a parent that serves a second database serves it after
    /// creation, and the bootstrap ran there, after the flags were copied (once).
    #[tokio::test]
    async fn child_create_copies_flags_then_bootstraps() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        add_row(&w, main.id, "analytics", DataApiState::On, "reporting");
        add_row(&w, main.id, "pending", DataApiState::Enabling, "public");
        let d = |name: &str, system: bool| {
            format!("d|{}||{}|{}|||||||\n", hex::encode(name), hex::encode("postgres"), if system { "t" } else { "f" })
        };
        let catalog = [d("postgres", true), d("app", false), d("analytics", false), d("pending", false)].concat();
        w.computes.answer("FROM pg_authid", &catalog);
        journal.lock().unwrap().clear();

        let dev = ready_branch(&w, id, "dev", main.id).await;

        let ep = w.metadata.branch(dev).endpoint_id.unwrap();
        // Copied: `on` stays `on` with its schema; `enabling` becomes `off`.
        let r = row(&w, dev, "analytics");
        assert_eq!((r.state, r.schema.as_str()), (DataApiState::On, "reporting"));
        assert_eq!(row(&w, dev, "pending").state, DataApiState::Off);
        assert_eq!(count(&journal, "metadata.copy_parent_database_flags"), 1);
        let password = authenticator_password(&w, &w.metadata.branch(dev));
        assert_eq!(
            scripts_in(&w, &ep, "analytics"),
            [data_api_sql("postgres", &password, "reporting")]
        );
        assert!(scripts_in(&w, &ep, "pending").is_empty());
        // Order: the resync wrote the rows, the flags were copied, then the bootstrap ran.
        let resync = pos(&journal, "metadata.apply_resync");
        let copy = pos(&journal, "metadata.copy_parent_database_flags");
        let boot = pos(&journal, &format!("compute.sql {ep} analytics {DATA_API}"));
        assert!(resync < copy && copy < boot, "{:?}", entries(&journal));
        // Main is bootstrapped once, not again after the copy (its schema didn't change).
        assert_eq!(scripts_in(&w, &ep, "app").len(), 1);
    }

    #[test]
    fn bootstrap_skips_a_missing_default_schema() {
        let sql = data_api_sql("postgres", "pw", "reporting");
        let guard = "pg_catalog.to_regnamespace('\"reporting\"') IS NOT NULL";
        // Every statement that names the schema sits inside a guard, so a schema that's gone
        // (a restore to before it existed, a DROP SCHEMA) doesn't fail the bootstrap.
        let first = sql.find(&format!("  IF {guard} THEN\n    ALTER DEFAULT PRIVILEGES")).expect("guarded defaults");
        let usage = sql.find("GRANT USAGE ON SCHEMA \"reporting\" TO anon, authenticated, authenticator;").unwrap();
        // The asked-for rebootstrap grants it too (it's what a branch bootstrapped before
        // authenticator needed it gets on its next run).
        let existing = data_api_sql_granting_existing("postgres", "pw", "reporting");
        assert!(existing.contains("GRANT USAGE ON SCHEMA \"reporting\" TO anon, authenticated, authenticator;"));
        let end = first + sql[first..].find("  END IF;").unwrap();
        assert!(first < usage && usage < end);
        for statement in sql.match_indices("IN SCHEMA \"reporting\"").map(|(i, _)| i) {
            assert!(statement > first, "{statement}");
        }
        assert!(sql.contains(&format!("  IF {guard} AND NOT EXISTS (SELECT FROM chelabase.bootstrap) THEN")));
        // The schema-free default privilege stays outside the guard.
        let revoke = sql.find("REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;").unwrap();
        assert!(revoke < first);
        // The machinery itself is not guarded, and the marker comes before the last line.
        assert!(sql.find("CREATE SCHEMA IF NOT EXISTS chelabase").unwrap() < first);
        let marker = sql.find("SELECT 'chelabase_missing_schema' WHERE pg_catalog.to_regnamespace('\"reporting\"') IS NULL;\n").unwrap();
        assert!(marker < sql.find("current_setting('chelabase.quarantined'").unwrap());
        assert!(super::missing_schema("chelabase_missing_schema\n\n"));
        assert!(super::missing_schema("chelabase_missing_schema\nchelabase_quarantine_1\n"));
        assert!(!super::missing_schema("\n"));
        assert_eq!(quarantined("chelabase_missing_schema\nchelabase_quarantine_1\n"), Some("chelabase_quarantine_1"));
    }

    #[tokio::test]
    async fn a_child_whose_main_names_a_missing_schema_still_becomes_ready() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.metadata.state().db_rows.get_mut(&main.id).unwrap()[0].schema = "gone".to_string();
        // The script reports that it skipped the schema's statements.
        w.computes.answer("chelabase.quarantined", "chelabase_missing_schema\n");

        let dev = ready_branch(&w, id, "dev", main.id).await;

        assert_eq!(w.metadata.branch(dev).status, BranchStatus::Ready);
        assert_eq!(row(&w, dev, "app").schema, "gone");
        let ep = w.metadata.branch(dev).endpoint_id.unwrap();
        // Main is bootstrapped again with the copied schema (the early run used `public`).
        let scripts = scripts_in(&w, &ep, "app");
        assert!(scripts.last().unwrap().contains("to_regnamespace('\"gone\"')"), "{}", scripts.len());
    }
}
