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
//! **Invariant (tenants).** No action by a role without the owner's or Admin's rights can
//! fail, disable or stall the install, and the sweep never blocks tenant writes for more than
//! its lock_timeout ([`SWEEP_LOCK_TIMEOUT`]). So: only what could hand someone the owner
//! role's rights refuses (its attributes, memberships, settings, what it *owns* outside the
//! schema, a role or schema the install didn't make); whatever merely names the role as a
//! grantee or a policy's target is revoked or ignored. The sweep runs DDL only when the
//! catalog shows something missing or wrong, waits at most the lock_timeout for any lock, and a
//! lock timeout or another sync holding the database is a retry on the next pass, never a
//! failure ([`retry_later`]). A session squatting on the sync's advisory key (any role may)
//! is ignored unless it is a superuser's, which is another sync. The owner role is reserved
//! by name everywhere a role can be chosen (`data_api::refuse_by_name`, rest's per-request
//! gate), so no key or run-as role is ever bound to it. One qualification: a transaction that
//! holds a lock on a triggered table itself delays that table's `CREATE TRIGGER` until it
//! ends (the sweep gives up after the lock_timeout and comes back), as any DDL would.
//! `audit_sweep_never_blocks_tenant_writes`, `audit_unprivileged_role_cannot_disable_the_triggers`,
//! `audit_owner_role_never_backs_a_key` and `audit_squatted_advisory_lock_never_stalls_the_install`
//! test it, and the attack matrix carries each move.
//!
//! **Invariant (dispatch, m13b).** The dispatch login [`DISPATCH`] holds exactly its
//! allowlist and nothing that could reach member data or another role: LOGIN NOINHERIT with
//! [`DISPATCH_CONNECTION_LIMIT`], no other attribute, no member or membership, nothing owned,
//! exactly [`DISPATCH_SETTINGS`], and exactly the grants [`FUNCTIONS_DISPATCH_SCHEMA_GRANTS`]
//! and [`FUNCTIONS_DISPATCH_OUTBOX_GRANTS`] (from the owner,
//! without grant option). Any attribute, membership, setting or ownership beyond that refuses
//! the install (the foreign check, as for [`OWNER`]); any grant beyond it is revoked on every
//! pass ([`revoke_sql`]), and a missing one is put back. Its password is the branch's own
//! (sealed `Purpose::FunctionsDispatch`), set by [`role_sql`] only while the role has no
//! membership either way. The name is reserved like the owner's (`refuse_by_name`'s
//! `chelabase_` prefix), so no key or run-as role is ever bound to it. The attack matrix
//! carries its moves; `dispatch_role_privilege_matrix` shows what it can't do.
//!
//! [`Worker::sync_fn_triggers`] keeps a branch's databases in step with its table triggers
//! (`function_triggers`, source `table`), as the superuser (`cloud_admin`):
//!
//! - **The role**: [`OWNER`], made once per compute by [`role_sql`] (NOLOGIN NOINHERIT
//!   CONNECTION LIMIT 0, by the superuser, so it never has members). Every check and install
//!   that follows requires that very role.
//! - **The dispatch login** (m13b): [`DISPATCH`], made by [`role_sql`] beside the owner when
//!   a trigger is wanted, for the functions service's dispatcher (`services/functions/src/events`).
//! - **The foreign check** ([`check_sql`], in every database of the compute, before anything
//!   is installed anywhere, and again at the top of each install): the role and the schema
//!   `chelabase_functions` must be exactly what the install makes. That means no attribute,
//!   membership or setting on the role, and nothing it *owns* outside the schema
//!   (`pg_shdepend` ownership entries: shared objects, and each database's own). Entries that
//!   only name the role as a grantee (a grant on any object, a default privilege, a large
//!   object) or as a policy's target give nobody the role's rights (nobody can act as it, and
//!   `enqueue()` reads nothing): they pass the check; the grants are revoked ([`revoke_sql`])
//!   and a policy is left to its table's owner. The schema must be its own, with nothing in
//!   the schema but the outbox, its sequence, index and row type, and `enqueue()`. Anything
//!   else is refused with [`FOREIGN_MARK`], never repaired. Every `chelabase_fn_*` trigger
//!   running `enqueue()` is dropped ([`drop_triggers_sql`]), and every row read settles
//!   `failed` (terminal until the trigger is changed or pended again). A `failed` row is never
//!   wanted, so its database trigger is dropped, never installed.
//! - **The machinery**, checked each run while the database has a trigger to install and made
//!   or put back only where the catalog shows it missing or wrong (so a pass over a database
//!   in good order runs no DDL and takes no lock a tenant write conflicts with): the schema
//!   (no rights for PUBLIC, so no data API role reaches it), the outbox
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
//!   <schema>.<table> FOR EACH ROW EXECUTE FUNCTION chelabase_functions.enqueue('<id>',
//!   '<run-as OID>')`, on
//!   plain tables only. A partitioned table's row triggers would be cloned onto every new
//!   partition with the rights of whoever adds it, and only [`OWNER`] may execute `enqueue()`;
//!   such a row settles `failed` with [`PLAIN_TABLES_ONLY`]. A trigger is dropped when it runs
//!   a function of `chelabase_functions`, or has a wanted name, and isn't exactly a wanted
//!   one (name, plain table, function, events, arguments, enabled, no column list or `WHEN`).
//!   Then each wanted one whose table exists is created. Names reach the SQL only as literals,
//!   and the statements are built by `format('%I')`.
//! - Event triggers are off for every transaction (`SET LOCAL event_triggers = off`), so no
//!   tenant code runs as the superuser. Every transaction waits at most [`SWEEP_LOCK_TIMEOUT`]
//!   for a lock, and two syncs of one database serialize on the advisory key
//!   [`SYNC_LOCK_KEY`] without waiting: the second finds a superuser's session holding it and
//!   yields ([`BUSY_MARK`], retried); any other holder is a tenant squatting on the key and is
//!   ignored.
//!
//! m17: a trigger is wanted only while its function has an approved run-as role OID on the
//! branch (else it settles [`RUN_AS_ROLE_REQUIRED`]), and is created, kept and fed only while
//! that role may read the table ([`run_as_reads`]: it exists, has table-level `SELECT` and
//! `USAGE` on the table's schema, and is outside the table's row level security);
//! `enqueue()` checks the same rule on every row.
//! A trigger that fails it is dropped, its queued events are removed (counted in
//! `dropped_count`), and it settles [`RUN_AS_CANNOT_READ_TABLE`] or [`RUN_AS_SUBJECT_TO_RLS`].
//!
//! The script answers one line per wanted trigger: `fn_trigger <id> ok`, `… no_select`,
//! `… subject_to_rls`, `… not_a_plain_table` or `… table_missing`. The worker settles the rows
//! it read, by the `enabled_since` it read (`MetadataClient::settle_trigger_installs`): only
//! `installed` rows are the dispatcher's (m13).
//!
//! m13 (#175): a disabled trigger is never wanted (its database trigger is dropped and its
//! queued events deleted); a deleted or disabled trigger, and a deleted function's triggers,
//! are recorded in `function_trigger_drops`, which keeps the branch in the reconcile sweep
//! until a complete sync ([`Worker::sync_fn_triggers`] answers `Ok(true)`) has dropped them
//! ("drop, then forget"). `enqueue()` also records the transaction id (`txid`). The events are
//! delivered by the functions service (m13b, `services/functions/src/events`), as the login
//! [`DISPATCH`].
//!
//! When: after a branch create (the child's rows are copies, `pending_install`, and its
//! database still has the parent's triggers and outbox); after a reset or restore (the switch
//! pended every table trigger of the branch); and on each reconcile pass
//! ([`Worker::sync_pending_fn_triggers`]). That pass covers branches with a pending row, and
//! branches whose installed triggers are checked for drift (dropped, disabled or changed: put
//! back), up to [`SWEEP_BATCH`], least recently attempted first, from one page of candidates
//! (each pass reads the page after the last, [`SweepCursor`]). A branch create also mints the
//! child's run-as keys ([`Worker::mint_child_run_as`]); the reconcile pass retries any still
//! owed ([`Worker::sync_pending_run_as`], chosen by state, not by `last_error`).

use chelabase_metadata::{
    BranchRecord, BranchStatus, DatabaseOrigin, MetadataClient, RunAs, RunAsMint, RunAsUnmintable,
    TriggerDelivery, TriggerInstall, TriggerRecord, TriggerSource, TriggerState,
};
use chelabase_nats::{AllRowsCheckReply, AllRowsCheckRequest};
use chelabase_neon::ComputeBackend;
use chelabase_schema::exclusions::{
    FUNCTION_TRIGGER_PREFIX, FUNCTIONS_DISPATCH_OUTBOX_GRANTS, FUNCTIONS_DISPATCH_ROLE,
    FUNCTIONS_DISPATCH_SCHEMA_GRANTS, FUNCTIONS_NOTIFY_CHANNEL, FUNCTIONS_SCHEMA,
};
use chelabase_schema::sql::literal;
use chelabase_types::BranchId;
use chelabase_types::data_key::{self, DataKeyKind};
use tracing::{info, warn};
use uuid::Uuid;

use crate::Worker;
use crate::data_api::RoleCheck;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::resync::SYSTEM_DATABASE;
use crate::review::review_databases_sql;

/// The NOLOGIN role owning the schema, the outbox and the trigger function.
pub const OWNER: &str = "chelabase_functions_owner";

/// m13b: the functions service's dispatcher login (LOGIN NOINHERIT, no other attribute or
/// membership), granted exactly [`FUNCTIONS_DISPATCH_SCHEMA_GRANTS`] and
/// [`FUNCTIONS_DISPATCH_OUTBOX_GRANTS`] (the shared names, `chelabase_schema::exclusions`).
pub const DISPATCH: &str = FUNCTIONS_DISPATCH_ROLE;

/// m13b: the dispatch login's connection limit. The dispatcher holds one connection per
/// branch database with an installed trigger; during a rolling deploy two replicas may each.
pub const DISPATCH_CONNECTION_LIMIT: u32 = 40;

/// m13b: the dispatch login's settings, sorted as the foreign check compares them: no event
/// trigger (a login one included) fires for its sessions, and names resolve in `pg_catalog`.
const DISPATCH_SETTINGS: [&str; 2] = ["event_triggers=off", "search_path=pg_catalog, pg_temp"];

/// m13b (B2): `last_error` on a trigger on a reserved schema
/// (`chelabase_schema::exclusions::function_trigger_schema_refused`): never installed.
pub const RESERVED_SCHEMA: &str = "function triggers can't go on a reserved schema";

/// The channel `enqueue()` notifies after each insert (the dispatcher listens, m13).
pub const NOTIFY_CHANNEL: &str = FUNCTIONS_NOTIFY_CHANNEL;

/// The script's first line (the fakes' needle).
pub const SYNC_MARK: &str = "-- chelabase: function triggers";

/// The error the install raises when `chelabase_functions` isn't [`OWNER`]'s.
pub const FOREIGN_MARK: &str = "chelabase_functions_foreign";

/// The transaction-scoped advisory lock two syncs of one database serialize on: the bytes of
/// "chelabase" followed by 0x08 (the bootstrap's is 0x01). Never waited on: a superuser's
/// session holding it is another sync ([`BUSY_MARK`]); any other holder is ignored.
pub const SYNC_LOCK_KEY: i64 = 0x6368_656c_6164_6208;

/// The error a sync raises when another sync (a superuser's session) holds [`SYNC_LOCK_KEY`]:
/// retried on the next pass ([`retry_later`]).
pub const BUSY_MARK: &str = "chelabase_fn_busy";

/// How long any statement of a sync waits for a lock (the third invariant: a tenant write
/// queues behind the sweep for at most this long). A timeout is a retry, never a failure.
pub const SWEEP_LOCK_TIMEOUT: &str = "2s";

/// A sync error that means "try again on the next pass": a lock timeout (a tenant held what
/// the sync needed), or another sync of the database ([`BUSY_MARK`]).
pub fn retry_later(error: &str) -> bool {
    error.contains("canceling statement due to lock timeout") || error.contains(BUSY_MARK)
}

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

/// m17: `last_error` on an enabled table trigger whose function has no approved run-as role
/// (`run_as_role_oid`) on the branch: nothing is installed until a member sets one.
pub const RUN_AS_ROLE_REQUIRED: &str = "run_as_role_required";

/// m17: `last_error` on a table trigger whose function's run-as role (the approved OID) is
/// gone, lacks table-level `SELECT` on the table or lacks `USAGE` on its schema
/// ([`run_as_reads`]).
pub const RUN_AS_CANNOT_READ_TABLE: &str = "run_as_role_cannot_read_table";

/// m17: `last_error` on a table trigger whose table has row level security that applies to
/// the function's run-as role (no BYPASSRLS, and not the owner, or the table forces RLS on
/// its owner) ([`run_as_reads`]).
pub const RUN_AS_SUBJECT_TO_RLS: &str = "run_as_role_subject_to_rls";

/// The stored error for a script answer.
pub fn install_error(answer: &str) -> String {
    match answer {
        "not_a_plain_table" => PLAIN_TABLES_ONLY.to_string(),
        "no_select" => RUN_AS_CANNOT_READ_TABLE.to_string(),
        "subject_to_rls" => RUN_AS_SUBJECT_TO_RLS.to_string(),
        other => other.to_string(),
    }
}

/// The settings every script starts with: the search path, event triggers off, the short lock
/// wait ([`SWEEP_LOCK_TIMEOUT`]), and a statement timeout of its own: a database's or role's
/// shorter `statement_timeout` (a tenant's `ALTER DATABASE … SET`) would otherwise fail every
/// pass, and none of a sync's statements waits on more than a lock.
const PRELUDE: &str = "BEGIN;\nSET LOCAL search_path = pg_catalog, pg_temp;\nSET LOCAL event_triggers = off;\nSET LOCAL lock_timeout = '2s';\nSET LOCAL statement_timeout = '2min';\n";

/// Run once per sync in the compute's system database, before any check: makes [`OWNER`]
/// when `create` and it's missing (`NOLOGIN NOINHERIT CONNECTION LIMIT 0`, by the superuser,
/// so nobody else is ever its member), and answers `fn_owner <oid>` when it exists. The
/// checks and installs that follow require that very OID: a role made by anyone in between is
/// refused.
///
/// m13b: with `dispatch_verifier` (the branch's dispatch password's SCRAM verifier), it also
/// makes [`DISPATCH`] when missing (LOGIN NOINHERIT, [`DISPATCH_CONNECTION_LIMIT`], the
/// verifier, [`DISPATCH_SETTINGS`]), and sets the verifier on one that carries another (a
/// child's copy of its parent's, a restored one) while the role has no membership either way
/// (a role someone else made is the foreign check's to refuse, never handed a password). It
/// answers `fn_dispatch <oid>` too, which the checks require likewise.
pub fn role_sql(create: bool, dispatch_verifier: Option<&str>) -> String {
    let mut make = if create {
        format!(
            "DO $chelabase_fn$ BEGIN\n  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = '{OWNER}') THEN\n    \
             CREATE ROLE {OWNER} NOLOGIN NOINHERIT CONNECTION LIMIT 0;\n  END IF;\nEND $chelabase_fn$;\n"
        )
    } else {
        String::new()
    };
    if let (true, Some(verifier)) = (create, dispatch_verifier) {
        let verifier = literal(verifier);
        make.push_str(&format!(
            r#"DO $chelabase_fn$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = '{DISPATCH}') THEN
    CREATE ROLE {DISPATCH} LOGIN NOINHERIT CONNECTION LIMIT {DISPATCH_CONNECTION_LIMIT} PASSWORD {verifier};
    ALTER ROLE {DISPATCH} SET search_path = pg_catalog, pg_temp;
    ALTER ROLE {DISPATCH} SET event_triggers = off;
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_authid d WHERE d.rolname = '{DISPATCH}' AND d.rolpassword IS DISTINCT FROM {verifier}
      AND NOT EXISTS (SELECT FROM pg_catalog.pg_auth_members m WHERE m.roleid = d.oid OR m.member = d.oid)) THEN
    ALTER ROLE {DISPATCH} PASSWORD {verifier};
  END IF;
END $chelabase_fn$;
"#
        ));
    }
    format!(
        "{SYNC_MARK} role\n{PRELUDE}{make}SELECT 'fn_owner ' || oid FROM pg_catalog.pg_roles WHERE rolname = '{OWNER}';\n\
         SELECT 'fn_dispatch ' || oid FROM pg_catalog.pg_roles WHERE rolname = '{DISPATCH}';\nCOMMIT;\n"
    )
}

/// The OIDs [`role_sql`] read: [`OWNER`]'s and (m13b) [`DISPATCH`]'s. The checks and installs
/// that follow require exactly these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Roles {
    pub owner: Option<u32>,
    pub dispatch: Option<u32>,
}

/// The answer of [`role_sql`].
pub fn parse_role(out: &str) -> Roles {
    let oid = |prefix: &str| {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(prefix)?.parse().ok())
    };
    Roles {
        owner: oid("fn_owner "),
        dispatch: oid("fn_dispatch "),
    }
}

/// The foreign check (the invariant's "never adopts"), for the database it runs in: raises
/// [`FOREIGN_MARK`] with the reason when [`OWNER`] isn't the role `role` names (made by
/// [`role_sql`]), when the schema isn't its, when the role has any attribute, membership or
/// setting, when it owns anything beyond what the install gives it (`pg_shdepend` ownership
/// entries: shared objects, and this database's objects outside the schema; the worker runs
/// the check in every database of the compute), or when the schema holds anything the install
/// doesn't make. Refused, never repaired. An entry that only names the role as a grantee or a
/// policy's target (any role can make one) gives nobody its rights and never refuses
/// ([`revoke_sql`] takes the grants back).
///
/// m13b: likewise for [`DISPATCH`], whose allowlist is exact: the OID `roles` names, LOGIN
/// NOINHERIT with [`DISPATCH_CONNECTION_LIMIT`] and no other attribute (no expiry), exactly
/// [`DISPATCH_SETTINGS`], no member or membership, and nothing owned anywhere. Its grants are
/// [`revoke_sql`]'s to keep exact.
fn foreign_check_sql(roles: Roles) -> String {
    let oid = |r: Option<u32>| r.map_or_else(|| "NULL".to_string(), |r| r.to_string());
    let (expected, dexpected) = (oid(roles.owner), oid(roles.dispatch));
    let settings = DISPATCH_SETTINGS
        .iter()
        .map(|s| literal(s))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"DO $chelabase_fn$ DECLARE
  expected pg_catalog.oid := {expected};
  dexpected pg_catalog.oid := {dexpected};
  drole pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = '{DISPATCH}');
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
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_shdepend d WHERE d.refclassid = 'pg_catalog.pg_authid'::pg_catalog.regclass
      AND d.refobjid = role AND d.deptype = 'o'
      AND (d.dbid = 0 OR (d.dbid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database())
        AND NOT (
          (d.classid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND d.objid = ns)
          OR (d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.objid = ANY (ours))
          OR (d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
            AND d.objid = (SELECT p.oid FROM pg_catalog.pg_proc p WHERE p.pronamespace = ns AND p.proname = 'enqueue' AND p.pronargs = 0)))))) THEN
    why := 'the role {OWNER} owns objects outside {FUNCTIONS_SCHEMA}';
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_depend d WHERE d.refclassid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND d.refobjid = ns
      AND NOT ((d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
          AND d.objid IN (SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relnamespace = ns AND c.relname IN ('outbox', 'outbox_id_seq', 'outbox_pkey')))
        OR (d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
          AND d.objid IN (SELECT p.oid FROM pg_catalog.pg_proc p WHERE p.pronamespace = ns AND p.proname = 'enqueue' AND p.pronargs = 0))
        OR (d.classid = 'pg_catalog.pg_type'::pg_catalog.regclass
          AND d.objid IN (SELECT t.oid FROM pg_catalog.pg_type t WHERE t.typnamespace = ns AND t.typname IN ('outbox', '_outbox'))))) THEN
    why := 'the schema {FUNCTIONS_SCHEMA} holds objects Chelabase didn''t make';
  ELSIF drole IS DISTINCT FROM dexpected THEN
    why := 'the role {DISPATCH} was made by someone else';
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_authid a WHERE a.oid = drole AND (a.rolsuper OR a.rolinherit OR a.rolcreaterole
      OR a.rolcreatedb OR NOT a.rolcanlogin OR a.rolreplication OR a.rolbypassrls OR a.rolconnlimit <> {DISPATCH_CONNECTION_LIMIT}
      OR (a.rolvaliduntil IS NOT NULL AND a.rolvaliduntil <> 'infinity'))) THEN
    why := 'the role {DISPATCH} has attributes Chelabase never gives it';
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_auth_members m WHERE m.roleid = drole OR m.member = drole) THEN
    why := 'the role {DISPATCH} has members or memberships';
  ELSIF drole IS NOT NULL AND (EXISTS (SELECT FROM pg_catalog.pg_db_role_setting s WHERE s.setrole = drole AND s.setdatabase <> 0)
      OR (SELECT pg_catalog.array_agg(c ORDER BY c) FROM pg_catalog.pg_db_role_setting s CROSS JOIN LATERAL pg_catalog.unnest(s.setconfig) c
        WHERE s.setrole = drole AND s.setdatabase = 0) IS DISTINCT FROM ARRAY[{settings}]::pg_catalog.text[]) THEN
    why := 'the role {DISPATCH} has settings Chelabase never gives it';
  ELSIF EXISTS (SELECT FROM pg_catalog.pg_shdepend d WHERE d.refclassid = 'pg_catalog.pg_authid'::pg_catalog.regclass
      AND d.refobjid = drole AND d.deptype = 'o') THEN
    why := 'the role {DISPATCH} owns objects';
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
pub fn check_sql(roles: Roles) -> String {
    format!(
        "{SYNC_MARK} check\n{PRELUDE}{}ROLLBACK;\n",
        foreign_check_sql(roles)
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

/// The most candidates a sweep reads per pass: one page (it takes [`SWEEP_BATCH`] of them).
/// #302: the next pass reads the page after it ([`SweepCursor`]), so no candidate past the
/// first page is left out.
const SWEEP_CANDIDATES: i64 = 1000;

/// The two reconcile sweeps, each with its own attempt times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sweep {
    Triggers,
    RunAs,
}

/// Each sweep's state (this worker's memory: a restart starts with every branch unattempted,
/// from the lowest id).
pub type SweepAttempts = std::collections::HashMap<Sweep, SweepState>;

/// One sweep's state: when each branch was last attempted, and where its next page starts.
#[derive(Debug, Default)]
pub struct SweepState {
    pub attempts: std::collections::HashMap<BranchId, std::time::Instant>,
    pub cursor: SweepCursor,
}

/// #302: where a sweep reads its next page of candidates, a keyset on the branch id (the
/// page after [`Self::after`], or from the lowest id). A keyset, not an offset: a branch
/// added or removed between passes moves no other, so a pass from the lowest id to a short
/// page (a cycle) reads each branch that stays a candidate exactly once, and every cycle ends.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepCursor {
    after: Option<BranchId>,
}

/// The ids a page read covered: after `from` (from the lowest when none) up to and including
/// `to` (to the end when none: the page was short, so nothing follows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepWindow {
    pub from: Option<BranchId>,
    pub to: Option<BranchId>,
}

impl SweepWindow {
    pub fn contains(&self, branch: BranchId) -> bool {
        let id = branch.as_uuid();
        self.from.is_none_or(|f| id > f.as_uuid()) && self.to.is_none_or(|t| id <= t.as_uuid())
    }
}

impl SweepCursor {
    /// The id the next page starts after (none: from the lowest).
    pub fn after(self) -> Option<BranchId> {
        self.after
    }

    /// Moves past `page`, read in id order after `from` with `limit`: to its last id when the
    /// page is full (more may follow), else back to the lowest id (the page ran to the end).
    /// A cursor another pass moved meanwhile (it no longer stands at `from`) stays put. The
    /// window the page covered.
    pub fn advance(
        &mut self,
        from: Option<BranchId>,
        page: &[BranchId],
        limit: usize,
    ) -> SweepWindow {
        let to = if page.len() >= limit {
            page.last().copied()
        } else {
            None
        };
        if self.after == from {
            self.after = to;
        }
        SweepWindow { from, to }
    }
}

/// One pass of a sweep over `page` (its candidates read after `from` with `limit`): moves the
/// cursor, forgets the attempts of branches in the page's window that left the sweep (others
/// wait for their own page), and takes up to `n` ([`sweep_order`]).
pub fn sweep_pass(
    state: &mut SweepState,
    from: Option<BranchId>,
    page: &[(BranchId, bool)],
    limit: usize,
    n: usize,
) -> Vec<BranchId> {
    let ids: Vec<BranchId> = page.iter().map(|(b, _)| *b).collect();
    let window = state.cursor.advance(from, &ids, limit);
    state
        .attempts
        .retain(|b, _| !window.contains(*b) || ids.contains(b));
    sweep_order(page, &state.attempts, n)
}

/// A table trigger to install: its row's id, table and events, and (m17) its function's
/// approved run-as role OID on the branch, which the install and `enqueue()` check with
/// [`run_as_reads`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub id: Uuid,
    pub schema: String,
    pub table: String,
    pub events: Vec<String>,
    pub run_as_oid: u32,
}

/// m17: each function's approved run-as role OID on the branch (`run_as_role_oid`, while a
/// `run_as_role` is set), by function id. A function missing here has none.
pub type RunAsOids = std::collections::HashMap<Uuid, u32>;

/// m17: [`RunAsOids`] from the branch's functions. An OID outside Postgres's range can name no
/// role and is left out (so its triggers are never wanted).
pub fn run_as_oids(functions: &[chelabase_metadata::BranchFunction]) -> RunAsOids {
    functions
        .iter()
        .filter(|f| f.run_as_role.is_some())
        .filter_map(|f| Some((f.function_id, u32::try_from(f.run_as_role_oid?).ok()?)))
        .collect()
}

/// **Invariant (m17, read rule).** A table trigger's database trigger exists, is kept, and
/// captures a row only while its function's approved run-as role OID `run_as`:
///
/// 1. names an existing role (by OID, never by name, so a dropped and re-made namesake never
///    stands in);
/// 2. has table-level `SELECT` on the table (`has_table_privilege`, with membership
///    inheritance as Postgres applies it; a column grant is not enough, since every column is
///    delivered) and `USAGE` on its schema (`has_schema_privilege`: without it the role can't
///    reach the table at all); and
/// 3. when the table has row level security enabled, is outside it as Postgres decides
///    (`check_enable_rls`): the role is a superuser or has BYPASSRLS itself (never inherited),
///    or it has the privileges of the table's owner (`pg_has_role(…, 'USAGE')`, as Postgres's
///    owner check) and the table doesn't `FORCE ROW LEVEL SECURITY`.
///
/// This one fragment is used by the keep (drop loop), the create loop, the per-trigger answer
/// and the outbox cleanup in [`sync_sql`], and by `enqueue()` at capture time
/// ([`enqueue_body`]), so they can't drift. `run_as` is an `oid` expression and `class` a
/// `pg_class` row alias in scope. The `CASE` guards make the privilege functions run only for
/// an OID `pg_roles` holds; its own alias (`chelabase_run_as`) never captures a caller's name.
/// It reads catalogs and calls built-in functions only, so it runs no
/// user code (policies are never evaluated). `run_as_rule_model_agrees_with_postgres` in
/// `tests/fn_triggers_live.rs` checks it against what Postgres itself lets the role read.
pub fn run_as_reads(run_as: &str, class: &str) -> String {
    format!(
        "({} AND {})",
        run_as_can_select(run_as, class),
        run_as_outside_rls(run_as, class)
    )
}

/// Rule parts 1 and 2 of [`run_as_reads`]: the role exists, has table-level `SELECT` and
/// has `USAGE` on the table's schema.
pub fn run_as_can_select(run_as: &str, class: &str) -> String {
    format!(
        "(CASE WHEN EXISTS (SELECT FROM pg_catalog.pg_roles chelabase_run_as WHERE chelabase_run_as.oid = {run_as}) \
THEN pg_catalog.has_table_privilege({run_as}, {class}.oid, 'SELECT') \
AND pg_catalog.has_schema_privilege({run_as}, {class}.relnamespace, 'USAGE') ELSE false END)"
    )
}

/// Rule part 3 of [`run_as_reads`]: no row level security on the table, or the role is
/// outside it.
fn run_as_outside_rls(run_as: &str, class: &str) -> String {
    format!(
        "(NOT {class}.relrowsecurity \
OR EXISTS (SELECT FROM pg_catalog.pg_roles chelabase_run_as WHERE chelabase_run_as.oid = {run_as} AND (chelabase_run_as.rolsuper OR chelabase_run_as.rolbypassrls)) \
OR (NOT {class}.relforcerowsecurity AND CASE WHEN EXISTS (SELECT FROM pg_catalog.pg_roles chelabase_run_as WHERE chelabase_run_as.oid = {run_as}) \
THEN pg_catalog.pg_has_role({run_as}, {class}.relowner, 'USAGE') ELSE false END))"
    )
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
/// A failed row is never wanted (its database trigger is dropped, never installed), nor is a
/// disabled one (m13: its database trigger is dropped and the cleanup deletes its queued
/// events, so a disabled trigger queues nothing; a disabled *function*'s triggers stay and
/// queue, delivered by the functions service).
///
/// m17: nor is one whose function has no run-as role OID in `run_as` (it settles
/// [`RUN_AS_ROLE_REQUIRED`]); each wanted row carries that OID for the read rule.
pub fn wanted_in(triggers: &[TriggerRecord], db: &str, run_as: &RunAsOids) -> Vec<Wanted> {
    triggers
        .iter()
        .filter(|t| t.pg_database == db && wanted(t, run_as))
        .filter_map(|t| {
            Some(Wanted {
                id: t.id,
                schema: t.schema_name.clone()?,
                table: t.table_name.clone()?,
                events: t.events.clone(),
                run_as_oid: *run_as.get(&t.function_id)?,
            })
        })
        .collect()
}

/// A table trigger row whose database trigger should be in place: [`installable`] and (m17)
/// its function has a run-as role OID.
fn wanted(t: &TriggerRecord, run_as: &RunAsOids) -> bool {
    installable(t) && run_as.contains_key(&t.function_id)
}

/// A table trigger row the install would consider: enabled, not failed and (m13b) not on a
/// reserved schema.
fn installable(t: &TriggerRecord) -> bool {
    t.source == TriggerSource::Table
        && t.enabled
        && t.state != TriggerState::Failed
        && refusal(t).is_none()
}

/// m17: why an installable row is never wanted for want of a run-as role
/// ([`RUN_AS_ROLE_REQUIRED`]), for the settle.
pub fn run_as_refusal(t: &TriggerRecord, run_as: &RunAsOids) -> Option<&'static str> {
    (installable(t) && !run_as.contains_key(&t.function_id)).then_some(RUN_AS_ROLE_REQUIRED)
}

/// m13b (B2): why a table trigger row is never installed whatever the database holds: on a
/// reserved schema ([`RESERVED_SCHEMA`]), which the trigger API refuses too.
pub fn refusal(t: &TriggerRecord) -> Option<&'static str> {
    t.schema_name
        .as_deref()
        .is_some_and(chelabase_schema::exclusions::function_trigger_schema_refused)
        .then_some(RESERVED_SCHEMA)
}

/// The predicate "trigger `tg` on `c` in `n` is exactly the wanted `w`", for the script: its
/// shape, with the arguments `enqueue('<trigger id>', '<run-as OID>')` (m17: a changed run-as
/// OID replaces the trigger). Whether `w.run_as` may read `c` is [`run_as_reads`]'s, added
/// where a trigger is kept, made or answered for.
const EXACT: &str = "w.name = tg.tgname AND w.schema_name = n.nspname AND w.table_name = c.relname
      AND tg.tgfoid = pg_catalog.to_regprocedure('chelabase_functions.enqueue()')
      AND tg.tgtype = w.tgtype AND tg.tgenabled = 'O' AND tg.tgnargs = 2
      AND tg.tgargs = pg_catalog.convert_to(w.trigger_id::pg_catalog.text, 'UTF8') || '\\x00'::pg_catalog.bytea
        || pg_catalog.convert_to(w.run_as::pg_catalog.text, 'UTF8') || '\\x00'::pg_catalog.bytea
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

/// The body of `enqueue()` as the catalog stores it (`prosrc`): the install compares it on
/// every run and replaces the function only when it differs.
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
///
/// m17: first, it queues nothing unless the trigger's recorded run-as OID (its second
/// argument) passes [`run_as_reads`] on the table right now, so a revoked `SELECT`, newly
/// enabled or forced RLS, a removed BYPASSRLS or a changed owner stops capture at once, not
/// at the next sync. The check reads catalogs only (no policy is evaluated).
fn enqueue_body() -> String {
    let reads = run_as_reads("ra", "c");
    format!(
        r#"
DECLARE
  expr pg_catalog.text;
  rec pg_catalog.jsonb;
  old_rec pg_catalog.jsonb;
  ra pg_catalog.oid;
BEGIN
  IF TG_NARGS <> 2 THEN
    RETURN NULL;
  END IF;
  ra := TG_ARGV[1]::pg_catalog.oid;
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_class c WHERE c.oid = TG_RELID AND {reads}) THEN
    RETURN NULL;
  END IF;
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
      'record', rec, 'old_record', old_rec, 'txid', pg_catalog.txid_current()),
    pg_catalog.current_setting('neon.timeline_id', true));
  PERFORM pg_catalog.pg_notify('{NOTIFY_CHANNEL}', '');
  RETURN NULL;
END"#
    )
}

/// The `search_path` `enqueue()` is created with, as `pg_proc.proconfig` stores it.
const ENQUEUE_SEARCH_PATH: &str = "pg_catalog, chelabase_functions, pg_temp";

/// The schema, the outbox and the trigger function, each made or put back only when the
/// catalog shows it missing or wrong (the third invariant: a pass over a database in good
/// order runs no DDL, so it takes no lock a tenant write conflicts with; `ALTER TABLE …
/// OWNER` alone would take an `ACCESS EXCLUSIVE` lock on the outbox every pass). The repair
/// ran first, so an outbox that is there has the right shape and owner. `enqueue()` is
/// replaced when its body ([`enqueue_body`]), security, `search_path`, owner, language or
/// return type differ; a function whose ACL is still the default (`PUBLIC` may execute) gets
/// the revoke. Every other grant is [`revoke_sql`]'s.
fn install_objects_sql() -> String {
    let body = enqueue_body();
    let expected_body = literal(&body);
    let search_path = literal(&format!("search_path={ENQUEUE_SEARCH_PATH}"));
    format!(
        r#"DO $chelabase_fn$ DECLARE
  role pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = '{OWNER}');
  ns pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = '{FUNCTIONS_SCHEMA}');
  box pg_catalog.oid;
  fn record;
BEGIN
  IF ns IS NULL THEN
    CREATE SCHEMA {FUNCTIONS_SCHEMA} AUTHORIZATION {OWNER};
    REVOKE ALL ON SCHEMA {FUNCTIONS_SCHEMA} FROM PUBLIC;
    ns := (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = '{FUNCTIONS_SCHEMA}');
  END IF;
  box := (SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relnamespace = ns AND c.relname = 'outbox');
  IF box IS NULL THEN
    CREATE TABLE {FUNCTIONS_SCHEMA}.outbox (
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
  END IF;
  SELECT p.prosrc, p.prosecdef, p.proconfig, p.proowner, p.proacl, p.prolang, p.prorettype, p.prokind INTO fn
    FROM pg_catalog.pg_proc p WHERE p.pronamespace = ns AND p.proname = 'enqueue' AND p.pronargs = 0;
  IF NOT FOUND OR fn.prosrc <> {expected_body} OR NOT fn.prosecdef
      OR fn.proconfig IS DISTINCT FROM ARRAY[{search_path}]::pg_catalog.text[]
      OR fn.proowner <> role OR fn.prokind <> 'f'
      OR fn.prolang <> (SELECT l.oid FROM pg_catalog.pg_language l WHERE l.lanname = 'plpgsql')
      OR fn.prorettype <> 'pg_catalog.trigger'::pg_catalog.regtype THEN
    EXECUTE $chelabase_fn_ddl$CREATE OR REPLACE FUNCTION {FUNCTIONS_SCHEMA}.enqueue() RETURNS trigger
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = {ENQUEUE_SEARCH_PATH} AS $chelabase_fn_body${body}$chelabase_fn_body$$chelabase_fn_ddl$;
    ALTER FUNCTION {FUNCTIONS_SCHEMA}.enqueue() OWNER TO {OWNER};
    REVOKE ALL ON FUNCTION {FUNCTIONS_SCHEMA}.enqueue() FROM PUBLIC;
  ELSIF fn.proacl IS NULL THEN
    REVOKE ALL ON FUNCTION {FUNCTIONS_SCHEMA}.enqueue() FROM PUBLIC;
  END IF;
END $chelabase_fn$;
"#
    )
}

/// Every grant on the schema, its tables and sequences and its routines to anyone but the
/// object's owner is revoked, `CASCADE` (as the superuser, the revoke acts for the owner, so
/// every grant chain from the owner goes; the owner is the only one who granted first). Then
/// every grant *to* [`OWNER`] elsewhere (`pg_shdepend` grantee entries: on a table, column,
/// sequence, routine, schema, type, database, large object, default privilege, language,
/// foreign-data wrapper or server, tablespace or parameter) is revoked: it gives nobody the
/// role's rights, and the foreign check let it pass. A policy naming the role is left as it
/// is (taking the role out of it would change the table owner's row security), as is any
/// other entry this doesn't know.
///
/// m13b: [`DISPATCH`]'s grants are an exact allowlist ([`FUNCTIONS_DISPATCH_SCHEMA_GRANTS`],
/// [`FUNCTIONS_DISPATCH_OUTBOX_GRANTS`]). Its entries on
/// the schema and the outbox are left to the exact step: when they aren't exactly those
/// privileges, granted by the owner without grant option, every privilege it holds there is
/// revoked and exactly those are granted (DDL only when the catalog differs, as the install).
/// Every other grant to it, anywhere (the outbox's columns, the sequence, `enqueue()`, any
/// tenant object), is revoked like the owner's.
fn revoke_sql() -> String {
    let ours = ours_sql();
    let owner_grants = grants_to_sql(OWNER, "");
    let (schema_want, schema_grant) = exact_grants(FUNCTIONS_DISPATCH_SCHEMA_GRANTS);
    let (box_want, box_grant) = exact_grants(FUNCTIONS_DISPATCH_OUTBOX_GRANTS);
    let dispatch_grants = grants_to_sql(
        DISPATCH,
        "\n      AND NOT ((d.classid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND d.objid = ours)\n        OR (d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.objsubid = 0 AND d.objid = box))",
    );
    format!(
        r#"DO $chelabase_fn$ DECLARE
  ours pg_catalog.oid := {ours};
  drole pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = '{DISPATCH}');
  box pg_catalog.oid;
  g record;
  da record;
BEGIN
  IF ours IS NULL THEN
    RETURN;
  END IF;
  box := (SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relnamespace = ours AND c.relname = 'outbox' AND c.relkind = 'r');
  FOR g IN
    SELECT 'SCHEMA {FUNCTIONS_SCHEMA}' AS obj, a.grantee
      FROM pg_catalog.pg_namespace n CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) a
      WHERE n.oid = ours AND a.grantee <> n.nspowner AND a.grantee IS DISTINCT FROM drole
    UNION
    SELECT CASE c.relkind WHEN 'S' THEN 'SEQUENCE ' ELSE 'TABLE ' END || c.oid::pg_catalog.regclass::pg_catalog.text, a.grantee
      FROM pg_catalog.pg_class c CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) a
      WHERE c.relnamespace = ours AND c.relkind IN ('r', 'S') AND a.grantee <> c.relowner
        AND NOT (c.oid = box AND a.grantee IS NOT DISTINCT FROM drole)
    UNION
    SELECT 'ROUTINE ' || p.oid::pg_catalog.regprocedure::pg_catalog.text, a.grantee
      FROM pg_catalog.pg_proc p CROSS JOIN LATERAL pg_catalog.aclexplode(p.proacl) a
      WHERE p.pronamespace = ours AND a.grantee <> p.proowner
  LOOP
    EXECUTE pg_catalog.format('REVOKE ALL ON %s FROM %s CASCADE', g.obj,
      CASE WHEN g.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.quote_ident(pg_catalog.pg_get_userbyid(g.grantee)) END);
  END LOOP;
  -- m13b: the dispatch login's exact grants on the schema and the outbox.
  IF drole IS NOT NULL THEN
    IF (SELECT pg_catalog.array_agg(a.privilege_type || ':' || a.is_grantable || ':' || (a.grantor = n.nspowner)
          ORDER BY a.privilege_type)
        FROM pg_catalog.pg_namespace n CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) a
        WHERE n.oid = ours AND a.grantee = drole) IS DISTINCT FROM {schema_want} THEN
      REVOKE ALL ON SCHEMA {FUNCTIONS_SCHEMA} FROM {DISPATCH} CASCADE;
      GRANT {schema_grant} ON SCHEMA {FUNCTIONS_SCHEMA} TO {DISPATCH};
    END IF;
    IF box IS NOT NULL AND (SELECT pg_catalog.array_agg(a.privilege_type || ':' || a.is_grantable || ':' || (a.grantor = c.relowner)
          ORDER BY a.privilege_type)
        FROM pg_catalog.pg_class c CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) a
        WHERE c.oid = box AND a.grantee = drole)
        IS DISTINCT FROM {box_want} THEN
      REVOKE ALL ON TABLE {FUNCTIONS_SCHEMA}.outbox FROM {DISPATCH} CASCADE;
      GRANT {box_grant} ON TABLE {FUNCTIONS_SCHEMA}.outbox TO {DISPATCH};
    END IF;
  END IF;
{owner_grants}{dispatch_grants}END $chelabase_fn$;
"#
    )
}

/// For an exact grant of `privileges` (sorted, as `aclexplode` entries ordered by privilege
/// sort): the `privilege:grantable:by owner` array the catalog must hold, and the `GRANT` list.
fn exact_grants(privileges: &[&str]) -> (String, String) {
    let want: Vec<String> = privileges.iter().map(|p| format!("'{p}:false:true'")).collect();
    (format!("ARRAY[{}]::pg_catalog.text[]", want.join(", ")), privileges.join(", "))
}

/// The loop that revokes every grant *to* `role` outside the schema (see [`revoke_sql`]):
/// `pg_shdepend` grantee entries of this database, and of the shared catalogs a grant can
/// name, each of a class it knows, except the entries `keep` (an SQL condition on `d`, with
/// `ours` and `box` in scope) leaves out. An entry of a class this doesn't know is left alone.
fn grants_to_sql(role: &str, keep: &str) -> String {
    const LOOP: &str = r#"  -- Grants *to* {ROLE} on other objects (any role's, on what it owns): the foreign check
  -- lets them pass (they give nobody the role's rights), and they are revoked here. An entry
  -- of a class this doesn't know is left alone.
  FOR g IN SELECT d.classid, d.objid, d.objsubid FROM pg_catalog.pg_shdepend d
    WHERE d.refclassid = 'pg_catalog.pg_authid'::pg_catalog.regclass
      AND d.refobjid = (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = '{ROLE}') AND d.deptype = 'a'{KEEP}
      AND (d.dbid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database())
        OR (d.dbid = 0 AND d.classid IN ('pg_catalog.pg_database'::pg_catalog.regclass,
          'pg_catalog.pg_tablespace'::pg_catalog.regclass, 'pg_catalog.pg_parameter_acl'::pg_catalog.regclass)))
  LOOP
    IF g.classid = 'pg_catalog.pg_class'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL %s ON %s %s FROM {ROLE} CASCADE',
        CASE WHEN g.objsubid = 0 THEN '' ELSE pg_catalog.format('(%I)', (SELECT a.attname FROM pg_catalog.pg_attribute a
          WHERE a.attrelid = g.objid AND a.attnum = g.objsubid)) END,
        CASE WHEN (SELECT c.relkind FROM pg_catalog.pg_class c WHERE c.oid = g.objid) = 'S' THEN 'SEQUENCE' ELSE 'TABLE' END,
        g.objid::pg_catalog.regclass);
    ELSIF g.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON ROUTINE %s FROM {ROLE} CASCADE', g.objid::pg_catalog.regprocedure);
    ELSIF g.classid = 'pg_catalog.pg_namespace'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON SCHEMA %I FROM {ROLE} CASCADE', (SELECT n.nspname FROM pg_catalog.pg_namespace n WHERE n.oid = g.objid));
    ELSIF g.classid = 'pg_catalog.pg_type'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON %s %s FROM {ROLE} CASCADE',
        CASE WHEN (SELECT t.typtype FROM pg_catalog.pg_type t WHERE t.oid = g.objid) = 'd' THEN 'DOMAIN' ELSE 'TYPE' END,
        g.objid::pg_catalog.regtype);
    ELSIF g.classid = 'pg_catalog.pg_database'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON DATABASE %I FROM {ROLE} CASCADE', (SELECT d.datname FROM pg_catalog.pg_database d WHERE d.oid = g.objid));
    ELSIF g.classid = 'pg_catalog.pg_largeobject'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON LARGE OBJECT %s FROM {ROLE} CASCADE', g.objid);
    ELSIF g.classid = 'pg_catalog.pg_default_acl'::pg_catalog.regclass THEN
      SELECT a.defaclrole, a.defaclnamespace, a.defaclobjtype INTO da FROM pg_catalog.pg_default_acl a WHERE a.oid = g.objid;
      IF FOUND THEN
        EXECUTE pg_catalog.format('ALTER DEFAULT PRIVILEGES FOR ROLE %I%s REVOKE ALL ON %s FROM {ROLE} CASCADE',
          pg_catalog.pg_get_userbyid(da.defaclrole),
          CASE WHEN da.defaclnamespace = 0 THEN '' ELSE pg_catalog.format(' IN SCHEMA %I',
            (SELECT n.nspname FROM pg_catalog.pg_namespace n WHERE n.oid = da.defaclnamespace)) END,
          CASE da.defaclobjtype WHEN 'r' THEN 'TABLES' WHEN 'S' THEN 'SEQUENCES' WHEN 'f' THEN 'FUNCTIONS'
            WHEN 'T' THEN 'TYPES' WHEN 'n' THEN 'SCHEMAS' END);
      END IF;
    ELSIF g.classid = 'pg_catalog.pg_language'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON LANGUAGE %I FROM {ROLE} CASCADE', (SELECT l.lanname FROM pg_catalog.pg_language l WHERE l.oid = g.objid));
    ELSIF g.classid = 'pg_catalog.pg_foreign_data_wrapper'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON FOREIGN DATA WRAPPER %I FROM {ROLE} CASCADE', (SELECT w.fdwname FROM pg_catalog.pg_foreign_data_wrapper w WHERE w.oid = g.objid));
    ELSIF g.classid = 'pg_catalog.pg_foreign_server'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON FOREIGN SERVER %I FROM {ROLE} CASCADE', (SELECT s.srvname FROM pg_catalog.pg_foreign_server s WHERE s.oid = g.objid));
    ELSIF g.classid = 'pg_catalog.pg_tablespace'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON TABLESPACE %I FROM {ROLE} CASCADE', (SELECT t.spcname FROM pg_catalog.pg_tablespace t WHERE t.oid = g.objid));
    ELSIF g.classid = 'pg_catalog.pg_parameter_acl'::pg_catalog.regclass THEN
      EXECUTE pg_catalog.format('REVOKE ALL ON PARAMETER %s FROM {ROLE} CASCADE', (SELECT p.parname FROM pg_catalog.pg_parameter_acl p WHERE p.oid = g.objid));
    END IF;
  END LOOP;
"#;
    LOOP.replace("{ROLE}", role).replace("{KEEP}", keep)
}

/// The script that brings one database in step with `wanted` (see the module docs);
/// `truncate` empties the outbox first. Without wanted triggers it installs nothing, and only
/// repairs, drops leftovers and cleans the outbox of a schema [`OWNER`] owns.
pub fn sync_sql(wanted: &[Wanted], truncate: bool, roles: Roles) -> String {
    let rows: Vec<String> = wanted
        .iter()
        .filter_map(|w| {
            let (events, tgtype) = trigger_events(&w.events)?;
            Some(format!(
                "({}, {}, {}, {}, {}, {tgtype}, {})",
                literal(&trigger_name(w.id)),
                literal(&w.id.to_string()),
                literal(&w.schema),
                literal(&w.table),
                literal(&events),
                w.run_as_oid,
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
    let check = foreign_check_sql(roles);
    let (repair, revoke, ours) = (repair_sql(), revoke_sql(), ours_sql());
    let truncate = if truncate {
        format!(
            "  IF pg_catalog.current_setting('chelabase_fn.fresh', true) = 'true' THEN\n    TRUNCATE {FUNCTIONS_SCHEMA}.outbox;\n  END IF;\n"
        )
    } else {
        String::new()
    };
    // m17: the read rule, one fragment for the keep, the create, the cleanup and the answer.
    let reads = run_as_reads("w.run_as", "c");
    let can_select = run_as_can_select("w.run_as", "c");
    // The advisory key as `pg_locks` shows it: the high and low 32 bits.
    let (key_hi, key_lo) = ((SYNC_LOCK_KEY >> 32) as u32, (SYNC_LOCK_KEY & 0xffff_ffff) as u32);
    format!(
        r#"{SYNC_MARK}
{PRELUDE}-- Two syncs of one database never run at once, and nobody waits on the key: a superuser's
-- session holding it is another sync (busy, retried); any other holder squats on it and is ignored.
DO $chelabase_fn$ BEGIN
  IF NOT pg_catalog.pg_try_advisory_xact_lock({SYNC_LOCK_KEY}) AND EXISTS (
      SELECT FROM pg_catalog.pg_locks l JOIN pg_catalog.pg_stat_activity s ON s.pid = l.pid
        JOIN pg_catalog.pg_roles r ON r.oid = s.usesysid
      WHERE l.locktype = 'advisory' AND l.granted AND l.objsubid = 1 AND l.classid = {key_hi} AND l.objid = {key_lo}
        AND l.database = (SELECT d.oid FROM pg_catalog.pg_database d WHERE d.datname = pg_catalog.current_database())
        AND r.rolsuper) THEN
    RAISE EXCEPTION '{BUSY_MARK}: another sync of this database holds the lock';
  END IF;
END $chelabase_fn$;
CREATE TEMP TABLE chelabase_fn_wanted (name text, trigger_id uuid, schema_name text, table_name text, events text, tgtype int2, run_as oid) ON COMMIT DROP;
{insert}-- After the lock. `fresh`: none of this branch's wanted triggers (its own trigger ids) is
-- exactly in place, so the database holds only what it came with (the source's triggers, under
-- the source's ids) and its outbox only the source's rows: a create, reset or restore truncates
-- it. Once one is in place, a sync of this branch already ran here and its triggers may have
-- queued this branch's own events since, so nothing is truncated. Either way the cleanup below
-- deletes every row of another timeline or of an unwanted trigger.
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
      AND NOT EXISTS (SELECT FROM pg_temp.chelabase_fn_wanted w WHERE {EXACT} AND {reads})
  LOOP
    EXECUTE pg_catalog.format('DROP TRIGGER %I ON %I.%I', t.tgname, t.nspname, t.relname);
  END LOOP;
  FOR t IN SELECT w.* FROM pg_temp.chelabase_fn_wanted w
    JOIN pg_catalog.pg_namespace n ON n.nspname = w.schema_name
    JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = w.table_name AND c.relkind = 'r'
    WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_trigger tg WHERE tg.tgrelid = c.oid AND tg.tgname = w.name)
      AND {reads}
  LOOP
    EXECUTE pg_catalog.format('CREATE TRIGGER %I AFTER %s ON %I.%I FOR EACH ROW EXECUTE FUNCTION {FUNCTIONS_SCHEMA}.enqueue(%L, %L)',
      t.name, t.events, t.schema_name, t.table_name, t.trigger_id, t.run_as);
  END LOOP;
END $chelabase_fn$;
DO $chelabase_fn$ DECLARE
  ours pg_catalog.oid := {ours};
  dropped pg_catalog.text;
  -- m17: the wanted triggers whose events stay: all but those whose table is there and
  -- whose run-as role may not read it (read here, as the superuser, before the role switch).
  keep pg_catalog.uuid[] := ARRAY(SELECT w.trigger_id FROM pg_temp.chelabase_fn_wanted w
    WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid
      WHERE n.nspname = w.schema_name AND c.relname = w.table_name AND NOT {reads}));
BEGIN
  IF ours IS NULL OR pg_catalog.to_regclass('{FUNCTIONS_SCHEMA}.outbox') IS NULL THEN
    RETURN;
  END IF;
  -- The outbox was repaired above; its cleanup runs as its owner, never as the superuser.
  SET LOCAL ROLE {OWNER};
{truncate}  -- m13b fix (S2): this timeline's rows it removes, per trigger, from the DELETE itself.
  WITH d AS (DELETE FROM {FUNCTIONS_SCHEMA}.outbox
      WHERE timeline_id IS DISTINCT FROM pg_catalog.current_setting('neon.timeline_id', true)
         OR trigger_id <> ALL (keep)
      RETURNING trigger_id, timeline_id IS NOT DISTINCT FROM pg_catalog.current_setting('neon.timeline_id', true) AS here)
  SELECT pg_catalog.string_agg(c.t || ':' || c.n, ' ') INTO dropped
    FROM (SELECT d.trigger_id::pg_catalog.text AS t, pg_catalog.count(*)::pg_catalog.text AS n FROM d WHERE d.here GROUP BY 1) c;
  RESET ROLE;
  PERFORM pg_catalog.set_config('chelabase_fn.dropped', coalesce(dropped, ''), true);
END $chelabase_fn$;
SELECT 'fn_dropped ' || coalesce(pg_catalog.current_setting('chelabase_fn.dropped', true), '');
SELECT 'fn_trigger ' || w.trigger_id || CASE WHEN EXISTS (SELECT FROM pg_catalog.pg_trigger tg
    JOIN pg_catalog.pg_class c ON c.oid = tg.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    WHERE {EXACT} AND {reads}) THEN ' ok'
    WHEN EXISTS (SELECT FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = w.schema_name AND c.relname = w.table_name AND NOT {can_select}) THEN ' no_select'
    WHEN EXISTS (SELECT FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = w.schema_name AND c.relname = w.table_name AND NOT {reads}) THEN ' subject_to_rls'
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

/// m13b: the [`DISPATCH`] verifier of `password` with its stored `salt` (SCRAM, Postgres's
/// default iteration count), so the install sets exactly it.
pub fn dispatch_verifier(password: &str, salt: &[u8]) -> String {
    chelabase_neon::scram::verifier_with_salt(password, salt, 4096)
}

/// m13b fix (S2): the cleanup's `fn_dropped <id>:<n> …` line: the rows of each trigger it
/// removed from this timeline.
pub fn parse_dropped(out: &str) -> Vec<(Uuid, i64)> {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix("fn_dropped"))
        .flat_map(|rest| rest.split_whitespace())
        .filter_map(|pair| {
            let (id, n) = pair.split_once(':')?;
            Some((id.parse().ok()?, n.parse().ok()?))
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
    pub(crate) async fn ready_endpoint(
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
    ///
    /// `Ok(true)` when every database of the branch was brought in step (or refused, with its
    /// triggers dropped); `Ok(false)` when nothing ran (the branch isn't ready) or a database
    /// yielded to a tenant's lock or another sync (m13: a recorded drop is cleared only after
    /// a complete sync).
    pub async fn sync_fn_triggers(&self, branch: BranchId, truncate: bool) -> Result<bool, String> {
        let Some(meta) = self.metadata.functions() else {
            return Ok(false);
        };
        let Some((_, ep)) = self.ready_endpoint(branch).await? else {
            return Ok(false);
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
        // m17: each function's approved run-as OID; a trigger without one is never wanted.
        let run_as = run_as_oids(
            &meta
                .list_for_branch(branch)
                .await
                .map_err(|e| format!("listing the functions: {e}"))?,
        );
        let wanted_any = triggers.iter().any(|t| wanted(t, &run_as));
        // m13b: the dispatch login's verifier, made and stored once per branch.
        let verifier = if wanted_any {
            Some(self.dispatch_verifier(meta, branch).await?)
        } else {
            None
        };
        let role = self
            .computes
            .admin_sql(
                &ep,
                SYSTEM_DATABASE,
                &role_sql(wanted_any, verifier.as_deref()),
            )
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
                            .map(|()| true)
                    }
                    None => Err(format!("checking the function triggers' role in {db}: {e}")),
                };
            }
        }
        let mut answers: Vec<(Uuid, Option<String>)> = Vec::new();
        let mut dropped: Vec<(Uuid, i64)> = Vec::new();
        let mut failure = None;
        let mut complete = true;
        // The databases whose script ran to the end in this pass (or that don't exist, so
        // hold no trigger): only there is a trigger the database doesn't want known to be
        // dropped and its events removed.
        let mut synced: Vec<&str> = Vec::new();
        for db in &databases {
            let wanted = wanted_in(&triggers, db, &run_as);
            match self
                .computes
                .admin_sql(&ep, db, &sync_sql(&wanted, truncate, role))
                .await
            {
                Ok(out) => {
                    synced.push(db);
                    answers.extend(parse_sync(&out));
                    dropped.extend(parse_dropped(&out));
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    let why = if database_missing(&e, db) {
                        synced.push(db);
                        Some("database_missing".to_string())
                    } else {
                        foreign_refusal(&e)
                    };
                    match why {
                        Some(why) => {
                            answers.extend(wanted.iter().map(|w| (w.id, Some(why.clone()))))
                        }
                        // A lock a tenant held, or another sync of the database: the rows
                        // stay as they are and the next pass comes back. Not a failure.
                        None if retry_later(&e) => {
                            complete = false;
                            info!(branch_id = %branch, database = %db, "function triggers' sync yields: {}", e.lines().find(|l| l.contains("ERROR")).unwrap_or(&e));
                        }
                        None => {
                            warn!(branch_id = %branch, database = %db, "syncing the function triggers: {e}");
                            failure = Some(format!("syncing the function triggers of {db}: {e}"));
                        }
                    }
                }
            }
        }
        // A row the database doesn't want (reserved schema, no run-as role, bad events)
        // settles `failed` only once its database's script ran in this pass, which dropped
        // any database trigger it still had and removed its queued events. Otherwise it keeps
        // its state, so the branch stays in the sweep and the next pass comes back; a
        // `failed` row would take it out with its database trigger still in place.
        self.settle(meta, branch, &triggers, |t| {
            let unwanted = refusal(t).or_else(|| run_as_refusal(t, &run_as));
            if unwanted.is_some() && !synced.contains(&t.pg_database.as_str()) {
                return None;
            }
            if let Some(why) = unwanted {
                return Some(Some(why.to_string()));
            }
            match answers.iter().find(|(id, _)| *id == t.id) {
                Some((_, error)) => Some(error.as_deref().map(install_error)),
                // Bad events never reach the script (the schema's CHECK refuses them).
                None if trigger_events(&t.events).is_none()
                    && synced.contains(&t.pg_database.as_str()) =>
                {
                    Some(Some("invalid_events".into()))
                }
                None => None,
            }
        })
        .await?;
        // m13b fix (S2): the queued events of a trigger no longer wanted (turned off, failed)
        // count in its `dropped_count`, as when the dispatcher removes them.
        let drops: Vec<TriggerDelivery> = dropped
            .into_iter()
            .filter(|(id, _)| triggers.iter().any(|t| t.id == *id))
            .map(|(id, n)| TriggerDelivery {
                id,
                delivered: false,
                dead: 0,
                dropped: n,
                error: None,
            })
            .collect();
        if !drops.is_empty()
            && let Err(e) = meta.record_trigger_deliveries(branch, &drops).await
        {
            warn!(branch_id = %branch, "recording the dropped events: {e}");
        }
        failure.map_or(Ok(complete), Err)
    }

    /// m13b: the SCRAM verifier of the branch's [`DISPATCH`] password. A new password (sealed
    /// `Purpose::FunctionsDispatch`, AAD = branch id, with a random 16-byte salt) is stored when
    /// the branch has none; like the storage login's, it is read back, so a racing run's wins.
    /// The orchestrator hands the password to the functions service only
    /// (`/internal/functions/branches/{id}/connection`).
    async fn dispatch_verifier(
        &self,
        meta: &MetadataClient,
        branch: BranchId,
    ) -> Result<String, String> {
        use chelabase_secrets::{Purpose, open_credential, seal_credential};
        let stored = || async {
            let row = meta
                .branch_connection(branch)
                .await
                .map_err(|e| format!("reading the dispatch login: {e}"))?;
            row.and_then(|r| r.functions_password_enc.zip(r.functions_password_salt))
                .map(|(sealed, salt)| {
                    open_credential(
                        &self.key,
                        &branch.to_string(),
                        Purpose::FunctionsDispatch,
                        &sealed,
                    )
                    .map(|password| (password, salt))
                    .map_err(|e| format!("opening the dispatch login's password: {e}"))
                })
                .transpose()
        };
        let login = match stored().await? {
            Some(login) => login,
            None => {
                let password = crate::credentials::generate_password();
                let salt: [u8; 16] = rand::random();
                let sealed = seal_credential(
                    &self.key,
                    &branch.to_string(),
                    Purpose::FunctionsDispatch,
                    &password,
                );
                meta.set_functions_password(branch, &sealed, &salt)
                    .await
                    .map_err(|e| format!("storing the dispatch login's password: {e}"))?;
                info!(branch_id = %branch, "made the branch's {DISPATCH} login");
                stored().await?.ok_or_else(|| {
                    "the dispatch login's password wasn't stored (is the branch being deleted?)"
                        .to_string()
                })?
            }
        };
        Ok(dispatch_verifier(&login.0, &login.1))
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
        let mut sweeps = self.fn_sweeps.lock().unwrap_or_else(|e| e.into_inner());
        sweeps
            .entry(kind)
            .or_default()
            .attempts
            .insert(branch, std::time::Instant::now());
    }

    /// Where the sweep of `kind` reads its next page ([`SweepCursor`]).
    fn sweep_cursor(&self, kind: Sweep) -> Option<BranchId> {
        let sweeps = self.fn_sweeps.lock().unwrap_or_else(|e| e.into_inner());
        sweeps.get(&kind).and_then(|s| s.cursor.after())
    }

    /// Up to [`SWEEP_BATCH`] of `candidates`, the page read after `from`, for the sweep of
    /// `kind` ([`sweep_pass`]).
    fn sweep_batch(
        &self,
        kind: Sweep,
        from: Option<BranchId>,
        candidates: &[(BranchId, bool)],
    ) -> Vec<BranchId> {
        let mut sweeps = self.fn_sweeps.lock().unwrap_or_else(|e| e.into_inner());
        let state = sweeps.entry(kind).or_default();
        sweep_pass(
            state,
            from,
            candidates,
            SWEEP_CANDIDATES as usize,
            SWEEP_BATCH as usize,
        )
    }

    /// The reconcile sweep (best effort): branches with a pending table trigger to install,
    /// then branches whose installed triggers are checked for drift (a dropped, disabled or
    /// changed database trigger is put back; one whose table is gone fails), each pass taking
    /// the least recently attempted. A `failed` row never comes back here.
    pub async fn sync_pending_fn_triggers(&self) {
        let Some(meta) = self.metadata.functions() else {
            return;
        };
        let from = self.sweep_cursor(Sweep::Triggers);
        let candidates = match meta.trigger_sweep_branches(from, SWEEP_CANDIDATES).await {
            Ok(candidates) => candidates,
            Err(e) => return warn!("listing branches with function triggers: {e}"),
        };
        for branch in self.sweep_batch(Sweep::Triggers, from, &candidates) {
            self.attempted(Sweep::Triggers, branch);
            // m13: drop, then forget. The drops read before a complete sync are cleared after it:
            // that sync no longer wanted their triggers, so it dropped them.
            let drops = match meta.trigger_drops(branch).await {
                Ok(drops) => drops,
                Err(e) => {
                    warn!(branch_id = %branch, "reading the function triggers to drop: {e}");
                    continue;
                }
            };
            match self.sync_fn_triggers(branch, false).await {
                Ok(true) if !drops.is_empty() => {
                    if let Err(e) = meta.clear_trigger_drops(branch, &drops).await {
                        warn!(branch_id = %branch, "clearing the dropped function triggers: {e}");
                    }
                }
                Ok(_) => {}
                Err(e) => warn!(branch_id = %branch, "function triggers not synced: {e}"),
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
        let from = self.sweep_cursor(Sweep::RunAs);
        let candidates: Vec<(BranchId, bool)> =
            match meta.pending_run_as_branches(from, SWEEP_CANDIDATES).await {
                Ok(branches) => branches.into_iter().map(|b| (b, true)).collect(),
                Err(e) => return warn!("listing branches with run-as keys to mint: {e}"),
            };
        for branch in self.sweep_batch(Sweep::RunAs, from, &candidates) {
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
    use chelabase_time::{Timestamp, TimestampExt};

    const ROLES: Roles = Roles {
        owner: Some(4242),
        dispatch: Some(4343),
    };
    const VERIFIER: &str = "SCRAM-SHA-256$4096:AAAA$BBBB:CCCC";

    fn wanted(table: &str, events: &[&str]) -> Wanted {
        Wanted {
            id: Uuid::parse_str("0123456789abcdef0123456789abcdef").unwrap(),
            schema: "public".into(),
            table: table.into(),
            events: events.iter().map(|e| e.to_string()).collect(),
            run_as_oid: 16500,
        }
    }

    /// m17: a run-as OID for each record's function.
    fn with_run_as(records: &[&TriggerRecord]) -> RunAsOids {
        records.iter().map(|t| (t.function_id, 16500)).collect()
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
        let sql = sync_sql(&[wanted(nasty, &["insert"])], false, ROLES);
        let row = sql
            .lines()
            .find(|l| l.contains("chelabase_fn_0123"))
            .unwrap();
        assert_eq!(
            row.trim(),
            format!(
                "('chelabase_fn_0123456789abcdef0123456789abcdef', '01234567-89ab-cdef-0123-456789abcdef', 'public', {}, 'INSERT', 5, 16500);",
                literal(nasty)
            )
        );
        assert_eq!(sql.matches(nasty).count(), 0, "never raw");
        assert!(sql.contains("pg_catalog.format('CREATE TRIGGER %I AFTER %s ON %I.%I FOR EACH ROW EXECUTE FUNCTION chelabase_functions.enqueue(%L, %L)'"));
        assert!(sql.contains("pg_catalog.format('DROP TRIGGER %I ON %I.%I'"));
        // Rows with events the schema refuses never reach the script.
        assert!(
            !sync_sql(&[wanted("t", &["truncate"])], false, ROLES)
                .contains("INSERT INTO pg_temp")
        );
    }

    /// The function's privileges and search path, the event triggers off, the outbox cleaned.
    #[test]
    fn fn_triggers_script_is_locked_down() {
        let sql = sync_sql(&[wanted("orders", &["insert"])], true, ROLES);
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
            "OR trigger_id <> ALL (keep)",
            "REVOKE ALL ON %s FROM %s CASCADE",
            "OR EXISTS (SELECT FROM pg_catalog.pg_trigger WHERE tgrelid = box)",
        ] {
            assert!(sql.contains(needle), "{needle}");
        }
        assert!(
            !sync_sql(&[wanted("orders", &["insert"])], false, ROLES).contains("TRUNCATE")
        );
        // Audit B1: the row is never handed to to_jsonb whole (a user's cast would run as the
        // owner); the cleanup runs as the owner, after the repair, never as the superuser.
        assert!(!sql.contains("to_jsonb(NEW)") && !sql.contains("to_jsonb(OLD)"));
        // Round 3 (N1, S2): the truncate is decided after the lock; grants to the role revoked.
        assert!(
            sql.find("pg_try_advisory_xact_lock").unwrap()
                < sql.find("'chelabase_fn.fresh'").unwrap()
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
        let cleanup = sync_sql(&[], true, ROLES);
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

    /// #302: the cursor alone: a full page moves it to the page's last id, a short one back
    /// to the lowest, and a cursor another pass moved stays put.
    #[test]
    fn fn_triggers_sweep_cursor_pages_and_wraps() {
        let id = |n: u128| BranchId(Uuid::from_u128(n));
        let mut c = SweepCursor::default();
        let w = c.advance(None, &[id(1), id(2)], 2);
        let full = SweepWindow {
            from: None,
            to: Some(id(2)),
        };
        assert_eq!((w, c.after()), (full, Some(id(2))));
        assert!(w.contains(id(1)) && w.contains(id(2)) && !w.contains(id(3)));
        let w = c.advance(Some(id(2)), &[id(5)], 2);
        let short = SweepWindow {
            from: Some(id(2)),
            to: None,
        };
        assert_eq!((w, c.after()), (short, None));
        assert!(!w.contains(id(2)) && w.contains(id(3)) && w.contains(id(u128::MAX)));
        // A pass that read from a cursor that has moved since doesn't move it again.
        c.advance(None, &[id(1), id(2)], 2);
        c.advance(None, &[id(1), id(2)], 2);
        assert_eq!(c.after(), Some(id(2)));
    }

    /// #302, the sweep's invariant as a model: with candidates added and removed between
    /// passes, a cycle (from the lowest id to a short page) reads no branch twice, reads every
    /// branch that stays a candidate throughout it, and ends within one pass more than its
    /// full pages; every branch that stays a candidate is attempted within a bounded number of
    /// passes; and the attempt times kept are those of branches still candidates, or removed
    /// since the start of the cycle before.
    #[test]
    fn fn_triggers_sweep_cursor_model() {
        use proptest::prelude::*;
        use std::collections::{BTreeSet, HashMap, HashSet};
        use std::time::{Duration, Instant};

        const LIMIT: usize = 10;
        const BATCH: usize = 3;
        let id = |n: u16| BranchId(Uuid::from_u128(u128::from(n) << 64));
        // The default case count (256), or PROPTEST_CASES.
        proptest!(|(
            initial in proptest::collection::btree_set(1u16..400, 0..60),
            churn in proptest::collection::vec(
                proptest::collection::vec((any::<bool>(), 1u16..400), 0..4),
                0..200,
            ),
        )| {
            // Branches that stay candidates all along (ids above the churn's).
            let stable: BTreeSet<u16> = (0..25).map(|i| 400 + i * 7).collect();
            let mut live: BTreeSet<u16> = initial.union(&stable).copied().collect();
            let mut state = SweepState::default();
            let mut clock = Instant::now();
            // The current cycle: what it read, what was live all along, ids live at any point.
            let mut read: HashSet<u16> = HashSet::new();
            let mut throughout: BTreeSet<u16> = live.clone();
            let mut seen_in_cycle: BTreeSet<u16> = live.clone();
            let mut passes_in_cycle = 0usize;
            let mut removed_since: HashSet<u16> = HashSet::new();
            let mut removed_prev: HashSet<u16> = HashSet::new();
            let mut last_attempt: HashMap<u16, usize> = stable.iter().map(|s| (*s, 0)).collect();
            let passes = churn.len() + 200;
            for pass in 0..passes {
                for (add, n) in churn.get(pass).cloned().unwrap_or_default() {
                    if add {
                        live.insert(n);
                        seen_in_cycle.insert(n);
                    } else if live.remove(&n) {
                        throughout.remove(&n);
                        removed_since.insert(n);
                    }
                }
                // The SQL: ids after the cursor, in order, up to the limit.
                let from = state.cursor.after();
                let lo = from.map(|b| (b.as_uuid().as_u128() >> 64) as u16 + 1).unwrap_or(0);
                let page: Vec<(BranchId, bool)> =
                    live.range(lo..).take(LIMIT).map(|n| (id(*n), false)).collect();
                for (b, _) in &page {
                    let n = (b.as_uuid().as_u128() >> 64) as u16;
                    prop_assert!(read.insert(n), "{n} read twice in one cycle");
                }
                for b in sweep_pass(&mut state, from, &page, LIMIT, BATCH) {
                    clock += Duration::from_millis(1);
                    state.attempts.insert(b, clock);
                    last_attempt.insert((b.as_uuid().as_u128() >> 64) as u16, pass);
                }
                passes_in_cycle += 1;
                if state.cursor.after().is_none() {
                    for n in &throughout {
                        prop_assert!(read.contains(n), "{n} missed by a cycle");
                    }
                    prop_assert!(passes_in_cycle <= seen_in_cycle.len() / LIMIT + 1);
                    for b in state.attempts.keys() {
                        let n = (b.as_uuid().as_u128() >> 64) as u16;
                        prop_assert!(
                            live.contains(&n) || removed_since.contains(&n) || removed_prev.contains(&n),
                            "{n} kept after it left"
                        );
                    }
                    read.clear();
                    throughout = live.clone();
                    seen_in_cycle = live.clone();
                    passes_in_cycle = 0;
                    removed_prev = std::mem::take(&mut removed_since);
                }
                // A stable branch waits at most: its page comes once a cycle (at most
                // 425 / LIMIT + 1 passes) and takes BATCH of at most LIMIT, least recent first.
                let bound = (425 / LIMIT + 1) * LIMIT.div_ceil(BATCH) + 1;
                for s in &stable {
                    prop_assert!(pass - last_attempt[s] <= bound, "{s} starved at pass {pass}");
                }
            }
        });
    }

    /// Round 4 (B1, B2) and the third invariant: the sweep's script takes no lock a tenant
    /// write conflicts with while the machinery is in place (DDL only behind catalog checks),
    /// waits at most [`SWEEP_LOCK_TIMEOUT`] for any lock, never waits on the advisory key, and
    /// a lock timeout or a busy sync is a retry; the foreign check refuses ownership only, and
    /// the revoke covers default privileges and large objects.
    #[test]
    fn fn_triggers_sweep_is_gentle_on_tenants() {
        let sql = sync_sql(&[wanted("orders", &["insert"])], false, ROLES);
        let at = |needle: &str| sql.find(needle).unwrap_or_else(|| panic!("{needle}"));
        assert!(sql.contains("SET LOCAL lock_timeout = '2s';"), "a short lock_timeout");
        assert!(!sql.contains("lock_timeout = '20s'"));
        assert!(sql.contains("pg_catalog.pg_try_advisory_xact_lock("), "never waits on the key");
        assert!(!sql.contains("pg_advisory_xact_lock("));
        // Every install statement sits behind a catalog check.
        let create_schema = at("CREATE SCHEMA chelabase_functions AUTHORIZATION chelabase_functions_owner;");
        assert!(at("IF ns IS NULL THEN") < create_schema);
        let owner = at("ALTER TABLE chelabase_functions.outbox OWNER TO chelabase_functions_owner;");
        assert!(at("IF box IS NULL THEN") < owner && owner < at("END IF;\n  SELECT p.prosrc"));
        assert!(at("IF NOT FOUND OR fn.prosrc <> ") < at("CREATE OR REPLACE FUNCTION chelabase_functions.enqueue()"));
        assert!(!sql.contains("CREATE TABLE IF NOT EXISTS") && !sql.contains("CREATE SCHEMA IF NOT EXISTS"));
        // B2: only ownership outside the schema refuses; grantee-only entries are revoked.
        assert!(sql.contains("AND d.refobjid = role AND d.deptype = 'o'"));
        assert!(!sql.contains("NOT (d.deptype = 'a'"));
        assert!(sql.contains("owns objects outside chelabase_functions"));
        for needle in [
            "REVOKE ALL ON LARGE OBJECT %s FROM chelabase_functions_owner CASCADE",
            "ALTER DEFAULT PRIVILEGES FOR ROLE %I%s REVOKE ALL ON %s FROM chelabase_functions_owner CASCADE",
            "'pg_catalog.pg_largeobject'::pg_catalog.regclass",
            "'pg_catalog.pg_default_acl'::pg_catalog.regclass",
        ] {
            assert!(sql.contains(needle), "{needle}");
        }
    }

    /// The third invariant: a lock a tenant held, or another sync of the database, is a retry
    /// on the next pass, never a failure; anything else still is one.
    #[test]
    fn fn_triggers_lock_waits_are_retried() {
        assert!(retry_later(
            "psql in compute ep-x exited with 3: ERROR:  canceling statement due to lock timeout\nCONTEXT: SQL statement"
        ));
        assert!(retry_later(&format!(
            "ERROR:  {BUSY_MARK}: another sync of this database holds the lock"
        )));
        assert!(!retry_later("ERROR:  syntax error at or near \"x\""));
        assert!(!retry_later("ERROR:  canceling statement due to statement timeout"));
        assert!(!retry_later(&format!("ERROR:  {FOREIGN_MARK}: the schema is not ours")));
        assert!(PRELUDE.contains(&format!("SET LOCAL lock_timeout = '{SWEEP_LOCK_TIMEOUT}';")));
        for script in [role_sql(true, Some(VERIFIER)), check_sql(ROLES), drop_triggers_sql()] {
            assert!(script.contains("SET LOCAL lock_timeout = '2s';"), "{script}");
        }
    }

    /// The functions service's dispatcher keeps its own copies of the login and channel names
    /// (`functions::events::dispatch`); until it uses the shared ones they must be equal.
    #[test]
    fn fn_triggers_dispatch_names_match_the_functions_service() {
        assert_eq!(functions::events::dispatch::LOGIN, DISPATCH);
        assert_eq!(functions::events::dispatch::CHANNEL, NOTIFY_CHANNEL);
        // The exact grants the install makes and checks are the shared lists.
        let sql = revoke_sql();
        assert!(sql.contains("GRANT USAGE ON SCHEMA chelabase_functions TO chelabase_functions_dispatch;"), "{sql}");
        assert!(sql.contains("IS DISTINCT FROM ARRAY['USAGE:false:true']::pg_catalog.text[]"), "{sql}");
    }

    /// A database-level (or role-level) `statement_timeout` shorter than a sync's statements
    /// never fails every pass: each script sets its own, bounded, before anything else runs.
    #[test]
    fn fn_triggers_scripts_set_their_own_statement_timeout() {
        let set = "SET LOCAL statement_timeout = '2min';";
        let sync = sync_sql(&[wanted("orders", &["insert"])], false, ROLES);
        for script in [role_sql(true, Some(VERIFIER)), check_sql(ROLES), drop_triggers_sql(), sync] {
            let at = script.find(set).unwrap_or_else(|| panic!("no {set} in {script}"));
            assert!(at < script.find("DO $chelabase_fn$").unwrap_or(usize::MAX), "{script}");
        }
    }

    fn record(enabled: bool, state: TriggerState) -> TriggerRecord {
        TriggerRecord {
            id: Uuid::new_v4(),
            branch_id: BranchId::new(),
            function_id: Uuid::new_v4(),
            source: TriggerSource::Table,
            enabled,
            state,
            enabled_since: Timestamp::now(),
            pg_database: "app".into(),
            schema_name: Some("public".into()),
            table_name: Some("orders".into()),
            bucket: None,
            prefix: None,
            events: vec!["insert".into()],
            last_error: None,
            dead_count: 0,
            dropped_count: 0,
            last_delivery_error: None,
            last_delivered_at: None,
        }
    }

    /// m13 (#175, S4): a disabled trigger is never wanted, so its database trigger is dropped
    /// and the cleanup deletes its queued events: a disabled trigger queues nothing.
    #[test]
    fn fn_triggers_disabled_rows_are_not_wanted() {
        let on = record(true, TriggerState::Installed);
        let off = record(false, TriggerState::Installed);
        let pending_off = record(false, TriggerState::PendingInstall);
        let run_as = with_run_as(&[&on, &off, &pending_off]);
        let wanted = wanted_in(&[on.clone(), off, pending_off], "app", &run_as);
        assert_eq!(wanted.iter().map(|w| w.id).collect::<Vec<_>>(), vec![on.id]);
    }

    /// m13: `enqueue()` records the transaction id, for the event's `txid`.
    #[test]
    fn fn_triggers_enqueue_records_the_txid() {
        assert!(enqueue_body().contains("'txid', pg_catalog.txid_current()"));
    }

    /// Round 2 (B1, B2): plain tables only; the install adopts nothing it didn't make.
    #[test]
    fn fn_triggers_plain_tables_and_no_adoption() {
        let sql = sync_sql(&[wanted("orders", &["insert"])], false, ROLES);
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
        assert!(check_sql(ROLES).contains(FOREIGN_MARK));
        assert!(role_sql(true, Some(VERIFIER)).contains(
            "CREATE ROLE chelabase_functions_owner NOLOGIN NOINHERIT CONNECTION LIMIT 0"
        ));
    }

    /// m13b: the dispatch login is made once per compute (LOGIN NOINHERIT, its connection
    /// limit, the branch's verifier, `search_path` and `event_triggers` pinned) and its
    /// password follows the branch's only while it has no membership; the foreign check takes
    /// exactly that role (the OID read, those attributes and settings, no membership, nothing
    /// owned); the revoke keeps exactly its grants (USAGE on the schema; SELECT, UPDATE and
    /// DELETE on the outbox, from the owner) and takes back everything else granted to it.
    #[test]
    fn fn_triggers_dispatch_role_is_an_exact_allowlist() {
        let role = role_sql(true, Some(VERIFIER));
        for needle in [
            &format!("CREATE ROLE chelabase_functions_dispatch LOGIN NOINHERIT CONNECTION LIMIT {DISPATCH_CONNECTION_LIMIT} PASSWORD '{VERIFIER}';"),
            "ALTER ROLE chelabase_functions_dispatch SET search_path = pg_catalog, pg_temp;",
            "ALTER ROLE chelabase_functions_dispatch SET event_triggers = off;",
            &format!("ALTER ROLE chelabase_functions_dispatch PASSWORD '{VERIFIER}';"),
            "SELECT 'fn_dispatch ' || oid FROM pg_catalog.pg_roles WHERE rolname = 'chelabase_functions_dispatch';",
        ] {
            assert!(role.contains(needle), "{needle}\n{role}");
        }
        let alter = role.find("ALTER ROLE chelabase_functions_dispatch PASSWORD").unwrap();
        assert!(role[..alter].contains("NOT EXISTS (SELECT FROM pg_catalog.pg_auth_members m WHERE m.roleid = d.oid OR m.member = d.oid)"));
        let plain = role_sql(false, None);
        assert!(!plain.contains("CREATE ROLE") && !plain.contains("ALTER ROLE"), "{plain}");
        assert!(plain.contains("'fn_dispatch '"));
        assert_eq!(
            parse_role("x\nfn_owner 11\nfn_dispatch 12\n"),
            Roles { owner: Some(11), dispatch: Some(12) }
        );
        assert_eq!(parse_role("fn_owner 11\n"), Roles { owner: Some(11), dispatch: None });

        let sql = sync_sql(&[wanted("orders", &["insert"])], false, ROLES);
        for needle in [
            "dexpected pg_catalog.oid := 4343;",
            "the role chelabase_functions_dispatch was made by someone else",
            &format!("OR NOT a.rolcanlogin OR a.rolreplication OR a.rolbypassrls OR a.rolconnlimit <> {DISPATCH_CONNECTION_LIMIT}"),
            "the role chelabase_functions_dispatch has attributes Chelabase never gives it",
            "the role chelabase_functions_dispatch has members or memberships",
            "the role chelabase_functions_dispatch has settings Chelabase never gives it",
            "ARRAY['event_triggers=off', 'search_path=pg_catalog, pg_temp']::pg_catalog.text[]",
            "the role chelabase_functions_dispatch owns objects",
            "GRANT USAGE ON SCHEMA chelabase_functions TO chelabase_functions_dispatch;",
            "GRANT DELETE, SELECT, UPDATE ON TABLE chelabase_functions.outbox TO chelabase_functions_dispatch;",
            "IS DISTINCT FROM ARRAY['USAGE:false:true']::pg_catalog.text[]",
            "IS DISTINCT FROM ARRAY['DELETE:false:true', 'SELECT:false:true', 'UPDATE:false:true']::pg_catalog.text[]",
            "REVOKE ALL ON SCHEMA chelabase_functions FROM chelabase_functions_dispatch CASCADE;",
            "REVOKE ALL ON TABLE chelabase_functions.outbox FROM chelabase_functions_dispatch CASCADE;",
            "REVOKE ALL ON ROUTINE %s FROM chelabase_functions_dispatch CASCADE",
            "REVOKE ALL ON LARGE OBJECT %s FROM chelabase_functions_dispatch CASCADE",
            "ALTER DEFAULT PRIVILEGES FOR ROLE %I%s REVOKE ALL ON %s FROM chelabase_functions_dispatch CASCADE",
        ] {
            assert!(sql.contains(needle), "{needle}");
        }
        // Nothing beyond the allowlist: no INSERT, no sequence, no enqueue(), no membership.
        for never in [
            "INSERT ON TABLE chelabase_functions.outbox TO chelabase_functions_dispatch",
            "ON SEQUENCE chelabase_functions.outbox_id_seq TO chelabase_functions_dispatch",
            "EXECUTE ON FUNCTION chelabase_functions.enqueue() TO chelabase_functions_dispatch",
            "GRANT chelabase_functions_owner TO chelabase_functions_dispatch",
            "TO chelabase_functions_dispatch WITH GRANT OPTION",
        ] {
            assert!(!sql.contains(never), "{never}");
        }
        // The grants are made only where the catalog differs, after the install.
        let grant = sql.find("GRANT USAGE ON SCHEMA chelabase_functions TO chelabase_functions_dispatch;").unwrap();
        assert!(sql.find("CREATE SCHEMA").unwrap() < grant);
        assert!(sql.find("IS DISTINCT FROM ARRAY['USAGE:false:true']").unwrap() < grant);
        // The owner's grant revokes keep their text: the dispatch role's own loop is beside it.
        assert!(sql.contains("REVOKE ALL ON ROUTINE %s FROM chelabase_functions_owner CASCADE"));
    }

    /// m13b (B2): a trigger on a reserved schema is never wanted (its database trigger, if
    /// any, is dropped) and its row settles failed with [`RESERVED_SCHEMA`].
    #[test]
    fn fn_triggers_reserved_schemas_are_never_wanted() {
        let mut outbox = record(true, TriggerState::PendingInstall);
        outbox.schema_name = Some("chelabase_functions".into());
        outbox.table_name = Some("outbox".into());
        let mut storage = record(true, TriggerState::Installed);
        storage.schema_name = Some("storage".into());
        let fine = record(true, TriggerState::PendingInstall);
        let run_as = with_run_as(&[&outbox, &storage, &fine]);
        let wanted = wanted_in(&[outbox.clone(), storage.clone(), fine.clone()], "app", &run_as);
        assert_eq!(wanted.iter().map(|w| w.id).collect::<Vec<_>>(), vec![fine.id]);
        assert_eq!(refusal(&outbox), Some(RESERVED_SCHEMA));
        assert_eq!(refusal(&storage), Some(RESERVED_SCHEMA));
        assert_eq!(refusal(&fine), None);
    }

    /// m13b fix (S2): the cleanup counts this timeline's removed rows per trigger, from the
    /// DELETE itself, as its owner; the worker reads them back.
    #[test]
    fn fn_triggers_cleanup_counts_what_it_drops() {
        let sql = sync_sql(&[wanted("orders", &["insert"])], false, ROLES);
        let role = sql.find("SET LOCAL ROLE chelabase_functions_owner;").unwrap();
        let delete = sql.find("WITH d AS (DELETE FROM chelabase_functions.outbox").unwrap();
        assert!(role < delete && delete < sql.find("RESET ROLE;").unwrap());
        assert!(sql.contains("FROM d WHERE d.here GROUP BY 1"));
        assert!(sql.contains("SELECT 'fn_dropped ' || coalesce(pg_catalog.current_setting('chelabase_fn.dropped', true), '');"));
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(parse_dropped(&format!("x\nfn_dropped {a}:2 {b}:1\n")), vec![(a, 2), (b, 1)]);
        assert!(parse_dropped("fn_dropped \n").is_empty());
    }

    /// The `{EXACT} AND <rule>` checks of the script, by role: keep (drop loop), create,
    /// cleanup and answer, each with the one read rule.
    fn rule_sites(sql: &str) -> [&str; 4] {
        let site = |from: &str, to: &str| {
            let at = sql.find(from).unwrap_or_else(|| panic!("no {from}"));
            let len = sql[at..].find(to).unwrap_or_else(|| panic!("no {to} after {from}"));
            &sql[at..at + len]
        };
        [
            site("AND NOT EXISTS (SELECT FROM pg_temp.chelabase_fn_wanted w WHERE", "\n  LOOP"),
            site("WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_trigger tg WHERE tg.tgrelid = c.oid AND tg.tgname = w.name)", "\n  LOOP"),
            site("keep pg_catalog.uuid[] := ARRAY(", "\nBEGIN"),
            site("SELECT 'fn_trigger ' || w.trigger_id", "WHEN EXISTS (SELECT FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n      WHERE n.nspname = w.schema_name AND c.relname = w.table_name) THEN ' not_a_plain_table'"),
        ]
    }

    /// m17 (AC 3, 4): the keep and create predicates require the run-as OID to exist and
    /// hold table-level `SELECT` and `USAGE` on the table's schema (`has_table_privilege` and
    /// `has_schema_privilege` on `w.run_as`, guarded by `pg_roles`), the answer reports
    /// `no_select`, and the cleanup removes the events of a trigger whose role may not read
    /// its table.
    #[test]
    fn sync_requires_select_and_schema_usage_for_the_run_as_oid() {
        let sql = sync_sql(&[wanted("orders", &["insert"])], false, ROLES);
        let select = "(CASE WHEN EXISTS (SELECT FROM pg_catalog.pg_roles chelabase_run_as WHERE chelabase_run_as.oid = w.run_as) THEN pg_catalog.has_table_privilege(w.run_as, c.oid, 'SELECT') AND pg_catalog.has_schema_privilege(w.run_as, c.relnamespace, 'USAGE') ELSE false END)";
        assert!(sql.contains(select), "{sql}");
        for site in rule_sites(&sql) {
            assert!(site.contains(select), "{site}");
        }
        assert!(sql.contains("run_as oid) ON COMMIT DROP;"));
        assert!(sql.contains("THEN ' no_select'"));
        assert!(sql.contains("OR trigger_id <> ALL (keep)"));
        // A column grant is not table-level SELECT: no column privilege check stands in.
        assert!(!sql.contains("has_column_privilege") && !sql.contains("has_any_column_privilege"));
        // The trigger records the OID it was checked for, so a changed OID replaces it.
        assert!(sql.contains("tg.tgnargs = 2"));
        assert!(sql.contains("pg_catalog.convert_to(w.run_as::pg_catalog.text, 'UTF8')"));
        assert!(sql.contains("enqueue(%L, %L)'"));
        assert_eq!(install_error("no_select"), RUN_AS_CANNOT_READ_TABLE);
        assert_eq!(RUN_AS_CANNOT_READ_TABLE, "run_as_role_cannot_read_table");
    }

    /// m17 (AC 8-12, 14): the read rule refuses a role subject to the table's RLS (no
    /// superuser or BYPASSRLS of its own, and not the owner or the table forces RLS);
    /// `subject_to_rls` is answered after `no_select`; keep, create, cleanup, answer and
    /// `enqueue()` all use the one fragment.
    #[test]
    fn sync_refuses_run_as_subject_to_rls() {
        let rule = run_as_reads("w.run_as", "c");
        for needle in [
            "NOT c.relrowsecurity",
            "(chelabase_run_as.rolsuper OR chelabase_run_as.rolbypassrls)",
            "pg_catalog.pg_has_role(w.run_as, c.relowner, 'USAGE')",
            "NOT c.relforcerowsecurity AND",
        ] {
            assert!(rule.contains(needle), "{needle}: {rule}");
        }
        // BYPASSRLS is read on the role itself (Postgres doesn't inherit it).
        assert!(!rule.contains("pg_auth_members"));
        let sql = sync_sql(&[wanted("orders", &["insert"])], false, ROLES);
        for site in rule_sites(&sql) {
            assert!(site.contains(&rule), "{site}");
        }
        let no_select = sql.find("THEN ' no_select'").unwrap();
        let rls = sql.find("THEN ' subject_to_rls'").unwrap();
        let plain = sql.find("THEN ' not_a_plain_table'").unwrap();
        assert!(sql.find("THEN ' ok'").unwrap() < no_select && no_select < rls && rls < plain);
        assert_eq!(install_error("subject_to_rls"), RUN_AS_SUBJECT_TO_RLS);
        assert_eq!(RUN_AS_SUBJECT_TO_RLS, "run_as_role_subject_to_rls");
        // Capture time: enqueue() checks its recorded OID (its second argument) first.
        let body = enqueue_body();
        let capture = run_as_reads("ra", "c");
        let check = body.find(&format!("WHERE c.oid = TG_RELID AND {capture}")).expect("capture check");
        assert!(body.contains("ra := TG_ARGV[1]::pg_catalog.oid;"));
        assert!(check < body.find("INSERT INTO chelabase_functions.outbox").unwrap());
        assert!(body.contains("IF TG_NARGS <> 2 THEN\n    RETURN NULL;"));
    }

    /// m17 (AC 2): a trigger whose function has no run-as OID is never wanted, and settles
    /// `run_as_role_required`; one with an OID carries it; a disabled or failed row gets no
    /// such verdict (it isn't installable at all).
    #[test]
    fn wanted_needs_a_run_as_role() {
        let with = record(true, TriggerState::PendingInstall);
        let without = record(true, TriggerState::Installed);
        let disabled = record(false, TriggerState::Installed);
        let mut run_as = with_run_as(&[&with]);
        let wanted = wanted_in(&[with.clone(), without.clone(), disabled.clone()], "app", &run_as);
        assert_eq!(wanted.iter().map(|w| (w.id, w.run_as_oid)).collect::<Vec<_>>(), vec![(with.id, 16500)]);
        assert_eq!(run_as_refusal(&with, &run_as), None);
        assert_eq!(run_as_refusal(&without, &run_as), Some(RUN_AS_ROLE_REQUIRED));
        assert_eq!(run_as_refusal(&disabled, &run_as), None);
        assert_eq!(RUN_AS_ROLE_REQUIRED, "run_as_role_required");
        // The branch's functions: only a set run-as role with an OID in range counts.
        let function = |role: Option<&str>, oid: Option<i64>| chelabase_metadata::BranchFunction {
            function_id: Uuid::new_v4(),
            name: "f".into(),
            kind: chelabase_metadata::FunctionKind::Function,
            deployment_id: Uuid::new_v4(),
            enabled: true,
            auth: chelabase_metadata::FunctionAuth::Key,
            run_as_role: role.map(str::to_string),
            run_as_key_id: None,
            run_as_role_oid: oid,
            run_as_unmintable: None,
            last_error: None,
            updated_at: Timestamp::now(),
            health: None,
        };
        let fs = [
            function(Some("reader"), Some(16500)),
            function(None, Some(16501)),
            function(Some("reader"), None),
            function(Some("reader"), Some(-1)),
            function(Some("reader"), Some(i64::from(u32::MAX) + 1)),
        ];
        run_as = run_as_oids(&fs);
        assert_eq!(run_as, RunAsOids::from([(fs[0].function_id, 16500)]));
    }
}
