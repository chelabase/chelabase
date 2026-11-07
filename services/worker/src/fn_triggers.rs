//! Edge functions m8: database-event triggers in a branch's databases.
//!
//! [`sync_sql`] keeps one database in step with its branch's table triggers
//! (`function_triggers`, source `table`), as the superuser (`cloud_admin`), in one transaction:
//!
//! - **The machinery**, made or put back each run while the database has a trigger to install:
//!   the NOLOGIN role [`OWNER`] (no members, no memberships, no login, no attributes), the
//!   schema `chelabase_functions` it owns (no rights for PUBLIC, so no data API role reaches
//!   it), the outbox `chelabase_functions.outbox` and the trigger function
//!   `chelabase_functions.enqueue()`, both [`OWNER`]'s with every right revoked from PUBLIC.
//!   The function is `SECURITY DEFINER` with `search_path = pg_catalog, chelabase_functions,
//!   pg_temp` and qualifies every name, so a user's `public.pg_notify` or
//!   `public.jsonb_build_object` never runs in it. It inserts the row's event (the trigger id
//!   from its argument, the operation, schema, table, new and old rows, and the Neon timeline)
//!   and notifies [`NOTIFY_CHANNEL`].
//! - **The outbox**: truncated when asked (a branch create, reset or restore: the database
//!   came from another timeline), and on every run its rows from another timeline are deleted
//!   (a create, reset or restore whose own run failed is caught up by the sweep).
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
//! child's run-as keys ([`Worker::mint_child_run_as`]).

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

/// The machinery's DDL (see the module docs), after the role and schema checks.
fn install_sql() -> String {
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
CREATE SCHEMA IF NOT EXISTS {FUNCTIONS_SCHEMA} AUTHORIZATION {OWNER};
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
BEGIN
  INSERT INTO {FUNCTIONS_SCHEMA}.outbox (trigger_id, payload, timeline_id)
  VALUES (TG_ARGV[0]::pg_catalog.uuid,
    pg_catalog.jsonb_build_object('type', TG_OP, 'schema', TG_TABLE_SCHEMA, 'table', TG_TABLE_NAME,
      'record', CASE WHEN TG_OP OPERATOR(pg_catalog.<>) 'DELETE' THEN pg_catalog.to_jsonb(NEW) END,
      'old_record', CASE WHEN TG_OP OPERATOR(pg_catalog.<>) 'INSERT' THEN pg_catalog.to_jsonb(OLD) END),
    pg_catalog.current_setting('neon.timeline_id', true));
  PERFORM pg_catalog.pg_notify('{NOTIFY_CHANNEL}', '');
  RETURN NULL;
END $chelabase_fn$;
ALTER FUNCTION {FUNCTIONS_SCHEMA}.enqueue() OWNER TO {OWNER};
REVOKE ALL ON FUNCTION {FUNCTIONS_SCHEMA}.enqueue() FROM PUBLIC;
"#
    )
}

/// The script that brings one database in step with `wanted` (see the module docs);
/// `truncate` empties the outbox first. Without wanted triggers it installs nothing, and only
/// drops leftovers and cleans the outbox of a schema [`OWNER`] owns.
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
    let install = if rows.is_empty() {
        String::new()
    } else {
        install_sql()
    };
    let truncate = if truncate {
        format!("    TRUNCATE {FUNCTIONS_SCHEMA}.outbox;\n")
    } else {
        String::new()
    };
    format!(
        r#"{SYNC_MARK}
BEGIN;
SET LOCAL search_path = pg_catalog, pg_temp;
SET LOCAL event_triggers = off;
SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
SELECT pg_catalog.pg_advisory_xact_lock({SYNC_LOCK_KEY}) IS NULL;
CREATE TEMP TABLE chelabase_fn_wanted (name text, trigger_id uuid, schema_name text, table_name text, events text, tgtype int2) ON COMMIT DROP;
{insert}{install}DO $chelabase_fn$ DECLARE
  ours pg_catalog.oid := (SELECT n.oid FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner
    WHERE n.nspname = '{FUNCTIONS_SCHEMA}' AND r.rolname = '{OWNER}');
  t record;
BEGIN
  IF ours IS NULL THEN
    RETURN;
  END IF;
  IF pg_catalog.to_regclass('{FUNCTIONS_SCHEMA}.outbox') IS NOT NULL THEN
{truncate}    DELETE FROM {FUNCTIONS_SCHEMA}.outbox WHERE timeline_id IS DISTINCT FROM pg_catalog.current_setting('neon.timeline_id', true);
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
    /// run leaves the function without a key (logged).
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
        for f in functions.iter().filter(|f| f.run_as_key_id.is_none()) {
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
                            None
                        }
                        AllRowsCheckReply::Exposed { .. } => Some(format!(
                            "run_as_role_refused: the role {role} reaches a role with the \"all rows\" policy"
                        )),
                        AllRowsCheckReply::Failed { message } => {
                            warn!(branch_id = %branch, function = %f.name, "the all-rows check failed: {message}");
                            None
                        }
                    }
                }
                RoleCheck::Refused(why) if why.ends_with("does not exist") => {
                    Some(RUN_AS_ROLE_MISSING.to_string())
                }
                RoleCheck::Refused(why) => Some(format!("run_as_role_refused: {why}")),
                RoleCheck::NotReady | RoleCheck::Failed(_) => {
                    warn!(branch_id = %branch, function = %f.name, "the run-as role couldn't be checked");
                    None
                }
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
            "DELETE FROM chelabase_functions.outbox WHERE timeline_id IS DISTINCT FROM pg_catalog.current_setting('neon.timeline_id', true);",
        ] {
            assert!(sql.contains(needle), "{needle}");
        }
        assert!(!sync_sql(&[wanted("orders", &["insert"])], false).contains("TRUNCATE"));
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
