//! Edge functions m8: database-event triggers in a branch's databases.
//!
//! **Invariant.** No object a database member can create or alter ever runs with the rights
//! of [`OWNER`] or of the worker's superuser connection, and the install never adopts an
//! object it didn't create. The attack matrix in `tests/fn_triggers_live.rs`
//! (`attack_matrix_keeps_the_invariant`) checks it move by move.
//!
//! **Invariant (run-as).** A minted run-as key is always bound to the role OID a member
//! approved, and the worker never overrides a member's run-as choice. A child copies the
//! approved OID (`run_as_role_oid`) at its create. [`Worker::mint_child_run_as`] mints only for
//! a role of that very OID: a missing role, or a namesake with another OID, is a terminal
//! `run_as_unmintable` the sweep never retries. It writes only through
//! `MetadataClient::settle_run_as_mint`, which checks, under the function's locks, that the
//! member's choice is still the one checked.
//! `audit_run_as_mint_never_overrides_a_concurrent_change` and
//! `audit_child_run_as_key_needs_the_approved_oid` test it.
//!
//! [`Worker::sync_fn_triggers`] keeps a branch's databases in step with its table triggers
//! (`function_triggers`, source `table`), as the superuser (`cloud_admin`):
//!
//! - **The role**: [`OWNER`], made once per compute by [`role_sql`] (NOLOGIN NOINHERIT
//!   CONNECTION LIMIT 0, by the superuser, so it never has members). Every check and install
//!   that follows requires that very role.
//! - **The foreign check** ([`check_sql`], in every database of the compute, before anything
//!   is installed anywhere, and again at the top of each install): the role and the schema
//!   `chelabase_functions` must be exactly what the install makes. That means no attribute,
//!   membership or setting on the role; nothing it owns or holds rights on outside the schema
//!   (`pg_shdepend`: shared objects, and each database's own). A grant *to* the role on a
//!   table, routine, schema, type or database is let through and revoked: nobody can act as
//!   the role, and `enqueue()` reads nothing. The schema must be its own, with nothing in
//!   the schema but the outbox, its sequence, index and row type, and `enqueue()`. Anything
//!   else is refused with [`FOREIGN_MARK`], never repaired. Every `chelabase_fn_*` trigger
//!   running `enqueue()` is dropped ([`drop_triggers_sql`]), and every row read settles
//!   `failed` (terminal until the trigger is changed or pended again). A `failed` row is never
//!   wanted, so its database trigger is dropped, never installed.
//! - **The machinery**, made or put back each run while the database has a trigger to install:
//!   the schema (no rights for PUBLIC, so no data API role reaches it), the outbox
//!   `chelabase_functions.outbox` and the trigger function `chelabase_functions.enqueue()`,
//!   all [`OWNER`]'s. Every grant on them to anyone but the owner is revoked on every run.
//!   The function is `SECURITY DEFINER` with `search_path = pg_catalog, chelabase_functions,
//!   pg_temp` and qualifies every name, so a user's `public.pg_notify` never runs in it. It
//!   evaluates no user code at all: a built-in-typed column goes through `to_jsonb`, and any
//!   other value is its type output text, so no user cast ever runs as [`OWNER`]
//!   ([`install_objects_sql`]). It inserts the row's event (the trigger id from its argument,
//!   the operation, schema, table, new and old rows, and the Neon timeline) and notifies
//!   [`NOTIFY_CHANNEL`]. Owner-level roles (`neon_superuser` holds `pg_read_all_data` and
//!   `pg_write_all_data`) can still read and write the outbox directly, so a consumer must
//!   check each row's trigger id and table against the metadata (m13, #175).
//! - **The repair**, before anything runs on the outbox: an outbox of another shape (columns,
//!   defaults, triggers, rules, policies, row level security, extra constraints or indexes,
//!   owner) is dropped and rebuilt ([`repair_sql`]).
//! - **The outbox**: truncated after a branch create, reset or restore (the database came from
//!   another timeline) while the rows read are still pending and, decided after the database's
//!   advisory lock, no wanted trigger is in place yet (a sweep that got there first cleaned
//!   it). On every run, its rows from
//!   another timeline or for a trigger the database doesn't want are deleted. The cleanup runs
//!   as [`OWNER`] (`SET LOCAL ROLE`), after the repair, never as the superuser.
//! - **The triggers**: `chelabase_fn_<id as 32 hex>` ([`trigger_name`]), `AFTER <events> ON
//!   <schema>.<table> FOR EACH ROW EXECUTE FUNCTION chelabase_functions.enqueue('<id>')`, on
//!   plain tables only. A partitioned table's row triggers would be cloned onto every new
//!   partition with the rights of whoever adds it, and only [`OWNER`] may execute `enqueue()`;
//!   such a row settles `failed` with [`PLAIN_TABLES_ONLY`]. A trigger is dropped when it runs
//!   a function of `chelabase_functions`, or has a wanted name, and isn't exactly a wanted
//!   one (name, plain table, function, events, argument, enabled, no column list or `WHEN`).
//!   Then each wanted one whose table exists is created. Names reach the SQL only as literals,
//!   and the statements are built by `format('%I')`.
//! - Event triggers are off for every transaction (`SET LOCAL event_triggers = off`), so no
//!   tenant code runs as the superuser.
//!
//! The script answers one line per wanted trigger: `fn_trigger <id> ok`, `… table_missing` or
//! `… not_a_plain_table`. The worker settles the rows it read, by the `enabled_since` it read
//! (`MetadataClient::settle_trigger_installs`): only `installed` rows are the dispatcher's
//! (m13).
//!
//! When: after a branch create (the child's rows are copies, `pending_install`, and its
//! database still has the parent's triggers and outbox); after a reset or restore (the switch
//! pended every table trigger of the branch); and on each reconcile pass
//! ([`Worker::sync_pending_fn_triggers`]). That pass covers branches with a pending row, and
//! branches whose installed triggers are checked for drift (dropped, disabled or changed: put
//! back), up to [`SWEEP_BATCH`], least recently attempted first. A branch create also mints the
//! child's run-as keys ([`Worker::mint_child_run_as`]); the reconcile pass retries any still
//! owed ([`Worker::sync_pending_run_as`], chosen by state, not by `last_error`).

use chelabase_metadata::{
    BranchRecord, BranchStatus, DatabaseOrigin, MetadataClient, RunAs, RunAsMint, RunAsUnmintable,
    TriggerInstall, TriggerRecord, TriggerSource, TriggerState,
};
use chelabase_nats::{AllRowsCheckReply, AllRowsCheckRequest};
use chelabase_neon::ComputeBackend;
use chelabase_schema::exclusions::{FUNCTION_TRIGGER_PREFIX, FUNCTIONS_SCHEMA};
use chelabase_schema::sql::literal;
use chelabase_types::BranchId;
use chelabase_types::data_key::{self, DataKeyKind};
use tracing::{info, warn};
use uuid::Uuid;

use crate::Worker;
use crate::data_api::{LOCK_TIMEOUT_SECS, RoleCheck};
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::resync::SYSTEM_DATABASE;
use crate::review::review_databases_sql;

/// The NOLOGIN role owning the schema, the outbox and the trigger function.
pub const OWNER: &str = "chelabase_functions_owner";

/// The channel `enqueue()` notifies after each insert (the dispatcher listens, m13).
pub const NOTIFY_CHANNEL: &str = "chelabase_functions";

/// The script's first line (the fakes' needle).
pub const SYNC_MARK: &str = "-- chelabase: function triggers";

/// The error the install raises when `chelabase_functions` isn't [`OWNER`]'s.
pub const FOREIGN_MARK: &str = "chelabase_functions_foreign";

/// The transaction-scoped advisory lock two syncs of one database serialize on: the bytes of
/// "chelabase" followed by 0x08 (the bootstrap's is 0x01).
pub const SYNC_LOCK_KEY: i64 = 0x6368_656c_6164_6208;

/// `last_error` on a child's function whose run-as role its catalog doesn't have.
pub const RUN_AS_ROLE_MISSING: &str = "run_as_role_missing";

/// `last_error` prefix on a child's function whose run-as role name now belongs to another role
/// than the one a member approved (another OID), or with no approved OID.
pub const RUN_AS_ROLE_CHANGED: &str = "run_as_role_changed";

/// `last_error` prefix on a child's function whose run-as role the checks refuse.
pub const RUN_AS_ROLE_REFUSED: &str = "run_as_role_refused";

/// `last_error` prefix on a child's function whose run-as key couldn't be minted yet (the
/// role or "all rows" check couldn't run); the reconcile sweep retries it.
pub const RUN_AS_KEY_PENDING: &str = "run_as_key_pending";

/// `last_error` on a trigger whose table isn't a plain table (partitioned tables, views, …):
/// a partitioned table's row triggers are cloned onto each new partition with the rights of
/// whoever adds it, and nobody but [`OWNER`] may execute `enqueue()`.
pub const PLAIN_TABLES_ONLY: &str = "function triggers support plain tables only";

/// The stored error for a script answer.
pub fn install_error(answer: &str) -> String {
    match answer {
        "not_a_plain_table" => PLAIN_TABLES_ONLY.to_string(),
        other => other.to_string(),
    }
}

/// The settings every script starts with.
const PRELUDE: &str =
    "BEGIN;\nSET LOCAL search_path = pg_catalog, pg_temp;\nSET LOCAL event_triggers = off;\n";

/// Run once per sync in the compute's system database, before any check: makes [`OWNER`]
/// when `create` and it's missing (`NOLOGIN NOINHERIT CONNECTION LIMIT 0`, by the superuser,
/// so nobody else is ever its member), and answers `fn_owner <oid>` when it exists. The
/// checks and installs that follow require that very OID: a role made by anyone in between is
/// refused.
pub fn role_sql(create: bool) -> String {
    let create = if create {
        format!(
            "DO $chelabase_fn$ BEGIN\n  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = '{OWNER}') THEN\n    \
             CREATE ROLE {OWNER} NOLOGIN NOINHERIT CONNECTION LIMIT 0;\n  END IF;\nEND $chelabase_fn$;\n"
        )
    } else {
        String::new()
    };
    format!(
        "{SYNC_MARK} role\n{PRELUDE}{create}SELECT 'fn_owner ' || oid FROM pg_catalog.pg_roles WHERE rolname = '{OWNER}';\nCOMMIT;\n"
    )
}

/// The answer of [`role_sql`].
pub fn parse_role(out: &str) -> Option<u32> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("fn_owner ")?.parse().ok())
}

/// The foreign check (the invariant's "never adopts"), for the database it runs in: raises
/// [`FOREIGN_MARK`] with the reason when [`OWNER`] isn't the role `role` names (made by
/// [`role_sql`]), when the schema isn't its, when the role has any attribute, membership,
/// setting, ownership or right beyond what the install gives it (`pg_shdepend`: shared
/// objects, and this database's objects outside the schema; the worker runs the check in
/// every database of the compute), or when the schema holds anything the install doesn't
/// make. Refused, never repaired.
fn foreign_check_sql(role: Option<u32>) -> String {
    let expected = role.map_or_else(|| "NULL".to_string(), |r| r.to_string());
    format!(
        r#"DO $chelabase_fn$ DECLARE
  expected pg_catalog.oid := {expected};
  role pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = '{OWNER}');
  ns pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = '{FUNCTIONS_SCHEMA}');
  box pg_catalog.oid;
  ours pg_catalog.oid[];
  why pg_catalog.text;
BEGIN
  box := (SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relnamespace = ns AND c.relname = 'outbox');
  ours := ARRAY(SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relnamespace = ns
      AND ((c.relname = 'outbox' AND c.relkind = 'r') OR (c.relname = 'outbox_id_seq' AND c.relkind = 'S')
        OR (c.relname = 'outbox_pkey' AND c.relkind = 'i'))
    UNION SELECT c.reltoastrelid FROM pg_catalog.pg_class c WHERE c.oid = box AND c.reltoastrelid <> 0
    UNION SELECT i.indexrelid FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.reltoastrelid = i.indrelid WHERE c.oid = box);
  IF role IS DISTINCT FROM expected THEN
    why := 'the role {OWNER} was made by someone else';
  ELSIF ns IS NOT NULL AND (role IS NULL OR (SELECT nspowner FROM pg_catalog.pg_namespace WHERE oid = ns) <> role) THEN
    why := 'the schema {FUNCTIONS_SCHEMA} is not {OWNER}''s';
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_authid a WHERE a.oid = role AND (a.rolsuper OR a.rolinherit OR a.rolcreaterole
      OR a.rolcreatedb OR a.rolcanlogin OR a.rolreplication OR a.rolbypassrls OR a.rolconnlimit <> 0
      OR a.rolpassword IS NOT NULL OR a.rolvaliduntil IS NOT NULL)) THEN
    why := 'the role {OWNER} has attributes Chelabase never gives it';
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_auth_members m WHERE m.roleid = role OR m.member = role) THEN
    why := 'the role {OWNER} has members or memberships';
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_db_role_setting s WHERE s.setrole = role) THEN
    why := 'the role {OWNER} has settings';
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_shdepend d WHERE d.refclassid = 'pg_catalog.pg_authid'::pg_catalog.regclass AND d.refobjid = role
      AND NOT (d.deptype = 'a' AND d.classid IN ('pg_catalog.pg_class'::pg_catalog.regclass, 'pg_catalog.pg_proc'::pg_catalog.regclass,
        'pg_catalog.pg_namespace'::pg_catalog.regclass, 'pg_catalog.pg_type'::pg_catalog.regclass, 'pg_catalog.pg_database'::pg_catalog.regclass))
      AND (d.dbid = 0 OR (d.dbid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database())
        AND NOT (d.deptype = 'o' AND (
          (d.classid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND d.objid = ns)
          OR (d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.objid = ANY (ours))
          OR (d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
            AND d.objid = (SELECT p.oid FROM pg_catalog.pg_proc p WHERE p.pronamespace = ns AND p.proname = 'enqueue' AND p.pronargs = 0))))))) THEN
    why := 'the role {OWNER} owns or holds rights on objects outside {FUNCTIONS_SCHEMA}';
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_depend d WHERE d.refclassid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND d.refobjid = ns
      AND NOT ((d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
          AND d.objid IN (SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relnamespace = ns AND c.relname IN ('outbox', 'outbox_id_seq', 'outbox_pkey')))
        OR (d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
          AND d.objid IN (SELECT p.oid FROM pg_catalog.pg_proc p WHERE p.pronamespace = ns AND p.proname = 'enqueue' AND p.pronargs = 0))
        OR (d.classid = 'pg_catalog.pg_type'::pg_catalog.regclass
          AND d.objid IN (SELECT t.oid FROM pg_catalog.pg_type t WHERE t.typnamespace = ns AND t.typname IN ('outbox', '_outbox'))))) THEN
    why := 'the schema {FUNCTIONS_SCHEMA} holds objects Chelabase didn''t make';
  END IF;
  IF why IS NOT NULL THEN
    RAISE EXCEPTION '{FOREIGN_MARK}: %; nothing was installed', why;
  END IF;
END $chelabase_fn$;
"#
    )
}

/// The foreign check alone, read-only, for one database (the worker runs it in every
/// database of the compute before any install).
pub fn check_sql(role: Option<u32>) -> String {
    format!(
        "{SYNC_MARK} check\n{PRELUDE}{}ROLLBACK;\n",
        foreign_check_sql(role)
    )
}

/// On a refusal: drops every `chelabase_fn_*` trigger running `chelabase_functions.enqueue()`
/// in the database, so a refused branch's triggers don't keep filling an outbox nobody reads.
/// A plain `DROP TRIGGER` as the superuser with event triggers off: nothing is adopted and no
/// tenant code runs.
pub fn drop_triggers_sql() -> String {
    format!(
        r#"{SYNC_MARK} drop
{PRELUDE}DO $chelabase_fn$ DECLARE
  t record;
BEGIN
  FOR t IN SELECT tg.tgname, n.nspname, c.relname FROM pg_catalog.pg_trigger tg
    JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_catalog.pg_proc p ON p.oid = tg.tgfoid JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace
    WHERE NOT tg.tgisinternal AND tg.tgparentid = 0
      AND pg_catalog.starts_with(tg.tgname::pg_catalog.text, '{FUNCTION_TRIGGER_PREFIX}')
      AND pn.nspname = '{FUNCTIONS_SCHEMA}' AND p.proname = 'enqueue'
  LOOP
    EXECUTE pg_catalog.format('DROP TRIGGER %I ON %I.%I', t.tgname, t.nspname, t.relname);
  END LOOP;
END $chelabase_fn$;
COMMIT;
"#
    )
}

/// The refusal in a script's error, from [`FOREIGN_MARK`] on (the row's `last_error`).
pub fn foreign_refusal(error: &str) -> Option<String> {
    let at = error.find(FOREIGN_MARK)?;
    let rest = &error[at..];
    Some(rest.lines().next().unwrap_or(rest).trim().to_string())
}

/// Branches the reconcile sweep syncs per pass.
const SWEEP_BATCH: i64 = 20;

/// The most candidates a sweep reads per pass (it takes [`SWEEP_BATCH`] of them).
const SWEEP_CANDIDATES: i64 = 1000;

/// The two reconcile sweeps, each with its own attempt times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sweep {
    Triggers,
    RunAs,
}

/// When each branch was last attempted, per sweep (this worker's memory: a restart starts
/// with every branch unattempted).
pub type SweepAttempts =
    std::collections::HashMap<Sweep, std::collections::HashMap<BranchId, std::time::Instant>>;

/// A table trigger to install: its row's id, table and events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub id: Uuid,
    pub schema: String,
    pub table: String,
    pub events: Vec<String>,
}

/// The trigger's name in the database.
pub fn trigger_name(id: Uuid) -> String {
    format!("{FUNCTION_TRIGGER_PREFIX}{}", id.simple())
}

/// `(the CREATE TRIGGER events, pg_trigger.tgtype)` for row-level AFTER triggers on
/// `insert`/`update`/`delete`; `None` for no event, a repeat or anything else.
pub fn trigger_events(events: &[String]) -> Option<(String, i16)> {
    let mut words = Vec::new();
    let mut tgtype = 1; // TRIGGER_TYPE_ROW; AFTER sets no bit.
    for (word, bit) in [("insert", 4), ("delete", 8), ("update", 16)] {
        match events.iter().filter(|e| e.as_str() == word).count() {
            0 => {}
            1 => {
                words.push(word.to_ascii_uppercase());
                tgtype |= bit;
            }
            _ => return None,
        }
    }
    (!words.is_empty() && words.len() == events.len()).then(|| (words.join(" OR "), tgtype))
}

/// The table trigger rows a database wants, from `triggers` (any source): the wanted list.
pub fn wanted_in(triggers: &[TriggerRecord], db: &str) -> Vec<Wanted> {
    triggers
        .iter()
        // A failed row is never wanted: its database trigger is dropped, never installed.
        .filter(|t| {
            t.source == TriggerSource::Table
                && t.state != TriggerState::Failed
                && t.pg_database == db
        })
        .filter_map(|t| {
            Some(Wanted {
                id: t.id,
                schema: t.schema_name.clone()?,
                table: t.table_name.clone()?,
                events: t.events.clone(),
            })
        })
        .collect()
}

/// The predicate "trigger `tg` on `c` in `n` is exactly the wanted `w`", for the script.
const EXACT: &str = "w.name = tg.tgname AND w.schema_name = n.nspname AND w.table_name = c.relname
      AND tg.tgfoid = pg_catalog.to_regprocedure('chelabase_functions.enqueue()')
      AND tg.tgtype = w.tgtype AND tg.tgenabled = 'O' AND tg.tgnargs = 1
      AND tg.tgargs = pg_catalog.convert_to(w.trigger_id::pg_catalog.text, 'UTF8') || '\\x00'::pg_catalog.bytea
      AND tg.tgqual IS NULL AND pg_catalog.cardinality(tg.tgattr::pg_catalog.int2[]) = 0
      AND tg.tgconstraint = 0 AND NOT tg.tgisinternal AND tg.tgparentid = 0 AND c.relkind = 'r'";

/// The outbox's columns as the repair check reads them (name, type, NOT NULL, default as
/// `pg_get_expr` prints it under the script's `search_path`). Any other shape is rebuilt.
pub const OUTBOX_SHAPE: &str = "id bigint nn = nextval('chelabase_functions.outbox_id_seq'::regclass),\
trigger_id uuid nn,payload jsonb nn,attempts integer nn = 0,\
next_attempt_at timestamp with time zone nn = now(),status text nn = 'pending'::text,\
timeline_id text,created_at timestamp with time zone nn = now()";

/// `ours`: the `chelabase_functions` schema's OID when [`OWNER`] owns it, else NULL.
fn ours_sql() -> String {
    format!(
        "(SELECT n.oid FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner
    WHERE n.nspname = '{FUNCTIONS_SCHEMA}' AND r.rolname = '{OWNER}')"
    )
}

/// `DROP <kind>` for a `pg_class.relkind` (the SQL `CASE`).
const DROP_KIND: &str = "CASE t.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' \
WHEN 'S' THEN 'SEQUENCE' WHEN 'f' THEN 'FOREIGN TABLE' WHEN 'i' THEN 'INDEX' WHEN 'I' THEN 'INDEX' \
WHEN 'c' THEN 'TYPE' ELSE 'TABLE' END";

/// After the foreign check, before anything else runs on the schema: an outbox of any other
/// shape (kind, owner, columns, defaults, row level security, triggers, rules, policies,
/// inheritance, constraints or indexes beyond its primary key) is dropped (a DROP runs none of
/// them) and made again. As the superuser: it only reads catalogs and drops.
fn repair_sql() -> String {
    let ours = ours_sql();
    let shape = literal(OUTBOX_SHAPE);
    format!(
        r#"DO $chelabase_fn$ DECLARE
  ours pg_catalog.oid := {ours};
  box pg_catalog.oid;
  t record;
BEGIN
  IF ours IS NULL THEN
    RETURN;
  END IF;
  box := (SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relnamespace = ours AND c.relname = 'outbox');
  IF box IS NOT NULL AND (
      EXISTS (SELECT FROM pg_catalog.pg_class c WHERE c.oid = box AND (c.relkind <> 'r' OR c.relrowsecurity OR c.relforcerowsecurity
        OR c.relowner <> (SELECT n.nspowner FROM pg_catalog.pg_namespace n WHERE n.oid = ours)))
      OR (SELECT pg_catalog.string_agg(a.attname || ' ' || pg_catalog.format_type(a.atttypid, a.atttypmod)
            || CASE WHEN a.attnotnull THEN ' nn' ELSE '' END || CASE WHEN a.attgenerated <> '' THEN ' generated' ELSE '' END
            || coalesce(' = ' || pg_catalog.pg_get_expr(d.adbin, d.adrelid), ''), ',' ORDER BY a.attnum)
          FROM pg_catalog.pg_attribute a LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
          WHERE a.attrelid = box AND a.attnum > 0 AND NOT a.attisdropped) IS DISTINCT FROM {shape}
      OR EXISTS (SELECT FROM pg_catalog.pg_trigger WHERE tgrelid = box)
      OR EXISTS (SELECT FROM pg_catalog.pg_rewrite WHERE ev_class = box)
      OR EXISTS (SELECT FROM pg_catalog.pg_policy WHERE polrelid = box)
      OR EXISTS (SELECT FROM pg_catalog.pg_inherits WHERE inhrelid = box OR inhparent = box)
      OR (SELECT pg_catalog.count(*) FROM pg_catalog.pg_constraint WHERE conrelid = box AND contype <> 'n') <> 1
      OR (SELECT pg_catalog.count(*) FROM pg_catalog.pg_index WHERE indrelid = box) <> 1) THEN
    RAISE WARNING 'chelabase: {FUNCTIONS_SCHEMA}.outbox had been changed; it was rebuilt and its events dropped';
    FOR t IN SELECT c.relkind FROM pg_catalog.pg_class c WHERE c.oid = box LOOP
      EXECUTE 'DROP ' || {DROP_KIND} || ' {FUNCTIONS_SCHEMA}.outbox CASCADE';
    END LOOP;
  END IF;
END $chelabase_fn$;
"#
    )
}

/// The schema, the outbox and the trigger function (install only).
///
/// `enqueue()` is `SECURITY DEFINER` (only it writes the outbox) and evaluates no user code:
/// a column goes through `to_jsonb` only when its type (or a domain's base type) is built in
/// (`to_jsonb` looks for a user cast to json only for types with OIDs from 16384 on, and
/// recurses into built-in arrays and composites only); any other value (an enum, a
/// composite, an array of a user type, an extension's type) is its type output text
/// (`format('%s')` calls the output function, never a cast, and only a superuser can define a
/// base type's output function; enums, records, arrays, ranges and domains print through
/// built-in ones; reading a value checks no domain constraint). The column list comes from the
/// catalog and is quoted by `%I`; every function and operator in the generated SELECT is
/// `pg_catalog`'s.
fn install_objects_sql() -> String {
    format!(
        r#"CREATE SCHEMA IF NOT EXISTS {FUNCTIONS_SCHEMA} AUTHORIZATION {OWNER};
REVOKE ALL ON SCHEMA {FUNCTIONS_SCHEMA} FROM PUBLIC;
CREATE TABLE IF NOT EXISTS {FUNCTIONS_SCHEMA}.outbox (
  id bigserial PRIMARY KEY,
  trigger_id uuid NOT NULL,
  payload jsonb NOT NULL,
  attempts integer NOT NULL DEFAULT 0,
  next_attempt_at timestamptz NOT NULL DEFAULT pg_catalog.now(),
  status text NOT NULL DEFAULT 'pending',
  timeline_id text,
  created_at timestamptz NOT NULL DEFAULT pg_catalog.now()
);
ALTER TABLE {FUNCTIONS_SCHEMA}.outbox OWNER TO {OWNER};
REVOKE ALL ON TABLE {FUNCTIONS_SCHEMA}.outbox FROM PUBLIC;
REVOKE ALL ON SEQUENCE {FUNCTIONS_SCHEMA}.outbox_id_seq FROM PUBLIC;
CREATE OR REPLACE FUNCTION {FUNCTIONS_SCHEMA}.enqueue() RETURNS trigger
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, {FUNCTIONS_SCHEMA}, pg_temp AS $chelabase_fn$
DECLARE
  expr pg_catalog.text;
  rec pg_catalog.jsonb;
  old_rec pg_catalog.jsonb;
BEGIN
  SELECT 'SELECT ' || coalesce(pg_catalog.string_agg(
      CASE WHEN (CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END) < 16384::pg_catalog.oid
        THEN pg_catalog.format('pg_catalog.jsonb_build_object(%L::pg_catalog.text, pg_catalog.to_jsonb(($1).%I))', a.attname, a.attname)
        ELSE pg_catalog.format('pg_catalog.jsonb_build_object(%L::pg_catalog.text, CASE WHEN pg_catalog.num_nulls(($1).%I) OPERATOR(pg_catalog.=) 0 THEN pg_catalog.to_jsonb(pg_catalog.format(''%%s'', ($1).%I)) END)',
          a.attname, a.attname, a.attname)
      END, ' OPERATOR(pg_catalog.||) ' ORDER BY a.attnum), '''{{}}''::pg_catalog.jsonb')
    INTO expr
    FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
    WHERE a.attrelid = TG_RELID AND a.attnum > 0 AND NOT a.attisdropped;
  IF TG_OP <> 'DELETE' THEN
    EXECUTE expr INTO rec USING NEW;
  END IF;
  IF TG_OP <> 'INSERT' THEN
    EXECUTE expr INTO old_rec USING OLD;
  END IF;
  INSERT INTO {FUNCTIONS_SCHEMA}.outbox (trigger_id, payload, timeline_id)
  VALUES (TG_ARGV[0]::pg_catalog.uuid,
    pg_catalog.jsonb_build_object('type', TG_OP, 'schema', TG_TABLE_SCHEMA, 'table', TG_TABLE_NAME,
      'record', rec, 'old_record', old_rec),
    pg_catalog.current_setting('neon.timeline_id', true));
  PERFORM pg_catalog.pg_notify('{NOTIFY_CHANNEL}', '');
  RETURN NULL;
END $chelabase_fn$;
ALTER FUNCTION {FUNCTIONS_SCHEMA}.enqueue() OWNER TO {OWNER};
REVOKE ALL ON FUNCTION {FUNCTIONS_SCHEMA}.enqueue() FROM PUBLIC;
"#
    )
}

/// Every grant on the schema, its tables and sequences and its routines to anyone but the
/// object's owner is revoked, `CASCADE` (as the superuser, the revoke acts for the owner, so
/// every grant chain from the owner goes; the owner is the only one who granted first).
fn revoke_sql() -> String {
    let ours = ours_sql();
    format!(
        r#"DO $chelabase_fn$ DECLARE
  ours pg_catalog.oid := {ours};
  g record;
BEGIN
  IF ours IS NULL THEN
    RETURN;
  END IF;
  FOR g IN
    SELECT 'SCHEMA {FUNCTIONS_SCHEMA}' AS obj, a.grantee
      FROM pg_catalog.pg_namespace n CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) a
      WHERE n.oid = ours AND a.grantee <> n.nspowner
    UNION
    SELECT CASE c.relkind WHEN 'S' THEN 'SEQUENCE ' ELSE 'TABLE ' END || c.oid::pg_catalog.regclass::pg_catalog.text, a.grantee
      FROM pg_catalog.pg_class c CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) a
      WHERE c.relnamespace = ours AND c.relkind IN ('r', 'S') AND a.grantee <> c.relowner
    UNION
    SELECT 'ROUTINE ' || p.oid::pg_catalog.regprocedure::pg_catalog.text, a.grantee
      FROM pg_catalog.pg_proc p CROSS JOIN LATERAL pg_catalog.aclexplode(p.proacl) a
      WHERE p.pronamespace = ours AND a.grantee <> p.proowner
  LOOP
    EXECUTE pg_catalog.format('REVOKE ALL ON %s FROM %s CASCADE', g.obj,
      CASE WHEN g.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.quote_ident(pg_catalog.pg_get_userbyid(g.grantee)) END);
  END LOOP;
  -- Grants *to* the owner role on other objects (a table owner's, on their own table): the
  -- foreign check lets them pass (nobody can act as the role, and enqueue() reads no
  -- table), and they are revoked here.
  FOR g IN SELECT d.classid, d.objid, d.objsubid FROM pg_catalog.pg_shdepend d
    WHERE d.refclassid = 'pg_catalog.pg_authid'::pg_catalog.regclass
      AND d.refobjid = (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = '{OWNER}') AND d.deptype = 'a'
      AND (d.dbid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database())
        OR (d.dbid = 0 AND d.classid = 'pg_catalog.pg_database'::pg_catalog.regclass))
  LOOP
    IF g.classid = 'pg_catalog.pg_class'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL %s ON %s %s FROM {OWNER} CASCADE',
        CASE WHEN g.objsubid = 0 THEN '' ELSE pg_catalog.format('(%I)', (SELECT a.attname FROM pg_catalog.pg_attribute a
          WHERE a.attrelid = g.objid AND a.attnum = g.objsubid)) END,
        CASE WHEN (SELECT c.relkind FROM pg_catalog.pg_class c WHERE c.oid = g.objid) = 'S' THEN 'SEQUENCE' ELSE 'TABLE' END,
        g.objid::pg_catalog.regclass);
    ELSIF g.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON ROUTINE %s FROM {OWNER} CASCADE', g.objid::pg_catalog.regprocedure);
    ELSIF g.classid = 'pg_catalog.pg_namespace'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON SCHEMA %I FROM {OWNER} CASCADE', (SELECT n.nspname FROM pg_catalog.pg_namespace n WHERE n.oid = g.objid));
    ELSIF g.classid = 'pg_catalog.pg_type'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON %s %s FROM {OWNER} CASCADE',
        CASE WHEN (SELECT t.typtype FROM pg_catalog.pg_type t WHERE t.oid = g.objid) = 'd' THEN 'DOMAIN' ELSE 'TYPE' END,
        g.objid::pg_catalog.regtype);
    ELSIF g.classid = 'pg_catalog.pg_database'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON DATABASE %I FROM {OWNER} CASCADE', (SELECT d.datname FROM pg_catalog.pg_database d WHERE d.oid = g.objid));
    END IF;
  END LOOP;
END $chelabase_fn$;
"#
    )
}

/// The script that brings one database in step with `wanted` (see the module docs);
/// `truncate` empties the outbox first. Without wanted triggers it installs nothing, and only
/// repairs, drops leftovers and cleans the outbox of a schema [`OWNER`] owns.
pub fn sync_sql(wanted: &[Wanted], truncate: bool, role: Option<u32>) -> String {
    let rows: Vec<String> = wanted
        .iter()
        .filter_map(|w| {
            let (events, tgtype) = trigger_events(&w.events)?;
            Some(format!(
                "({}, {}, {}, {}, {}, {tgtype})",
                literal(&trigger_name(w.id)),
                literal(&w.id.to_string()),
                literal(&w.schema),
                literal(&w.table),
                literal(&events),
            ))
        })
        .collect();
    let insert = if rows.is_empty() {
        String::new()
    } else {
        format!(
            "INSERT INTO pg_temp.chelabase_fn_wanted VALUES\n  {};\n",
            rows.join(",\n  ")
        )
    };
    let install_objects = if rows.is_empty() {
        String::new()
    } else {
        install_objects_sql()
    };
    let check = foreign_check_sql(role);
    let (repair, revoke, ours) = (repair_sql(), revoke_sql(), ours_sql());
    let truncate = if truncate {
        format!(
            "  IF pg_catalog.current_setting('chelabase_fn.fresh', true) = 'true' THEN\n    TRUNCATE {FUNCTIONS_SCHEMA}.outbox;\n  END IF;\n"
        )
    } else {
        String::new()
    };
    // Only UUIDs (hex and hyphens) reach the cleanup's body.
    let ids = wanted
        .iter()
        .map(|w| format!("'{}'", w.id))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"{SYNC_MARK}
{PRELUDE}SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
SELECT pg_catalog.pg_advisory_xact_lock({SYNC_LOCK_KEY}) IS NULL;
CREATE TEMP TABLE chelabase_fn_wanted (name text, trigger_id uuid, schema_name text, table_name text, events text, tgtype int2) ON COMMIT DROP;
{insert}-- After the lock: a database where a wanted trigger is already exactly in place was synced on
-- this timeline (a sweep got there first), so the truncate is skipped.
SELECT pg_catalog.set_config('chelabase_fn.fresh', (NOT EXISTS (SELECT FROM pg_temp.chelabase_fn_wanted w
  JOIN pg_catalog.pg_namespace n ON n.nspname = w.schema_name
  JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = w.table_name
  JOIN pg_catalog.pg_trigger tg ON tg.tgrelid = c.oid WHERE {EXACT}))::pg_catalog.text, true) IS NULL;
{check}{repair}{install_objects}{revoke}DO $chelabase_fn$ DECLARE
  ours pg_catalog.oid := {ours};
  t record;
BEGIN
  IF ours IS NULL THEN
    RETURN;
  END IF;
  FOR t IN SELECT tg.tgname, n.nspname, c.relname FROM pg_catalog.pg_trigger tg
    JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_catalog.pg_proc p ON p.oid = tg.tgfoid
    WHERE NOT tg.tgisinternal AND tg.tgparentid = 0
      AND (p.pronamespace = ours OR tg.tgname IN (SELECT w.name FROM pg_temp.chelabase_fn_wanted w))
      AND NOT EXISTS (SELECT FROM pg_temp.chelabase_fn_wanted w WHERE {EXACT})
  LOOP
    EXECUTE pg_catalog.format('DROP TRIGGER %I ON %I.%I', t.tgname, t.nspname, t.relname);
  END LOOP;
  FOR t IN SELECT w.* FROM pg_temp.chelabase_fn_wanted w
    JOIN pg_catalog.pg_namespace n ON n.nspname = w.schema_name
    JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = w.table_name AND c.relkind = 'r'
    WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_trigger tg WHERE tg.tgrelid = c.oid AND tg.tgname = w.name)
  LOOP
    EXECUTE pg_catalog.format('CREATE TRIGGER %I AFTER %s ON %I.%I FOR EACH ROW EXECUTE FUNCTION {FUNCTIONS_SCHEMA}.enqueue(%L)',
      t.name, t.events, t.schema_name, t.table_name, t.trigger_id);
  END LOOP;
END $chelabase_fn$;
DO $chelabase_fn$ DECLARE
  ours pg_catalog.oid := {ours};
BEGIN
  IF ours IS NULL OR pg_catalog.to_regclass('{FUNCTIONS_SCHEMA}.outbox') IS NULL THEN
    RETURN;
  END IF;
  -- The outbox was repaired above; its cleanup runs as its owner, never as the superuser.
  SET LOCAL ROLE {OWNER};
{truncate}  DELETE FROM {FUNCTIONS_SCHEMA}.outbox
    WHERE timeline_id IS DISTINCT FROM pg_catalog.current_setting('neon.timeline_id', true)
       OR trigger_id <> ALL (ARRAY[{ids}]::pg_catalog.uuid[]);
  RESET ROLE;
END $chelabase_fn$;
SELECT 'fn_trigger ' || w.trigger_id || CASE WHEN EXISTS (SELECT FROM pg_catalog.pg_trigger tg
    JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    WHERE {EXACT}) THEN ' ok'
    WHEN EXISTS (SELECT FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = w.schema_name AND c.relname = w.table_name) THEN ' not_a_plain_table'
    ELSE ' table_missing' END
  FROM pg_temp.chelabase_fn_wanted w;
COMMIT;
"#
    )
}

/// Up to `n` branches for the sweep: those with pending rows first, then by the least
/// recent attempt (never attempted first), so no stuck branch starves another.
pub fn sweep_order(
    candidates: &[(BranchId, bool)],
    attempts: &std::collections::HashMap<BranchId, std::time::Instant>,
    n: usize,
) -> Vec<BranchId> {
    let mut ordered: Vec<&(BranchId, bool)> = candidates.iter().collect();
    // Stable: pending first, then never attempted, then the oldest attempt.
    ordered.sort_by_key(|(b, pending)| (!*pending, attempts.get(b).copied()));
    ordered.into_iter().take(n).map(|(b, _)| *b).collect()
}

/// The script's answers: `(trigger id, None)` when installed, `(id, Some(why))` when not.
pub fn parse_sync(out: &str) -> Vec<(Uuid, Option<String>)> {
    out.lines()
        .filter_map(|line| {
            let mut words = line.trim().strip_prefix("fn_trigger ")?.split(' ');
            let id = words.next()?.parse().ok()?;
            Some(match words.next()? {
                "ok" => (id, None),
                other => (id, Some(other.to_string())),
            })
        })
        .collect()
}

/// What psql says when the database isn't there.
fn database_missing(err: &str, db: &str) -> bool {
    err.contains(&format!("database \"{db}\" does not exist"))
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The ready branch's record and its endpoint, or `None` (not ready, gone, no endpoint).
    async fn ready_endpoint(
        &self,
        branch: BranchId,
    ) -> Result<Option<(BranchRecord, String)>, String> {
        let record = self
            .metadata
            .get_branch(branch)
            .await
            .map_err(|e| format!("reading the branch: {e}"))?;
        Ok(record
            .filter(|b| b.status == BranchStatus::Ready)
            .and_then(|b| Some((b.clone(), b.endpoint_id?))))
    }

    /// Brings every database of the ready `branch` in step with its table triggers and
    /// settles the rows read (`pending_install` → `installed` or `failed`; an `installed` one
    /// whose trigger can't be put back → `failed`). In order:
    ///
    /// 1. [`role_sql`] in the system database (making [`OWNER`] when a trigger is wanted);
    /// 2. [`check_sql`] in every database of the compute: a refusal ([`FOREIGN_MARK`]) fails
    ///    every row read, terminally, and nothing is installed anywhere;
    /// 3. [`sync_sql`] in each of the branch's databases (and any a trigger names).
    ///
    /// `truncate` (after a create, reset or restore) empties the outbox only while a row read
    /// is still pending: a sweep that got there first already cleaned it. A database that
    /// can't be reached keeps its rows as they are (the sweep retries) and makes the call
    /// fail; one that doesn't exist fails its rows.
    pub async fn sync_fn_triggers(&self, branch: BranchId, truncate: bool) -> Result<(), String> {
        let Some(meta) = self.metadata.functions() else {
            return Ok(());
        };
        let Some((_, ep)) = self.ready_endpoint(branch).await? else {
            return Ok(());
        };
        let triggers: Vec<TriggerRecord> = meta
            .list_triggers(branch)
            .await
            .map_err(|e| format!("listing the triggers: {e}"))?
            .into_iter()
            .filter(|t| t.source == TriggerSource::Table)
            .collect();
        let truncate = truncate
            && triggers
                .iter()
                .any(|t| t.state == TriggerState::PendingInstall);
        let wanted_any = triggers.iter().any(|t| t.state != TriggerState::Failed);
        let role = self
            .computes
            .admin_sql(&ep, SYSTEM_DATABASE, &role_sql(wanted_any))
            .await
            .map(|out| parse_role(&out))
            .map_err(|e| format!("reading the function triggers' role: {e:#}"))?;
        let mut databases: Vec<String> = self
            .metadata
            .branch_databases(branch)
            .await
            .map_err(|e| format!("listing the databases: {e}"))?
            .into_iter()
            .filter(|d| d.origin != DatabaseOrigin::System)
            .map(|d| d.name)
            .collect();
        for t in &triggers {
            if !databases.contains(&t.pg_database) {
                databases.push(t.pg_database.clone());
            }
        }
        let compute_databases = self
            .computes
            .admin_sql(&ep, SYSTEM_DATABASE, &review_databases_sql())
            .await
            .map_err(|e| format!("listing the compute's databases: {e:#}"))?;
        for db in compute_databases
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
        {
            if let Err(e) = self.computes.admin_sql(&ep, db, &check_sql(role)).await {
                let e = format!("{e:#}");
                return match foreign_refusal(&e) {
                    Some(refusal) => {
                        warn!(branch_id = %branch, database = %db, "function triggers refused: {refusal}");
                        // Nothing keeps firing: the triggers go first. A database that can't
                        // be reached keeps the rows as they are, so the sweep comes back.
                        for db in &databases {
                            if let Err(e) =
                                self.computes.admin_sql(&ep, db, &drop_triggers_sql()).await
                            {
                                let e = format!("{e:#}");
                                if !database_missing(&e, db) {
                                    return Err(format!(
                                        "dropping the refused function triggers in {db}: {e}"
                                    ));
                                }
                            }
                        }
                        self.settle(meta, branch, &triggers, |_| Some(Some(refusal.clone())))
                            .await
                    }
                    None => Err(format!("checking the function triggers' role in {db}: {e}")),
                };
            }
        }
        let mut answers: Vec<(Uuid, Option<String>)> = Vec::new();
        let mut failure = None;
        for db in &databases {
            let wanted = wanted_in(&triggers, db);
            match self
                .computes
                .admin_sql(&ep, db, &sync_sql(&wanted, truncate, role))
                .await
            {
                Ok(out) => answers.extend(parse_sync(&out)),
                Err(e) => {
                    let e = format!("{e:#}");
                    let why = if database_missing(&e, db) {
                        Some("database_missing".to_string())
                    } else {
                        foreign_refusal(&e)
                    };
                    match why {
                        Some(why) => {
                            answers.extend(wanted.iter().map(|w| (w.id, Some(why.clone()))))
                        }
                        None => {
                            warn!(branch_id = %branch, database = %db, "syncing the function triggers: {e}");
                            failure = Some(format!("syncing the function triggers of {db}: {e}"));
                        }
                    }
                }
            }
        }
        self.settle(meta, branch, &triggers, |t| {
            match answers.iter().find(|(id, _)| *id == t.id) {
                Some((_, error)) => Some(error.as_deref().map(install_error)),
                // Bad events never reach the script (the schema's CHECK refuses them).
                None if trigger_events(&t.events).is_none() => Some(Some("invalid_events".into())),
                None => None,
            }
        })
        .await?;
        failure.map_or(Ok(()), Err)
    }

    /// Settles each row read that `outcome` answers for (`Some(None)`: in the database;
    /// `Some(Some(why))`: not), at the generation read.
    async fn settle(
        &self,
        meta: &MetadataClient,
        branch: BranchId,
        triggers: &[TriggerRecord],
        outcome: impl Fn(&TriggerRecord) -> Option<Option<String>>,
    ) -> Result<(), String> {
        let installs: Vec<TriggerInstall> = triggers
            .iter()
            .filter(|t| t.state != TriggerState::Failed)
            .filter_map(|t| {
                Some(TriggerInstall {
                    id: t.id,
                    enabled_since: t.enabled_since,
                    error: outcome(t)?,
                })
            })
            .collect();
        let settled = meta
            .settle_trigger_installs(branch, &installs)
            .await
            .map_err(|e| format!("settling the trigger installs: {e}"))?;
        if settled > 0 {
            info!(branch_id = %branch, settled, "function triggers settled");
        }
        Ok(())
    }

    /// Records an attempt on `branch` for the sweep of `kind`, now.
    fn attempted(&self, kind: Sweep, branch: BranchId) {
        let mut attempts = self.fn_sweeps.lock().unwrap_or_else(|e| e.into_inner());
        attempts
            .entry(kind)
            .or_default()
            .insert(branch, std::time::Instant::now());
    }

    /// Up to [`SWEEP_BATCH`] of `candidates` for the sweep of `kind` ([`sweep_order`]).
    fn sweep_batch(&self, kind: Sweep, candidates: &[(BranchId, bool)]) -> Vec<BranchId> {
        let mut attempts = self.fn_sweeps.lock().unwrap_or_else(|e| e.into_inner());
        let map = attempts.entry(kind).or_default();
        // Forget branches that left the sweep.
        map.retain(|b, _| candidates.iter().any(|(c, _)| c == b));
        sweep_order(candidates, map, SWEEP_BATCH as usize)
    }

    /// The reconcile sweep (best effort): branches with a pending table trigger to install,
    /// then branches whose installed triggers are checked for drift (a dropped, disabled or
    /// changed database trigger is put back; one whose table is gone fails), each pass taking
    /// the least recently attempted. A `failed` row never comes back here.
    pub async fn sync_pending_fn_triggers(&self) {
        let Some(meta) = self.metadata.functions() else {
            return;
        };
        let candidates = match meta.trigger_sweep_branches(SWEEP_CANDIDATES).await {
            Ok(candidates) => candidates,
            Err(e) => return warn!("listing branches with function triggers: {e}"),
        };
        for branch in self.sweep_batch(Sweep::Triggers, &candidates) {
            self.attempted(Sweep::Triggers, branch);
            if let Err(e) = self.sync_fn_triggers(branch, false).await {
                warn!(branch_id = %branch, "function triggers not synced: {e}");
            }
        }
    }

    /// The reconcile sweep for child run-as keys still to mint (best effort): every ready
    /// branch with a function that has a `run_as_role` and no key, chosen by state alone,
    /// each pass taking the least recently attempted.
    pub async fn sync_pending_run_as(&self) {
        let Some(meta) = self.metadata.functions() else {
            return;
        };
        let candidates: Vec<(BranchId, bool)> =
            match meta.pending_run_as_branches(SWEEP_CANDIDATES).await {
                Ok(branches) => branches.into_iter().map(|b| (b, true)).collect(),
                Err(e) => return warn!("listing branches with run-as keys to mint: {e}"),
            };
        for branch in self.sweep_batch(Sweep::RunAs, &candidates) {
            self.attempted(Sweep::RunAs, branch);
            self.mint_child_run_as(branch).await;
        }
    }

    /// After a branch create: the child's own run-as keys, then its triggers (truncating the
    /// outbox it copied). Best effort; the sweep retries the triggers.
    pub(crate) async fn fn_after_create(&self, branch: BranchId) {
        self.mint_child_run_as(branch).await;
        if let Err(e) = self.sync_fn_triggers(branch, true).await {
            warn!(branch_id = %branch, "installing the function triggers on the new branch: {e}");
        }
    }

    /// A ready child's managed run-as keys: for each function with a `run_as_role` and no key
    /// (the copy's), the role is checked on the child as a secret key's is (the name rules
    /// and the compute's check, which binds the role's OID, then the "all rows" check) and a
    /// key is minted through `set_run_as`. A role the child doesn't have sets
    /// [`RUN_AS_ROLE_MISSING`]; a refused one `run_as_role_refused: <why>`. A check that can't
    /// run sets `run_as_key_pending: <why>` ([`RUN_AS_KEY_PENDING`]), which the reconcile sweep
    /// retries (it retries the other two as well: a role can be made, or changed, later);
    /// minting clears these errors and never another one (an invocation error, say).
    pub(crate) async fn mint_child_run_as(&self, branch: BranchId) {
        let Some(meta) = self.metadata.functions() else {
            return;
        };
        if let Err(e) = self.mint_child_run_as_with(meta, branch).await {
            warn!(branch_id = %branch, "minting the child's run-as keys: {e}");
        }
    }

    async fn mint_child_run_as_with(
        &self,
        meta: &MetadataClient,
        branch: BranchId,
    ) -> Result<(), String> {
        let Some((record, ep)) = self.ready_endpoint(branch).await? else {
            return Ok(());
        };
        // The role check runs where `check_data_role` runs it: the branch's main database.
        let connection = self
            .metadata
            .branch_connection(branch)
            .await
            .map_err(|e| format!("reading the branch: {e}"))?;
        let Some(db) = connection.and_then(|c| c.database_name) else {
            return Ok(());
        };
        let functions = meta
            .list_for_branch(branch)
            .await
            .map_err(|e| e.to_string())?;
        // Chosen by state alone (the functions service overwrites `last_error` on any
        // invocation error): a role, no key, no terminal outcome.
        for f in functions
            .iter()
            .filter(|f| f.run_as_key_id.is_none() && f.run_as_unmintable.is_none())
        {
            let Some(role) = f.run_as_role.as_deref() else {
                continue;
            };
            let approved = f.run_as_role_oid;
            let terminal = |why: RunAsUnmintable, error: String| (why, error);
            let outcome: Result<Option<RunAs>, (RunAsUnmintable, String)> = match approved {
                // Nothing to bind to: never a namesake.
                None => Err(terminal(
                    RunAsUnmintable::Changed,
                    format!(
                        "{RUN_AS_ROLE_CHANGED}: no approved role identity for {role}; set the run-as role again"
                    ),
                )),
                // `Some(approved)`: the compute's check refuses a role holding the name with
                // another OID before it grants anything.
                Some(approved) => match self
                    .check_and_grant(branch, &ep, &db, role, Some(approved))
                    .await
                {
                    RoleCheck::Ok(oid, bypass) => {
                        let request = AllRowsCheckRequest {
                            branch_id: branch,
                            starting: vec![role.to_string()],
                            change: None,
                        };
                        match self.all_rows_check(request).await {
                            AllRowsCheckReply::Clear => {
                                let generated = data_key::generate(DataKeyKind::Secret);
                                Ok(Some(RunAs {
                                    role: role.to_string(),
                                    role_oid: oid,
                                    role_timeline: record.timeline_id.clone(),
                                    role_bypass_rls: bypass,
                                    key_id: Uuid::new_v4(),
                                    prefix: generated.prefix,
                                    sha256: generated.sha256,
                                    secret: generated.key,
                                }))
                            }
                            AllRowsCheckReply::Exposed { .. } => Err(terminal(
                                RunAsUnmintable::Refused,
                                format!(
                                    "{RUN_AS_ROLE_REFUSED}: the role {role} reaches a role with the \"all rows\" policy"
                                ),
                            )),
                            AllRowsCheckReply::Failed { message } => {
                                self.mint_pending(
                                    meta,
                                    branch,
                                    f,
                                    &format!("the \"all rows\" check failed: {message}"),
                                )
                                .await?;
                                Ok(None)
                            }
                        }
                    }
                    RoleCheck::Refused(why) if why.ends_with("does not exist") => Err(terminal(
                        RunAsUnmintable::Missing,
                        RUN_AS_ROLE_MISSING.to_string(),
                    )),
                    RoleCheck::Refused(why) if why.contains("different role") => Err(terminal(
                        RunAsUnmintable::Changed,
                        format!("{RUN_AS_ROLE_CHANGED}: {why}"),
                    )),
                    RoleCheck::Refused(why) => Err(terminal(
                        RunAsUnmintable::Refused,
                        format!("{RUN_AS_ROLE_REFUSED}: {why}"),
                    )),
                    RoleCheck::NotReady => {
                        self.mint_pending(
                            meta,
                            branch,
                            f,
                            "the branch's compute couldn't check the role",
                        )
                        .await?;
                        Ok(None)
                    }
                    RoleCheck::Failed(why) => {
                        self.mint_pending(meta, branch, f, &why).await?;
                        Ok(None)
                    }
                },
            };
            let settled = match &outcome {
                Ok(None) => continue,
                Ok(Some(run_as)) => {
                    meta.settle_run_as_mint(
                        branch,
                        f.function_id,
                        role,
                        approved,
                        RunAsMint::Key(run_as),
                    )
                    .await
                }
                Err((why, error)) => {
                    meta.settle_run_as_mint(
                        branch,
                        f.function_id,
                        role,
                        approved,
                        RunAsMint::Unmintable(*why, error),
                    )
                    .await
                }
            }
            .map_err(|e| format!("function {}: {e}", f.name))?;
            if !settled {
                // A member changed or cleared the run-as role meanwhile (or another worker
                // settled it): their choice stands, and the generated key is dropped.
                info!(branch_id = %branch, function = %f.name, "the run-as role changed while it was checked; nothing minted");
                continue;
            }
            if outcome.is_ok() {
                info!(branch_id = %branch, function = %f.name, "minted the child's run-as key");
                if f.last_error
                    .as_deref()
                    .is_some_and(|e| e.starts_with(RUN_AS_KEY_PENDING))
                {
                    meta.set_function_last_error(branch, f.function_id, None)
                        .await
                        .map_err(|e| format!("function {}: {e}", f.name))?;
                }
            }
        }
        Ok(())
    }

    /// Records that `f`'s run-as key couldn't be minted yet ([`RUN_AS_KEY_PENDING`]): the sweep
    /// retries it.
    async fn mint_pending(
        &self,
        meta: &MetadataClient,
        branch: BranchId,
        f: &chelabase_metadata::BranchFunction,
        why: &str,
    ) -> Result<(), String> {
        warn!(branch_id = %branch, function = %f.name, "the child's run-as key isn't minted yet: {why}");
        meta.set_function_last_error(
            branch,
            f.function_id,
            Some(&format!("{RUN_AS_KEY_PENDING}: {why}")),
        )
        .await
        .map_err(|e| format!("function {}: {e}", f.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted(table: &str, events: &[&str]) -> Wanted {
        Wanted {
            id: Uuid::parse_str("0123456789abcdef0123456789abcdef").unwrap(),
            schema: "public".into(),
            table: table.into(),
            events: events.iter().map(|e| e.to_string()).collect(),
        }
    }

    #[test]
    fn fn_triggers_names_and_events() {
        let id = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert_eq!(
            trigger_name(id),
            "chelabase_fn_0123456789abcdef0123456789abcdef"
        );
        let ev = |e: &[&str]| trigger_events(&e.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(ev(&["insert"]), Some(("INSERT".into(), 5)));
        assert_eq!(
            ev(&["update", "insert"]),
            Some(("INSERT OR UPDATE".into(), 21))
        );
        assert_eq!(
            ev(&["delete", "update", "insert"]),
            Some(("INSERT OR DELETE OR UPDATE".into(), 29))
        );
        for bad in [
            &[][..],
            &["insert", "insert"],
            &["truncate"],
            &["INSERT"],
            &["insert", "drop"],
        ] {
            assert_eq!(ev(bad), None, "{bad:?}");
        }
    }

    /// A table name with quotes, spaces, a backslash or a dollar-quote tag reaches the script
    /// only inside literals of the wanted rows; the statements quote it with `%I`.
    #[test]
    fn fn_triggers_identifiers_only_as_literals() {
        let nasty = r#"or"d's $chelabase_fn$ \x; DROP TABLE t; --"#;
        let sql = sync_sql(&[wanted(nasty, &["insert"])], false, Some(4242));
        let row = sql
            .lines()
            .find(|l| l.contains("chelabase_fn_0123"))
            .unwrap();
        assert_eq!(
            row.trim(),
            format!(
                "('chelabase_fn_0123456789abcdef0123456789abcdef', '01234567-89ab-cdef-0123-456789abcdef', 'public', {}, 'INSERT', 5);",
                literal(nasty)
            )
        );
        assert_eq!(sql.matches(nasty).count(), 0, "never raw");
        assert!(sql.contains("pg_catalog.format('CREATE TRIGGER %I AFTER %s ON %I.%I FOR EACH ROW EXECUTE FUNCTION chelabase_functions.enqueue(%L)'"));
        assert!(sql.contains("pg_catalog.format('DROP TRIGGER %I ON %I.%I'"));
        // Rows with events the schema refuses never reach the script.
        assert!(
            !sync_sql(&[wanted("t", &["truncate"])], false, Some(4242))
                .contains("INSERT INTO pg_temp")
        );
    }

    /// The function's privileges and search path, the event triggers off, the outbox cleaned.
    #[test]
    fn fn_triggers_script_is_locked_down() {
        let sql = sync_sql(&[wanted("orders", &["insert"])], true, Some(4242));
        assert!(sql.starts_with(&format!("{SYNC_MARK}\nBEGIN;\nSET LOCAL search_path = pg_catalog, pg_temp;\nSET LOCAL event_triggers = off;\n")));
        assert!(sql.trim_end().ends_with("COMMIT;"));
        for needle in [
            "LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, chelabase_functions, pg_temp AS",
            "ALTER FUNCTION chelabase_functions.enqueue() OWNER TO chelabase_functions_owner;",
            "REVOKE ALL ON FUNCTION chelabase_functions.enqueue() FROM PUBLIC;",
            "REVOKE ALL ON SCHEMA chelabase_functions FROM PUBLIC;",
            "REVOKE ALL ON TABLE chelabase_functions.outbox FROM PUBLIC;",
            "REVOKE ALL ON SEQUENCE chelabase_functions.outbox_id_seq FROM PUBLIC;",
            "PERFORM pg_catalog.pg_notify('chelabase_functions', '');",
            "pg_catalog.jsonb_build_object(",
            "TRUNCATE chelabase_functions.outbox;",
            "OR trigger_id <> ALL (ARRAY['01234567-89ab-cdef-0123-456789abcdef']::pg_catalog.uuid[]);",
            "REVOKE ALL ON %s FROM %s CASCADE",
            "OR EXISTS (SELECT FROM pg_catalog.pg_trigger WHERE tgrelid = box)",
        ] {
            assert!(sql.contains(needle), "{needle}");
        }
        assert!(
            !sync_sql(&[wanted("orders", &["insert"])], false, Some(4242)).contains("TRUNCATE")
        );
        // Audit B1: the row is never handed to to_jsonb whole (a user's cast would run as the
        // owner); the cleanup runs as the owner, after the repair, never as the superuser.
        assert!(!sql.contains("to_jsonb(NEW)") && !sql.contains("to_jsonb(OLD)"));
        // Round 3 (N1, S2): the truncate is decided after the lock; grants to the role revoked.
        assert!(
            sql.find("pg_advisory_xact_lock").unwrap() < sql.find("'chelabase_fn.fresh'").unwrap()
        );
        assert!(
            sql.contains("IF pg_catalog.current_setting('chelabase_fn.fresh', true) = 'true' THEN")
        );
        assert!(sql.contains("REVOKE ALL ON ROUTINE %s FROM chelabase_functions_owner CASCADE"));
        assert!(drop_triggers_sql().contains("pg_catalog.format('DROP TRIGGER %I ON %I.%I'"));
        let role = sql
            .find("SET LOCAL ROLE chelabase_functions_owner;")
            .unwrap();
        assert!(sql.find("had been changed; it was rebuilt").unwrap() < role);
        assert!(
            sql.find(FOREIGN_MARK).unwrap() < sql.find("CREATE SCHEMA").unwrap(),
            "checked before any install"
        );
        assert!(role < sql.find("TRUNCATE chelabase_functions.outbox;").unwrap());
        assert!(role < sql.find("DELETE FROM chelabase_functions.outbox").unwrap());
        // Nothing wanted: no install, only the cleanup of a schema that is ours.
        let cleanup = sync_sql(&[], true, Some(4242));
        assert!(!cleanup.contains("CREATE SCHEMA") && !cleanup.contains("CREATE ROLE"));
        assert!(cleanup.contains("IF ours IS NULL THEN\n    RETURN;"));
    }

    #[test]
    fn fn_triggers_answers_parse() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let out = format!("f\nfn_trigger {a} ok\nfn_trigger {b} table_missing\nnoise\n");
        assert_eq!(
            parse_sync(&out),
            vec![(a, None), (b, Some("table_missing".into()))]
        );
    }
    /// Round 2 (S2): 25 branches that always fail never starve the others: each pass takes
    /// the least recently attempted, a branch with pending rows before one being checked for
    /// drift, and every branch is attempted within two passes of 20.
    #[test]
    fn fn_triggers_sweep_order_rotates_past_stuck_branches() {
        use std::collections::HashMap;
        use std::time::{Duration, Instant};
        let stuck: Vec<(BranchId, bool)> = (0..25).map(|_| (BranchId::new(), true)).collect();
        let checked = (BranchId::new(), false);
        let mut candidates = stuck.clone();
        candidates.push(checked);
        let mut attempts: HashMap<BranchId, Instant> = HashMap::new();
        let mut clock = Instant::now();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2 {
            let picked = sweep_order(&candidates, &attempts, 20);
            assert_eq!(picked.len(), 20);
            for b in picked {
                clock += Duration::from_millis(1);
                attempts.insert(b, clock);
                seen.insert(b);
            }
        }
        assert!(
            stuck.iter().all(|(b, _)| seen.contains(b)),
            "a stuck branch starved"
        );
        // A branch that just got a pending row goes first, before every stuck one.
        let fresh = (BranchId::new(), true);
        candidates.push(fresh);
        assert_eq!(sweep_order(&candidates, &attempts, 1), vec![fresh.0]);
        // A drift check comes after pending branches, however long it waited.
        let only_pending = sweep_order(&candidates, &attempts, 26);
        assert!(!only_pending.contains(&checked.0));
        assert_eq!(
            sweep_order(&candidates, &attempts, 27).last(),
            Some(&checked.0)
        );
    }

    /// Round 2 (B1, B2): plain tables only; the install adopts nothing it didn't make.
    #[test]
    fn fn_triggers_plain_tables_and_no_adoption() {
        let sql = sync_sql(&[wanted("orders", &["insert"])], false, Some(4242));
        assert!(sql.contains("c.relkind = 'r'"), "plain tables only");
        assert!(!sql.contains("IN ('r', 'p')"));
        assert!(sql.contains("' not_a_plain_table'"));
        assert_eq!(install_error("not_a_plain_table"), PLAIN_TABLES_ONLY);
        assert_eq!(install_error("table_missing"), "table_missing");
        for needle in [
            "pg_catalog.pg_shdepend",
            "rolinherit",
            "4242",
            FOREIGN_MARK,
            "pg_catalog.pg_db_role_setting",
            "pg_catalog.pg_auth_members",
        ] {
            assert!(sql.contains(needle), "{needle}");
        }
        assert!(
            !sql.contains("CREATE ROLE"),
            "the role is made once, before the checks"
        );
        assert!(
            !sql.contains("REVOKE chelabase_functions_owner FROM"),
            "never repaired: refused"
        );
        assert!(check_sql(Some(4242)).contains(FOREIGN_MARK));
        assert!(role_sql(true).contains(
            "CREATE ROLE chelabase_functions_owner NOLOGIN NOINHERIT CONNECTION LIMIT 0"
        ));
    }
}
