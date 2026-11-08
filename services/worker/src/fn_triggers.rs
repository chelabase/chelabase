//! Edge functions m8: database-event triggers in a branch's databases.
//!
//! [`sync_sql`] keeps one database in step with its branch's table triggers
//! (`function_triggers`, source `table`), as the superuser (`cloud_admin`), in one transaction:
//!
//! - **The machinery**, made or put back each run while the database has a trigger to install:
//!   the NOLOGIN role [`OWNER`] (no members, no memberships, no login, no attributes), the
//!   schema `chelabase_functions` it owns (no rights for PUBLIC, so no data API role reaches
//!   it), the outbox `chelabase_functions.outbox` and the trigger function
//!   `chelabase_functions.enqueue()`, both [`OWNER`]'s. Every grant on the schema, the outbox,
//!   its sequence and the function to anyone but the owner is revoked on every run.
//!   The function is `SECURITY DEFINER` with `search_path = pg_catalog, chelabase_functions,
//!   pg_temp` and qualifies every name, so a user's `public.pg_notify` or
//!   `public.jsonb_build_object` never runs in it, and it evaluates no user code at all (audit
//!   B1): a built-in-typed column goes through `to_jsonb`, any other value is its type output
//!   text, so no user cast ever runs as [`OWNER`] ([`install_objects_sql`]). It inserts the
//!   row's event (the trigger id from its argument, the operation, schema, table, new and old
//!   rows, and the Neon timeline) and notifies [`NOTIFY_CHANNEL`].
//! - **The repair**, before anything runs on the schema: an outbox of another shape (columns,
//!   defaults, triggers, rules, policies, row level security, extra constraints or indexes,
//!   owner) is dropped and rebuilt, and any other relation or routine in the schema is
//!   dropped ([`repair_sql`]).
//! - **The outbox**: truncated when asked (a branch create, reset or restore: the database
//!   came from another timeline), and on every run its rows from another timeline or for a
//!   trigger the database doesn't want are deleted (a create, reset or restore whose own run
//!   failed is caught up by the sweep). The cleanup runs as [`OWNER`] (`SET LOCAL ROLE`),
//!   after the repair, never as the superuser.
//! - **The triggers**: `chelabase_fn_<id as 32 hex>` ([`trigger_name`]), `AFTER <events> ON
//!   <schema>.<table> FOR EACH ROW EXECUTE FUNCTION chelabase_functions.enqueue('<id>')`.
//!   Every trigger running a function of `chelabase_functions`, and any trigger with a wanted
//!   name, that isn't exactly a wanted one (name, table, function, events, argument, enabled,
//!   no column list or `WHEN`) is dropped; then each wanted one whose table exists (a plain or
//!   partitioned table) is created. Names reach the SQL only as literals, and the statements
//!   are built by `format('%I')`, so any table name (quotes, spaces) is quoted.
//! - Event triggers are off for the transaction (`SET LOCAL event_triggers = off`), so no
//!   tenant code runs as the superuser. A `chelabase_functions` schema [`OWNER`] doesn't own
//!   (a user's, from before the name was reserved) stops the install with [`FOREIGN_MARK`],
//!   changing nothing; a database without wanted triggers then is left alone.
//!
//! The script answers one line per wanted trigger: `fn_trigger <id> ok` or `fn_trigger <id>
//! table_missing`. The worker then settles the rows it read `pending_install`
//! (`MetadataClient::settle_trigger_installs`, by the `enabled_since` it read): only `installed`
//! rows are the dispatcher's (m13).
//!
//! When: after a branch create (the child's rows are copies, `pending_install`, and its
//! database still has the parent's triggers and outbox), after a reset or restore (the switch
//! pended every table trigger of the branch), and on reconcile for any branch with a pending
//! row (a trigger just added, or a lifecycle run that failed). A branch create also mints the
//! child's run-as keys ([`Worker::mint_child_run_as`]); one that can't be minted yet is
//! retried by the reconcile sweep ([`Worker::sync_pending_run_as`]).

use chelabase_metadata::{
    BranchRecord, BranchStatus, DatabaseOrigin, MetadataClient, RunAs, TriggerInstall,
    TriggerRecord, TriggerSource, TriggerState,
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

/// `last_error` prefix on a child's function whose run-as key couldn't be minted yet (the
/// role or "all rows" check couldn't run); the reconcile sweep retries it.
pub const RUN_AS_KEY_PENDING: &str = "run_as_key_pending";

/// Branches the reconcile sweep syncs per pass.
const SWEEP_BATCH: i64 = 20;

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
        .filter(|t| t.source == TriggerSource::Table && t.pg_database == db)
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
      AND tg.tgconstraint = 0 AND NOT tg.tgisinternal AND tg.tgparentid = 0";

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

/// The role's lockdown and the foreign-schema check (install only).
fn install_role_sql() -> String {
    format!(
        r#"DO $chelabase_fn$ DECLARE
  role pg_catalog.oid;
  m record;
BEGIN
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = '{OWNER}') THEN
    CREATE ROLE {OWNER} NOLOGIN;
  END IF;
  role := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = '{OWNER}');
  IF EXISTS (SELECT FROM pg_catalog.pg_namespace WHERE nspname = '{FUNCTIONS_SCHEMA}' AND nspowner <> role) THEN
    RAISE EXCEPTION '{FOREIGN_MARK}: the schema {FUNCTIONS_SCHEMA} is not Chelabase''s; it was left alone';
  END IF;
  FOR m IN SELECT r.rolname AS member, g.rolname AS grantor FROM pg_catalog.pg_auth_members a
    JOIN pg_catalog.pg_roles r ON r.oid = a.member JOIN pg_catalog.pg_roles g ON g.oid = a.grantor
    WHERE a.roleid = role
  LOOP
    EXECUTE pg_catalog.format('REVOKE {OWNER} FROM %I GRANTED BY %I CASCADE', m.member, m.grantor);
  END LOOP;
  FOR m IN SELECT r.rolname AS granted, g.rolname AS grantor FROM pg_catalog.pg_auth_members a
    JOIN pg_catalog.pg_roles r ON r.oid = a.roleid JOIN pg_catalog.pg_roles g ON g.oid = a.grantor
    WHERE a.member = role
  LOOP
    EXECUTE pg_catalog.format('REVOKE %I FROM {OWNER} GRANTED BY %I CASCADE', m.granted, m.grantor);
  END LOOP;
END $chelabase_fn$;
ALTER ROLE {OWNER} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS NOINHERIT CONNECTION LIMIT 0 PASSWORD NULL;
"#
    )
}

/// `DROP <kind>` for a `pg_class.relkind` (the SQL `CASE`).
const DROP_KIND: &str = "CASE t.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' \
WHEN 'S' THEN 'SEQUENCE' WHEN 'f' THEN 'FOREIGN TABLE' WHEN 'i' THEN 'INDEX' WHEN 'I' THEN 'INDEX' \
WHEN 'c' THEN 'TYPE' ELSE 'TABLE' END";

/// Before anything else runs on the schema: an outbox of any other shape (kind, owner,
/// columns, defaults, row level security, triggers, rules, policies, inheritance, constraints
/// or indexes beyond its primary key) is dropped (a DROP runs none of them), and so is every
/// relation and routine in the schema the install doesn't make. As the superuser: it only
/// reads catalogs and drops.
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
  FOR t IN SELECT c.relname, c.relkind FROM pg_catalog.pg_class c WHERE c.relnamespace = ours
      AND NOT ((c.relname = 'outbox' AND c.relkind = 'r') OR (c.relname = 'outbox_id_seq' AND c.relkind = 'S')
        OR (c.relname = 'outbox_pkey' AND c.relkind = 'i'))
  LOOP
    EXECUTE pg_catalog.format('DROP %s IF EXISTS {FUNCTIONS_SCHEMA}.%I CASCADE', {DROP_KIND}, t.relname);
  END LOOP;
  FOR t IN SELECT p.oid::pg_catalog.regprocedure AS f FROM pg_catalog.pg_proc p
      WHERE p.pronamespace = ours AND NOT (p.proname = 'enqueue' AND p.pronargs = 0)
  LOOP
    EXECUTE pg_catalog.format('DROP ROUTINE IF EXISTS %s CASCADE', t.f);
  END LOOP;
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
END $chelabase_fn$;
"#
    )
}

/// The script that brings one database in step with `wanted` (see the module docs);
/// `truncate` empties the outbox first. Without wanted triggers it installs nothing, and only
/// repairs, drops leftovers and cleans the outbox of a schema [`OWNER`] owns.
pub fn sync_sql(wanted: &[Wanted], truncate: bool) -> String {
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
    let (install_role, install_objects) = if rows.is_empty() {
        (String::new(), String::new())
    } else {
        (install_role_sql(), install_objects_sql())
    };
    let (repair, revoke, ours) = (repair_sql(), revoke_sql(), ours_sql());
    let truncate = if truncate {
        format!("  TRUNCATE {FUNCTIONS_SCHEMA}.outbox;\n")
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
BEGIN;
SET LOCAL search_path = pg_catalog, pg_temp;
SET LOCAL event_triggers = off;
SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
SELECT pg_catalog.pg_advisory_xact_lock({SYNC_LOCK_KEY}) IS NULL;
CREATE TEMP TABLE chelabase_fn_wanted (name text, trigger_id uuid, schema_name text, table_name text, events text, tgtype int2) ON COMMIT DROP;
{insert}{install_role}{repair}{install_objects}{revoke}DO $chelabase_fn$ DECLARE
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
    JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = w.table_name AND c.relkind IN ('r', 'p')
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
    WHERE {EXACT}) THEN ' ok' ELSE ' table_missing' END
  FROM pg_temp.chelabase_fn_wanted w;
COMMIT;
"#
    )
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

    /// Brings every database of the ready `branch` in step with its table triggers
    /// ([`sync_sql`]; `truncate` after a create, reset or restore) and settles the rows read
    /// `pending_install`. A database that can't be reached keeps its rows pending (the
    /// reconcile sweep retries) and makes the call fail; one that doesn't exist fails its rows.
    pub async fn sync_fn_triggers(&self, branch: BranchId, truncate: bool) -> Result<(), String> {
        let Some(meta) = self.metadata.functions() else {
            return Ok(());
        };
        let Some((_, ep)) = self.ready_endpoint(branch).await? else {
            return Ok(());
        };
        let triggers = meta
            .list_triggers(branch)
            .await
            .map_err(|e| format!("listing the triggers: {e}"))?;
        let mut databases: Vec<String> = self
            .metadata
            .branch_databases(branch)
            .await
            .map_err(|e| format!("listing the databases: {e}"))?
            .into_iter()
            .filter(|d| d.origin != DatabaseOrigin::System)
            .map(|d| d.name)
            .collect();
        for t in triggers.iter().filter(|t| t.source == TriggerSource::Table) {
            if !databases.contains(&t.pg_database) {
                databases.push(t.pg_database.clone());
            }
        }
        let mut answers = Vec::new();
        let mut failure = None;
        for db in &databases {
            let wanted = wanted_in(&triggers, db);
            match self
                .computes
                .admin_sql(&ep, db, &sync_sql(&wanted, truncate))
                .await
            {
                Ok(out) => answers.extend(parse_sync(&out)),
                Err(e) if database_missing(&format!("{e:#}"), db) => {
                    answers.extend(
                        wanted
                            .iter()
                            .map(|w| (w.id, Some("database_missing".to_string()))),
                    );
                }
                Err(e) => {
                    warn!(branch_id = %branch, database = %db, "syncing the function triggers: {e:#}");
                    failure = Some(format!("syncing the function triggers of {db}: {e:#}"));
                }
            }
        }
        let installs: Vec<TriggerInstall> = triggers
            .iter()
            .filter(|t| t.source == TriggerSource::Table && t.state == TriggerState::PendingInstall)
            .filter_map(|t| {
                let error = match answers.iter().find(|(id, _)| *id == t.id) {
                    Some((_, error)) => error.clone(),
                    // Bad events never reach the script (the schema's CHECK refuses them).
                    None if trigger_events(&t.events).is_none() => Some("invalid_events".into()),
                    None => return None,
                };
                Some(TriggerInstall {
                    id: t.id,
                    enabled_since: t.enabled_since,
                    error,
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
        failure.map_or(Ok(()), Err)
    }

    /// The reconcile sweep: syncs the branches with a pending table trigger (best effort).
    pub async fn sync_pending_fn_triggers(&self) {
        let Some(meta) = self.metadata.functions() else {
            return;
        };
        let branches = match meta.pending_trigger_branches(SWEEP_BATCH).await {
            Ok(branches) => branches,
            Err(e) => return warn!("listing branches with pending function triggers: {e}"),
        };
        for branch in branches {
            if let Err(e) = self.sync_fn_triggers(branch, false).await {
                warn!(branch_id = %branch, "function triggers stay pending: {e}");
            }
        }
    }

    /// The reconcile sweep for child run-as keys still to mint: every ready branch with a
    /// function that has a `run_as_role`, no key and no error but [`RUN_AS_KEY_PENDING`]'s
    /// (best effort).
    pub async fn sync_pending_run_as(&self) {
        let Some(meta) = self.metadata.functions() else {
            return;
        };
        let branches = match meta.pending_run_as_branches(SWEEP_BATCH).await {
            Ok(branches) => branches,
            Err(e) => return warn!("listing branches with run-as keys to mint: {e}"),
        };
        for branch in branches {
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
    /// retries; minting clears it. A function with any other error is left alone.
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
        let pending = |e: &Option<String>| {
            e.as_deref()
                .is_none_or(|e| e.starts_with(&format!("{RUN_AS_KEY_PENDING}: ")))
        };
        for f in functions
            .iter()
            .filter(|f| f.run_as_key_id.is_none() && pending(&f.last_error))
        {
            let Some(role) = f.run_as_role.as_deref() else {
                continue;
            };
            let refusal = match self.check_and_grant(branch, &ep, &db, role, None).await {
                RoleCheck::Ok(oid, bypass) => {
                    let request = AllRowsCheckRequest {
                        branch_id: branch,
                        starting: vec![role.to_string()],
                        change: None,
                    };
                    match self.all_rows_check(request).await {
                        AllRowsCheckReply::Clear => {
                            let generated = data_key::generate(DataKeyKind::Secret);
                            let run_as = RunAs {
                                role: role.to_string(),
                                role_oid: oid,
                                role_timeline: record.timeline_id.clone(),
                                role_bypass_rls: bypass,
                                key_id: Uuid::new_v4(),
                                prefix: generated.prefix,
                                sha256: generated.sha256,
                                secret: generated.key,
                            };
                            meta.set_run_as(branch, f.function_id, Some(&run_as))
                                .await
                                .map_err(|e| format!("function {}: {e}", f.name))?;
                            info!(branch_id = %branch, function = %f.name, "minted the child's run-as key");
                            if f.last_error.is_some() {
                                meta.set_function_last_error(branch, f.function_id, None)
                                    .await
                                    .map_err(|e| format!("function {}: {e}", f.name))?;
                            }
                            None
                        }
                        AllRowsCheckReply::Exposed { .. } => Some(format!(
                            "run_as_role_refused: the role {role} reaches a role with the \"all rows\" policy"
                        )),
                        AllRowsCheckReply::Failed { message } => {
                            warn!(branch_id = %branch, function = %f.name, "the all-rows check failed: {message}");
                            Some(format!(
                                "{RUN_AS_KEY_PENDING}: the \"all rows\" check failed: {message}"
                            ))
                        }
                    }
                }
                RoleCheck::Refused(why) if why.ends_with("does not exist") => {
                    Some(RUN_AS_ROLE_MISSING.to_string())
                }
                RoleCheck::Refused(why) => Some(format!("run_as_role_refused: {why}")),
                RoleCheck::NotReady => Some(format!(
                    "{RUN_AS_KEY_PENDING}: the branch's compute couldn't check the role"
                )),
                RoleCheck::Failed(why) => Some(format!("{RUN_AS_KEY_PENDING}: {why}")),
            };
            if let Some(error) = refusal {
                meta.set_function_last_error(branch, f.function_id, Some(&error))
                    .await
                    .map_err(|e| format!("function {}: {e}", f.name))?;
            }
        }
        Ok(())
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
        let sql = sync_sql(&[wanted(nasty, &["insert"])], false);
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
        assert!(!sync_sql(&[wanted("t", &["truncate"])], false).contains("INSERT INTO pg_temp"));
    }

    /// The function's privileges and search path, the event triggers off, the outbox cleaned.
    #[test]
    fn fn_triggers_script_is_locked_down() {
        let sql = sync_sql(&[wanted("orders", &["insert"])], true);
        assert!(sql.starts_with(&format!("{SYNC_MARK}\nBEGIN;\nSET LOCAL search_path = pg_catalog, pg_temp;\nSET LOCAL event_triggers = off;\n")));
        assert!(sql.trim_end().ends_with("COMMIT;"));
        for needle in [
            "LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, chelabase_functions, pg_temp AS",
            "ALTER FUNCTION chelabase_functions.enqueue() OWNER TO chelabase_functions_owner;",
            "REVOKE ALL ON FUNCTION chelabase_functions.enqueue() FROM PUBLIC;",
            "REVOKE ALL ON SCHEMA chelabase_functions FROM PUBLIC;",
            "REVOKE ALL ON TABLE chelabase_functions.outbox FROM PUBLIC;",
            "REVOKE ALL ON SEQUENCE chelabase_functions.outbox_id_seq FROM PUBLIC;",
            "ALTER ROLE chelabase_functions_owner NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS NOINHERIT",
            "PERFORM pg_catalog.pg_notify('chelabase_functions', '');",
            "pg_catalog.jsonb_build_object(",
            "TRUNCATE chelabase_functions.outbox;",
            "OR trigger_id <> ALL (ARRAY['01234567-89ab-cdef-0123-456789abcdef']::pg_catalog.uuid[]);",
            "REVOKE ALL ON %s FROM %s CASCADE",
            "OR EXISTS (SELECT FROM pg_catalog.pg_trigger WHERE tgrelid = box)",
        ] {
            assert!(sql.contains(needle), "{needle}");
        }
        assert!(!sync_sql(&[wanted("orders", &["insert"])], false).contains("TRUNCATE"));
        // Audit B1: the row is never handed to to_jsonb whole (a user's cast would run as the
        // owner); the cleanup runs as the owner, after the repair, never as the superuser.
        assert!(!sql.contains("to_jsonb(NEW)") && !sql.contains("to_jsonb(OLD)"));
        let role = sql
            .find("SET LOCAL ROLE chelabase_functions_owner;")
            .unwrap();
        assert!(sql.find("DROP ROUTINE IF EXISTS").unwrap() < role);
        assert!(role < sql.find("TRUNCATE chelabase_functions.outbox;").unwrap());
        assert!(role < sql.find("DELETE FROM chelabase_functions.outbox").unwrap());
        // Nothing wanted: no install, only the cleanup of a schema that is ours.
        let cleanup = sync_sql(&[], true);
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
}
