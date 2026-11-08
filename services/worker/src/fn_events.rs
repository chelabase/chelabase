//! Edge functions m13 (#175): database events delivered to functions.
//!
//! m8 ([`crate::fn_triggers`]) records each committed row change of a triggered table in the
//! branch database's `chelabase_functions.outbox`. The dispatcher delivers them: one task per
//! (ready branch, Postgres database) with an `installed`, enabled table trigger
//! (`MetadataClient::dispatch_targets`, polled every [`SUPERVISE_EVERY`]). Each pass of a task:
//!
//! 1. reads the branch, its triggers and its functions from the metadata and plans
//!    ([`plan`]): a trigger is *delivered* when it is `installed`, enabled, of this database
//!    and its function is enabled on the branch; *held* (its events stay queued) when it is
//!    enabled but still `pending_install` or its function is disabled or not deployed. Every
//!    other row is deleted. The task ends when the branch isn't ready or the database has no
//!    installed, enabled trigger left (so it stops within one pass of a branch delete, a
//!    disable or a trigger delete);
//! 2. claims ([`claim_sql`], one psql call): deletes rows of another timeline and rows of any
//!    trigger neither delivered nor held, bounds the outbox ([`OUTBOX_CAP`] rows, then
//!    [`RETENTION`]), then waits up to [`WAIT`] for a due row and claims up to [`CLAIM_BATCH`]
//!    with `FOR UPDATE SKIP LOCKED`, counting the attempt and leasing them for [`LEASE`];
//! 3. vets each row ([`vet`]): outbox rows are untrusted (an owner-level role, a member of
//!    `pg_write_all_data`, can write them), so the row's trigger must be one delivered here,
//!    its timeline the branch's, its schema, table and operation the trigger's, and its records
//!    shaped for the operation. A row that fails is deleted, never delivered;
//! 4. posts each event to the functions service (`POST /internal/invoke/{branch}/{name}` with
//!    `chelabase-event: db`, the worker's bearer), body `{id, type, schema, table, record,
//!    old_record, txid}` ([`event_body`]; over [`MAX_EVENT_BYTES`] the records become null with
//!    `truncated: true` and `primary_key`);
//! 5. settles ([`settle`], [`settle_sql`]): a 2xx deletes the row; anything else retries after
//!    [`backoff`], and the [`MAX_ATTEMPTS`]th failure leaves it `dead` (kept, counted in the
//!    trigger's `dead_count`, until the bound deletes it), and records what it saw in the
//!    metadata (`record_trigger_deliveries`).
//!
//! Delivery is at least once: a worker that stops between a delivery and its settle delivers
//! the event again once the lease runs out. `id` (`<timeline>:<database>:<row id>`) identifies
//! an event for dedupe. A disabled trigger is uninstalled by m8's sync (nothing queues); a
//! disabled function's events queue (up to [`OUTBOX_CAP`] per database or [`RETENTION`], the
//! oldest dropped first) and are delivered once it is enabled again. Polling keeps the compute
//! awake while a database has an installed, enabled trigger.
//!
//! **The database role.** Every statement on the outbox runs as `chelabase_functions_owner`
//! (`SET LOCAL ROLE` in the worker's psql session, as m8's cleanup does): a NOLOGIN role with
//! no attribute, membership or setting, owning nothing outside the schema (m8's foreign check),
//! so it reads no table a member hasn't granted to `PUBLIC` or to it. No new role, password or
//! grant exists, so m8's revokes keep nothing extra. Before the role is set, a guard ([`GUARD`])
//! checks, as the superuser and reading catalogs only, that the schema and the outbox are that
//! role's, a plain table without triggers, rules, policies, row security or inheritance, and
//! that the compute is on the branch's timeline; otherwise nothing runs (m8's sync repairs or
//! refuses it). The scripts name every object qualified, under `search_path = pg_catalog,
//! pg_temp`, with event triggers off: no object a member can create runs in them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chelabase_metadata::{
    BranchFunction, BranchStatus, FunctionKind, TriggerDelivery, TriggerRecord, TriggerSource,
    TriggerState,
};
use chelabase_neon::{ComputeBackend, SqlOpts};
use chelabase_schema::exclusions::FUNCTIONS_SCHEMA;
use chelabase_schema::sql::literal;
use chelabase_types::BranchId;
use serde_json::{Map, Value, json};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::Worker;
use crate::fn_schedules::Invoker;
use crate::fn_triggers::OWNER;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

/// A failed event is retried until this many attempts, then it is `dead`.
pub const MAX_ATTEMPTS: i32 = 10;
/// Rows claimed per pass.
pub const CLAIM_BATCH: i64 = 10;
/// A claimed row is due again after this long when its settle never comes (the worker
/// stopped): longer than a batch of invocations can take.
pub const LEASE: Duration = Duration::from_secs(900);
/// How long a claim waits, inside the database, for a due row.
pub const WAIT: Duration = Duration::from_secs(5);
/// The most rows an outbox keeps; the oldest beyond are deleted.
pub const OUTBOX_CAP: i64 = 10_000;
/// Rows older than this are deleted (queued for a disabled function, or dead).
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// An event body over this is sent truncated.
pub const MAX_EVENT_BYTES: usize = 256 * 1024;
/// How often the supervisor looks for databases to dispatch.
pub const SUPERVISE_EVERY: Duration = Duration::from_secs(5);
/// The most databases dispatched at once by one worker.
const MAX_TARGETS: i64 = 500;
/// A pass re-reads the branch before an invocation once this long has passed.
const RECHECK_EVERY: Duration = Duration::from_secs(10);

/// The scripts' first line (the fakes' needle).
pub const EVENTS_MARK: &str = "-- chelabase: function events";
/// The error the guard raises when the outbox isn't ready to read.
pub const NOT_READY_MARK: &str = "chelabase_fn_events_not_ready";

/// The delay before attempt `attempts + 1`, after `attempts` failed ones: 1 s, doubling, at
/// most 5 minutes.
pub fn backoff(attempts: i32) -> Duration {
    let exp = attempts.clamp(1, 20) as u32 - 1;
    Duration::from_secs((1u64 << exp).min(300))
}

/// What became of one delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The function answered 2xx.
    Delivered,
    /// Anything else: another status, a timeout, no answer.
    Failed(String),
}

/// What happens to a claimed row after its delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settle {
    Delete,
    Retry(Duration),
    Dead,
}

/// The row's fate after its `attempts`th attempt (the claim counted it) ended in `outcome`.
pub fn settle(attempts: i32, outcome: &Outcome) -> Settle {
    match outcome {
        Outcome::Delivered => Settle::Delete,
        Outcome::Failed(_) if attempts >= MAX_ATTEMPTS => Settle::Dead,
        Outcome::Failed(_) => Settle::Retry(backoff(attempts)),
    }
}

/// A trigger whose events this database delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deliverable {
    pub trigger_id: Uuid,
    pub function: String,
    pub schema: String,
    pub table: String,
    pub events: Vec<String>,
}

/// One pass's plan for a database ([`plan`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Whether the task keeps running: some trigger here is `installed` and enabled.
    pub run: bool,
    pub deliver: Vec<Deliverable>,
    /// Triggers whose events stay queued, undelivered.
    pub hold: Vec<Uuid>,
}

/// Plans a pass over `db` (see the module docs) from the branch's triggers and functions.
pub fn plan(db: &str, triggers: &[TriggerRecord], functions: &[BranchFunction]) -> Plan {
    let mut out = Plan::default();
    for t in triggers
        .iter()
        .filter(|t| t.source == TriggerSource::Table && t.enabled && t.pg_database == db)
    {
        let function = functions
            .iter()
            .find(|f| f.function_id == t.function_id && f.kind == FunctionKind::Function);
        match (t.state, function, &t.schema_name, &t.table_name) {
            (TriggerState::Installed, Some(f), Some(schema), Some(table)) if f.enabled => {
                out.run = true;
                out.deliver.push(Deliverable {
                    trigger_id: t.id,
                    function: f.name.clone(),
                    schema: schema.clone(),
                    table: table.clone(),
                    events: t.events.clone(),
                });
            }
            (TriggerState::Installed, ..) => {
                out.run = true;
                out.hold.push(t.id);
            }
            (TriggerState::PendingInstall, ..) => out.hold.push(t.id),
            (TriggerState::Failed, ..) => {}
        }
    }
    out
}

/// A claimed outbox row as the claim prints it (untrusted: see [`vet`]).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Claimed {
    pub id: i64,
    pub trigger_id: Uuid,
    pub attempts: i32,
    pub timeline_id: Option<String>,
    pub payload: Value,
    /// The payload was over [`MAX_EVENT_BYTES`]: the claim left its records out.
    #[serde(default)]
    pub big: bool,
    /// The primary key's values (from the record, else the old record), by column.
    #[serde(default)]
    pub pk: Value,
}

/// A vetted event.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: String,
    pub kind: &'static str,
    pub schema: String,
    pub table: String,
    pub record: Value,
    pub old_record: Value,
    pub txid: Value,
    pub big: bool,
    pub pk: Value,
}

/// Checks a claimed row against the trigger it names (`deliver`, found by its id among those
/// delivered here) and the branch's timeline; the event, or why the row is dropped.
pub fn vet(
    row: &Claimed,
    deliver: Option<&Deliverable>,
    timeline: &str,
    db: &str,
) -> Result<Event, &'static str> {
    let d = deliver.ok_or("not a trigger delivered here")?;
    if d.trigger_id != row.trigger_id {
        return Err("not a trigger delivered here");
    }
    if row.timeline_id.as_deref() != Some(timeline) {
        return Err("another timeline");
    }
    let p = row.payload.as_object().ok_or("not an object")?;
    let text = |k: &str| p.get(k).and_then(Value::as_str);
    if text("schema") != Some(d.schema.as_str()) || text("table") != Some(d.table.as_str()) {
        return Err("another table");
    }
    let kind = match text("type") {
        Some("INSERT") => "INSERT",
        Some("UPDATE") => "UPDATE",
        Some("DELETE") => "DELETE",
        _ => return Err("not an insert, update or delete"),
    };
    if !d.events.iter().any(|e| e.eq_ignore_ascii_case(kind)) {
        return Err("an operation the trigger doesn't name");
    }
    let field = |k: &str, wanted: bool| -> Result<Value, &'static str> {
        match p.get(k).unwrap_or(&Value::Null) {
            Value::Null if !wanted || row.big => Ok(Value::Null),
            v @ Value::Object(_) if wanted && !row.big => Ok(v.clone()),
            _ => Err("records not shaped for the operation"),
        }
    };
    let record = field("record", kind != "DELETE")?;
    let old_record = field("old_record", kind != "INSERT")?;
    let txid = match p.get("txid") {
        None | Some(Value::Null) => Value::Null,
        Some(v) if v.as_u64().is_some() => v.clone(),
        Some(_) => return Err("a bad txid"),
    };
    let pk = match &row.pk {
        v @ Value::Object(_) => v.clone(),
        _ => Value::Null,
    };
    Ok(Event {
        id: format!("{timeline}:{db}:{}", row.id),
        kind,
        schema: d.schema.clone(),
        table: d.table.clone(),
        record,
        old_record,
        txid,
        big: row.big,
        pk,
    })
}

/// The POST body the function's `fetch` handler gets: `{id, type, schema, table, record,
/// old_record, txid}`, or, over [`MAX_EVENT_BYTES`], the records null with `truncated: true`
/// and `primary_key` (the key's values, null for a table without one).
pub fn event_body(e: &Event) -> Vec<u8> {
    let mut body = Map::new();
    body.insert("id".into(), json!(e.id));
    body.insert("type".into(), json!(e.kind));
    body.insert("schema".into(), json!(e.schema));
    body.insert("table".into(), json!(e.table));
    body.insert("record".into(), e.record.clone());
    body.insert("old_record".into(), e.old_record.clone());
    body.insert("txid".into(), e.txid.clone());
    if !e.big {
        let full = serde_json::to_vec(&body).unwrap_or_default();
        if full.len() <= MAX_EVENT_BYTES {
            return full;
        }
    }
    body.insert("record".into(), Value::Null);
    body.insert("old_record".into(), Value::Null);
    body.insert("truncated".into(), json!(true));
    body.insert("primary_key".into(), e.pk.clone());
    serde_json::to_vec(&body).unwrap_or_default()
}

/// The checks before any statement on the outbox (see the module docs), as the superuser,
/// reading catalogs only; raises [`NOT_READY_MARK`]. `{timeline}` is replaced.
const GUARD: &str = r#"DO $chelabase_fn$ DECLARE
  role pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = '{OWNER}');
  ns pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname = '{SCHEMA}');
  box pg_catalog.oid;
BEGIN
  box := (SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relnamespace = ns AND c.relname = 'outbox');
  IF role IS NULL OR box IS NULL
      OR (SELECT nspowner FROM pg_catalog.pg_namespace WHERE oid = ns) <> role
      OR NOT EXISTS (SELECT FROM pg_catalog.pg_class c WHERE c.oid = box AND c.relkind = 'r' AND c.relowner = role
        AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity)
      OR EXISTS (SELECT FROM pg_catalog.pg_trigger WHERE tgrelid = box)
      OR EXISTS (SELECT FROM pg_catalog.pg_rewrite WHERE ev_class = box)
      OR EXISTS (SELECT FROM pg_catalog.pg_policy WHERE polrelid = box)
      OR EXISTS (SELECT FROM pg_catalog.pg_inherits WHERE inhrelid = box OR inhparent = box) THEN
    RAISE EXCEPTION '{NOT_READY}: the outbox is missing or not as the install makes it';
  END IF;
  IF pg_catalog.current_setting('neon.timeline_id', true) IS DISTINCT FROM {timeline} THEN
    RAISE EXCEPTION '{NOT_READY}: the compute is on another timeline than the branch';
  END IF;
END $chelabase_fn$;
SET LOCAL ROLE {OWNER};
"#;

/// The settings and checks each script starts with, then the role.
fn prelude(timeline: &str) -> String {
    let guard = GUARD
        .replace("{OWNER}", OWNER)
        .replace("{SCHEMA}", FUNCTIONS_SCHEMA)
        .replace("{NOT_READY}", NOT_READY_MARK)
        .replace("{timeline}", &literal(timeline));
    format!(
        "BEGIN;\nSET LOCAL search_path = pg_catalog, pg_temp;\nSET LOCAL event_triggers = off;\nSET LOCAL lock_timeout = '2s';\n{guard}"
    )
}

/// `ARRAY[...]::pg_catalog.uuid[]` of `ids` (UUIDs only: hex and hyphens).
fn uuid_array(ids: impl IntoIterator<Item = Uuid>) -> String {
    let ids: Vec<String> = ids.into_iter().map(|id| format!("'{id}'")).collect();
    format!("ARRAY[{}]::pg_catalog.uuid[]", ids.join(", "))
}

/// The claim (see the module docs): the cleanup and the bound in one transaction, then the
/// wait and the claim in another. Prints one `fn_event <json>` line per claimed row.
pub fn claim_sql(deliver: &[Deliverable], hold: &[Uuid], timeline: &str) -> String {
    let outbox = format!("{FUNCTIONS_SCHEMA}.outbox");
    let known = uuid_array(
        deliver
            .iter()
            .map(|d| d.trigger_id)
            .chain(hold.iter().copied()),
    );
    let wanted = uuid_array(deliver.iter().map(|d| d.trigger_id));
    let tl = literal(timeline);
    let prelude = prelude(timeline);
    let retention = RETENTION.as_secs();
    let lease = LEASE.as_secs();
    let wait = WAIT.as_secs();
    // The tables delivered here, for the primary key (matched by name in the catalogs, which
    // needs no privilege on the table or its schema).
    let tables = if deliver.is_empty() {
        "SELECT NULL::pg_catalog.uuid, NULL::pg_catalog.text, NULL::pg_catalog.text WHERE false"
            .to_string()
    } else {
        let rows: Vec<String> = deliver
            .iter()
            .map(|d| {
                format!(
                    "('{}'::pg_catalog.uuid, {}::pg_catalog.text, {}::pg_catalog.text)",
                    d.trigger_id,
                    literal(&d.schema),
                    literal(&d.table)
                )
            })
            .collect();
        format!("VALUES {}", rows.join(", "))
    };
    let due = format!(
        "o.status = 'pending' AND o.next_attempt_at <= pg_catalog.clock_timestamp() AND o.timeline_id = {tl} AND o.trigger_id = ANY ({wanted})"
    );
    format!(
        r#"{EVENTS_MARK} claim
{prelude}DELETE FROM {outbox} WHERE timeline_id IS DISTINCT FROM {tl} OR trigger_id <> ALL ({known});
DELETE FROM {outbox} WHERE id <= (SELECT id FROM {outbox} ORDER BY id DESC OFFSET {OUTBOX_CAP} LIMIT 1);
DELETE FROM {outbox} WHERE id IN (SELECT id FROM {outbox} ORDER BY id LIMIT 1000)
  AND created_at < pg_catalog.clock_timestamp() - pg_catalog.make_interval(secs => {retention});
COMMIT;
{prelude}DO $chelabase_fn$ DECLARE
  deadline pg_catalog.timestamptz := pg_catalog.clock_timestamp() + pg_catalog.make_interval(secs => {wait});
BEGIN
  WHILE pg_catalog.clock_timestamp() < deadline AND NOT EXISTS (SELECT FROM {outbox} o WHERE {due}) LOOP
    PERFORM pg_catalog.pg_sleep(0.2);
  END LOOP;
END $chelabase_fn$;
WITH c AS (SELECT o.id FROM {outbox} o WHERE {due} ORDER BY o.id LIMIT {CLAIM_BATCH} FOR UPDATE SKIP LOCKED),
  w (trigger_id, schema_name, table_name) AS ({tables}),
  k AS (SELECT w.trigger_id, a.attname::pg_catalog.text AS name
    FROM w JOIN pg_catalog.pg_namespace n ON n.nspname = w.schema_name
    JOIN pg_catalog.pg_class r ON r.relnamespace = n.oid AND r.relname = w.table_name
    JOIN pg_catalog.pg_index i ON i.indrelid = r.oid AND i.indisprimary
    JOIN pg_catalog.pg_attribute a ON a.attrelid = r.oid AND a.attnum = ANY (i.indkey::pg_catalog.int2[]))
UPDATE {outbox} o SET attempts = o.attempts + 1,
  next_attempt_at = pg_catalog.clock_timestamp() + pg_catalog.make_interval(secs => {lease})
FROM c WHERE o.id = c.id
RETURNING 'fn_event ' || pg_catalog.jsonb_build_object(
  'id', o.id, 'trigger_id', o.trigger_id, 'attempts', o.attempts, 'timeline_id', o.timeline_id,
  'big', pg_catalog.octet_length(o.payload::pg_catalog.text) > {MAX_EVENT_BYTES},
  'payload', CASE WHEN pg_catalog.octet_length(o.payload::pg_catalog.text) > {MAX_EVENT_BYTES}
    THEN o.payload - 'record' - 'old_record' ELSE o.payload END,
  'pk', (SELECT pg_catalog.jsonb_object_agg(k.name, coalesce(o.payload -> 'record' -> k.name, o.payload -> 'old_record' -> k.name))
    FROM k WHERE k.trigger_id = o.trigger_id))::pg_catalog.text;
COMMIT;
"#
    )
}

/// The claim's rows; a line that doesn't parse is skipped (its lease runs out, and the next
/// claim's vetting deletes it if it is still bad).
pub fn parse_claimed(out: &str) -> Vec<Claimed> {
    out.lines()
        .filter_map(|l| serde_json::from_str(l.trim().strip_prefix("fn_event ")?).ok())
        .collect()
}

/// Settles a pass: `drops` and delivered rows deleted, the others due again or `dead`.
pub fn settle_sql(settled: &[(i64, Settle)], timeline: &str) -> String {
    let outbox = format!("{FUNCTIONS_SCHEMA}.outbox");
    let deletes: Vec<String> = settled
        .iter()
        .filter(|(_, s)| *s == Settle::Delete)
        .map(|(id, _)| id.to_string())
        .collect();
    let updates: Vec<String> = settled
        .iter()
        .filter_map(|(id, s)| match s {
            Settle::Retry(after) => Some(format!("({id}, {}, 'pending')", after.as_secs())),
            Settle::Dead => Some(format!("({id}, 0, 'dead')")),
            Settle::Delete => None,
        })
        .collect();
    let mut sql = format!("{EVENTS_MARK} settle\n{}", prelude(timeline));
    if !deletes.is_empty() {
        sql.push_str(&format!(
            "DELETE FROM {outbox} WHERE id = ANY (ARRAY[{}]::pg_catalog.int8[]);\n",
            deletes.join(", ")
        ));
    }
    if !updates.is_empty() {
        sql.push_str(&format!(
            "UPDATE {outbox} o SET status = v.status,\n  next_attempt_at = pg_catalog.clock_timestamp() + pg_catalog.make_interval(secs => v.after)\nFROM (VALUES {}) v (id, after, status) WHERE o.id = v.id;\n",
            updates.join(", ")
        ));
    }
    sql.push_str("COMMIT;\n");
    sql
}

/// Whether a pass's task goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    Continue,
    Stop,
}

/// The per-trigger tally of a pass, for the metadata.
fn tally(tally: &mut HashMap<Uuid, TriggerDelivery>, trigger: Uuid, outcome: &Outcome, s: Settle) {
    let entry = tally.entry(trigger).or_insert(TriggerDelivery {
        id: trigger,
        delivered: false,
        dead: 0,
        error: None,
    });
    match outcome {
        Outcome::Delivered => entry.delivered = true,
        Outcome::Failed(why) => entry.error = Some(why.clone()),
    }
    if s == Settle::Dead {
        entry.dead += 1;
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The branch's timeline and endpoint when it is ready, else `None`.
    async fn dispatchable(&self, branch: BranchId) -> Result<Option<(String, String)>, String> {
        Ok(self.ready_endpoint(branch).await?.and_then(|(record, ep)| {
            (record.status == BranchStatus::Ready).then_some((record.timeline_id?, ep))
        }))
    }

    /// One pass over (`branch`, `db`): see the module docs.
    pub async fn dispatch_once(
        &self,
        invoker: &dyn Invoker,
        branch: BranchId,
        db: &str,
    ) -> Result<Pass, String> {
        let Some(meta) = self.metadata.functions() else {
            return Ok(Pass::Stop);
        };
        let Some((timeline, ep)) = self.dispatchable(branch).await? else {
            return Ok(Pass::Stop);
        };
        let triggers = meta
            .list_triggers(branch)
            .await
            .map_err(|e| format!("listing the triggers: {e}"))?;
        let functions = meta
            .list_for_branch(branch)
            .await
            .map_err(|e| format!("listing the functions: {e}"))?;
        let plan = plan(db, &triggers, &functions);
        if !plan.run {
            return Ok(Pass::Stop);
        }
        let opts = SqlOpts {
            timeout: WAIT + Duration::from_secs(25),
            app_name: Some("chelabase-fn-events".into()),
            ..SqlOpts::admin_default()
        };
        let out = self
            .computes
            .admin_sql_with(
                &ep,
                db,
                &claim_sql(&plan.deliver, &plan.hold, &timeline),
                &opts,
            )
            .await
            .map_err(|e| format!("claiming events: {e:#}"))?;
        let claimed = parse_claimed(&out);
        if claimed.is_empty() {
            return Ok(Pass::Continue);
        }
        let mut settled = Vec::with_capacity(claimed.len());
        let mut tallies = HashMap::new();
        let mut checked = Instant::now();
        for row in &claimed {
            let deliver = plan.deliver.iter().find(|d| d.trigger_id == row.trigger_id);
            let event = match vet(row, deliver, &timeline, db) {
                Ok(event) => event,
                Err(why) => {
                    warn!(branch_id = %branch, database = %db, row = row.id, "an outbox row was dropped: {why}");
                    settled.push((row.id, Settle::Delete));
                    continue;
                }
            };
            if checked.elapsed() > RECHECK_EVERY {
                checked = Instant::now();
                if self.dispatchable(branch).await?.as_ref().map(|(t, _)| t) != Some(&timeline) {
                    // The branch went away or moved: the rest are left to their leases.
                    break;
                }
            }
            let Some(d) = deliver else { continue };
            let outcome = match invoker
                .deliver_event(branch, &d.function, event_body(&event))
                .await
            {
                Ok(status) if (200..300).contains(&status) => Outcome::Delivered,
                Ok(status) => Outcome::Failed(format!("the function answered {status}")),
                Err(e) => Outcome::Failed(e),
            };
            let s = settle(row.attempts, &outcome);
            debug!(branch_id = %branch, database = %db, row = row.id, ?s, "event settled");
            tally(&mut tallies, d.trigger_id, &outcome, s);
            settled.push((row.id, s));
        }
        self.computes
            .admin_sql_with(&ep, db, &settle_sql(&settled, &timeline), &opts)
            .await
            .map_err(|e| format!("settling events: {e:#}"))?;
        let deliveries: Vec<TriggerDelivery> = tallies.into_values().collect();
        meta.record_trigger_deliveries(branch, &deliveries)
            .await
            .map_err(|e| format!("recording deliveries: {e}"))?;
        Ok(Pass::Continue)
    }
}

/// One database's task: passes until one says stop; a failed pass waits [`WAIT`] first.
async fn run_target<M, S, C, N, P>(
    worker: Arc<Worker<M, S, C, N, P>>,
    invoker: Arc<dyn Invoker>,
    branch: BranchId,
    db: String,
) where
    M: WorkerMetadata,
    S: Storage,
    C: ComputeBackend,
    N: CacheNotifier,
    P: SpecPusher,
{
    loop {
        match worker.dispatch_once(&*invoker, branch, &db).await {
            Ok(Pass::Continue) => {}
            Ok(Pass::Stop) => return,
            Err(e) => {
                warn!(branch_id = %branch, database = %db, "function events: {e}");
                tokio::time::sleep(WAIT).await;
            }
        }
    }
}

/// The supervisor: every [`SUPERVISE_EVERY`], a task for each dispatch target without one.
pub async fn dispatch_loop<M, S, C, N, P>(
    worker: Arc<Worker<M, S, C, N, P>>,
    invoker: Arc<dyn Invoker>,
) where
    M: WorkerMetadata + 'static,
    S: Storage + 'static,
    C: ComputeBackend + 'static,
    N: CacheNotifier + 'static,
    P: SpecPusher + 'static,
    Worker<M, S, C, N, P>: Send + Sync,
{
    let mut tasks: HashMap<(BranchId, String), tokio::task::JoinHandle<()>> = HashMap::new();
    let mut every = tokio::time::interval(SUPERVISE_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        tasks.retain(|_, t| !t.is_finished());
        let Some(meta) = worker.metadata.functions() else {
            return;
        };
        let targets = match meta.dispatch_targets(MAX_TARGETS).await {
            Ok(targets) => targets,
            Err(e) => {
                warn!("listing the databases to dispatch function events for: {e}");
                continue;
            }
        };
        for target in targets {
            if tasks.contains_key(&target) {
                continue;
            }
            let (branch, db) = target.clone();
            let task = tokio::spawn(run_target(worker.clone(), invoker.clone(), branch, db));
            tasks.insert(target, task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const TL: &str = "0123456789abcdef0123456789abcdef";

    fn deliverable() -> Deliverable {
        Deliverable {
            trigger_id: Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap(),
            function: "hello".into(),
            schema: "public".into(),
            table: "orders".into(),
            events: vec!["insert".into(), "update".into()],
        }
    }

    fn row(payload: Value) -> Claimed {
        Claimed {
            id: 7,
            trigger_id: deliverable().trigger_id,
            attempts: 1,
            timeline_id: Some(TL.into()),
            payload,
            big: false,
            pk: json!({"id": 1}),
        }
    }

    fn insert() -> Value {
        json!({"type": "INSERT", "schema": "public", "table": "orders", "record": {"id": 1}, "old_record": null, "txid": 42})
    }

    fn trigger(state: TriggerState, enabled: bool, function_id: Uuid) -> TriggerRecord {
        TriggerRecord {
            id: Uuid::new_v4(),
            branch_id: BranchId::new(),
            function_id,
            source: TriggerSource::Table,
            enabled,
            state,
            enabled_since: chrono::Utc::now(),
            pg_database: "app".into(),
            schema_name: Some("public".into()),
            table_name: Some("orders".into()),
            bucket: None,
            prefix: None,
            events: vec!["insert".into()],
            last_error: None,
            dead_count: 0,
            last_delivery_error: None,
            last_delivered_at: None,
        }
    }

    fn function(id: Uuid, enabled: bool) -> BranchFunction {
        BranchFunction {
            function_id: id,
            name: "hello".into(),
            kind: FunctionKind::Function,
            deployment_id: Uuid::new_v4(),
            enabled,
            auth: chelabase_metadata::FunctionAuth::Key,
            run_as_role: None,
            run_as_key_id: None,
            run_as_role_oid: None,
            run_as_unmintable: None,
            last_error: None,
            updated_at: chrono::Utc::now(),
            health: None,
        }
    }

    /// The untrusted row's checks: its trigger delivered here, its timeline, its table, its
    /// operation among the trigger's, and records shaped for it. The event is built from the
    /// trigger's names, never the row's extra keys.
    #[test]
    fn fn_events_vet_drops_forged_rows() {
        let d = deliverable();
        let ok = vet(&row(insert()), Some(&d), TL, "app").unwrap();
        assert_eq!(
            (ok.id.as_str(), ok.kind, ok.txid.clone()),
            (format!("{TL}:app:7").as_str(), "INSERT", json!(42))
        );
        assert_eq!(
            vet(&row(insert()), None, TL, "app"),
            Err("not a trigger delivered here")
        );
        let mut other = row(insert());
        other.trigger_id = Uuid::new_v4();
        assert_eq!(
            vet(&other, Some(&d), TL, "app"),
            Err("not a trigger delivered here")
        );
        let mut old_tl = row(insert());
        old_tl.timeline_id = Some("ffff".into());
        assert_eq!(vet(&old_tl, Some(&d), TL, "app"), Err("another timeline"));
        old_tl.timeline_id = None;
        assert_eq!(vet(&old_tl, Some(&d), TL, "app"), Err("another timeline"));
        let mut p = insert();
        p["table"] = json!("secrets");
        assert_eq!(vet(&row(p), Some(&d), TL, "app"), Err("another table"));
        let mut p = insert();
        p["schema"] = json!("private");
        assert_eq!(vet(&row(p), Some(&d), TL, "app"), Err("another table"));
        let mut p = insert();
        p["type"] = json!("DELETE");
        p["old_record"] = json!({"id": 1});
        p["record"] = Value::Null;
        assert_eq!(
            vet(&row(p), Some(&d), TL, "app"),
            Err("an operation the trigger doesn't name")
        );
        let mut p = insert();
        p["type"] = json!("TRUNCATE");
        assert_eq!(
            vet(&row(p), Some(&d), TL, "app"),
            Err("not an insert, update or delete")
        );
        let mut p = insert();
        p["old_record"] = json!({"id": 0});
        assert_eq!(
            vet(&row(p), Some(&d), TL, "app"),
            Err("records not shaped for the operation")
        );
        let mut p = insert();
        p["record"] = json!("text");
        assert_eq!(
            vet(&row(p), Some(&d), TL, "app"),
            Err("records not shaped for the operation")
        );
        let mut p = insert();
        p["txid"] = json!("1; drop");
        assert_eq!(vet(&row(p), Some(&d), TL, "app"), Err("a bad txid"));
        assert_eq!(
            vet(&row(json!([1])), Some(&d), TL, "app"),
            Err("not an object")
        );
        let mut p = insert();
        p["extra"] = json!("leak");
        let body: Value =
            serde_json::from_slice(&event_body(&vet(&row(p), Some(&d), TL, "app").unwrap()))
                .unwrap();
        assert_eq!(
            body,
            json!({"id": format!("{TL}:app:7"), "type": "INSERT", "schema": "public", "table": "orders", "record": {"id": 1}, "old_record": null, "txid": 42})
        );
    }

    /// Over 256 KiB: records null, `truncated`, the primary key; the claim's `big` rows too.
    #[test]
    fn fn_events_truncates_over_256_kib() {
        let d = deliverable();
        let mut p = insert();
        p["record"] = json!({"id": 1, "blob": "x".repeat(MAX_EVENT_BYTES)});
        let body: Value =
            serde_json::from_slice(&event_body(&vet(&row(p), Some(&d), TL, "app").unwrap()))
                .unwrap();
        assert_eq!(
            (body["record"].clone(), body["old_record"].clone()),
            (Value::Null, Value::Null)
        );
        assert_eq!(
            (body["truncated"].clone(), body["primary_key"].clone()),
            (json!(true), json!({"id": 1}))
        );
        let mut small = insert();
        small["record"] = json!({"id": 1, "blob": "x".repeat(MAX_EVENT_BYTES - 200)});
        let body: Value =
            serde_json::from_slice(&event_body(&vet(&row(small), Some(&d), TL, "app").unwrap()))
                .unwrap();
        assert!(body.get("truncated").is_none(), "just under: whole");
        let mut big =
            row(json!({"type": "INSERT", "schema": "public", "table": "orders", "txid": 1}));
        big.big = true;
        let body: Value =
            serde_json::from_slice(&event_body(&vet(&big, Some(&d), TL, "app").unwrap())).unwrap();
        assert_eq!(
            (body["truncated"].clone(), body["primary_key"].clone()),
            (json!(true), json!({"id": 1}))
        );
    }

    /// Pending rows are held (nothing delivered for them), a disabled function's are held, a
    /// disabled or failed trigger's are neither (the claim deletes them), and another
    /// database's are ignored. The task runs only while an installed, enabled trigger is here.
    #[test]
    fn fn_events_plan_holds_pending_and_disabled() {
        let f = Uuid::new_v4();
        let installed = trigger(TriggerState::Installed, true, f);
        let pending = trigger(TriggerState::PendingInstall, true, f);
        let failed = trigger(TriggerState::Failed, true, f);
        let off = trigger(TriggerState::Installed, false, f);
        let mut elsewhere = trigger(TriggerState::Installed, true, f);
        elsewhere.pg_database = "other".into();
        let all = [installed.clone(), pending.clone(), failed, off, elsewhere];
        let p = plan("app", &all, &[function(f, true)]);
        assert!(p.run);
        assert_eq!(
            p.deliver.iter().map(|d| d.trigger_id).collect::<Vec<_>>(),
            vec![installed.id]
        );
        assert_eq!(p.hold, vec![pending.id]);
        let p = plan("app", &all, &[function(f, false)]);
        assert!(p.run && p.deliver.is_empty());
        assert_eq!(
            p.hold,
            vec![installed.id, pending.id],
            "a disabled function's events queue"
        );
        let p = plan("app", &all, &[]);
        assert!(p.deliver.is_empty(), "no deployment on the branch: held");
        let p = plan("app", std::slice::from_ref(&pending), &[function(f, true)]);
        assert!(!p.run, "nothing installed: the task stops");
        assert!(
            !plan("app", &[], &[function(f, true)]).run,
            "every trigger deleted: the task stops"
        );
    }

    /// The scripts: the guard before the role, every statement as the owner role, names as
    /// literals, the claim's skip-locked lease and bound.
    #[test]
    fn fn_events_scripts_are_locked_down() {
        let mut d = deliverable();
        d.table = r#"or"d's $chelabase_fn$"#.into();
        let hold = Uuid::new_v4();
        let sql = claim_sql(&[d.clone()], &[hold], TL);
        assert!(sql.starts_with(&format!("{EVENTS_MARK} claim\nBEGIN;\nSET LOCAL search_path = pg_catalog, pg_temp;\nSET LOCAL event_triggers = off;\n")));
        assert_eq!(
            sql.matches("SET LOCAL ROLE chelabase_functions_owner;")
                .count(),
            2
        );
        let guard_at = sql.find(NOT_READY_MARK).unwrap();
        assert!(
            guard_at < sql.find("SET LOCAL ROLE").unwrap(),
            "the guard runs first"
        );
        assert!(sql.contains(&literal(&d.table)) && !sql.contains(&format!(" {} ", d.table)));
        assert!(sql.contains("FOR UPDATE SKIP LOCKED"));
        assert!(sql.contains(&format!(
            "trigger_id <> ALL (ARRAY['{}', '{hold}']::pg_catalog.uuid[])",
            d.trigger_id
        )));
        assert!(sql.contains(&format!(
            "o.trigger_id = ANY (ARRAY['{}']::pg_catalog.uuid[])",
            d.trigger_id
        )));
        assert!(sql.contains(&format!("OFFSET {OUTBOX_CAP}")));
        assert!(sql.contains(&format!("IS DISTINCT FROM '{TL}'")));
        let settle = settle_sql(
            &[
                (1, Settle::Delete),
                (2, Settle::Dead),
                (3, Settle::Retry(Duration::from_secs(4))),
            ],
            TL,
        );
        assert!(settle.contains("SET LOCAL ROLE chelabase_functions_owner;"));
        assert!(settle.contains("id = ANY (ARRAY[1]::pg_catalog.int8[])"));
        assert!(settle.contains("(2, 0, 'dead'), (3, 4, 'pending')"));
        let line = format!(
            "fn_event {}",
            json!({"id": 3, "trigger_id": d.trigger_id, "attempts": 2, "timeline_id": TL, "big": false, "payload": insert(), "pk": null})
        );
        let parsed = parse_claimed(&format!("{line}\nnoise\n"));
        assert_eq!((parsed.len(), parsed[0].id, parsed[0].attempts), (1, 3, 2));
    }

    #[test]
    fn fn_events_backoff_doubles_to_five_minutes() {
        let secs: Vec<u64> = (1..=MAX_ATTEMPTS).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 300]);
        assert_eq!(
            settle(MAX_ATTEMPTS - 1, &Outcome::Failed("500".into())),
            Settle::Retry(Duration::from_secs(256))
        );
        assert_eq!(
            settle(MAX_ATTEMPTS, &Outcome::Failed("500".into())),
            Settle::Dead
        );
        assert_eq!(settle(MAX_ATTEMPTS, &Outcome::Delivered), Settle::Delete);
    }

    /// One delivery attempt in the model: a 2xx, a 5xx, a timeout, or the worker stopping
    /// after the function ran and before the settle (its lease runs out).
    #[derive(Debug, Clone, Copy)]
    enum Step {
        Ok,
        Status5xx,
        Timeout,
        Crash,
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            Just(Step::Ok),
            Just(Step::Status5xx),
            Just(Step::Timeout),
            Just(Step::Crash)
        ]
    }

    #[derive(Debug, Clone)]
    struct Row {
        attempts: i32,
        due: u64,
        dead: bool,
    }

    proptest! {
        /// The dispatcher model (#175): random 2xx/5xx/timeout/crash sequences per event, run
        /// through the claim's semantics (due, pending, attempt counted, lease) and
        /// [`settle`]: every event is delivered at least once or goes dead at its
        /// [`MAX_ATTEMPTS`]th attempt (later only when the worker stopped during that one),
        /// none is retried after it or delivered after a settled 2xx, and a dead one is never
        /// claimed again.
        #[test]
        fn fn_events_model_at_least_once(scripts in prop::collection::vec(prop::collection::vec(step(), 0..24), 1..8)) {
            let mut rows: Vec<Option<Row>> = scripts.iter().map(|_| Some(Row { attempts: 0, due: 0, dead: false })).collect();
            let mut cursor = vec![0usize; scripts.len()];
            let mut delivered = vec![0u32; scripts.len()];
            let mut settled_ok = vec![false; scripts.len()];
            let mut now = 0u64;
            for _ in 0..10_000 {
                let due: Vec<usize> = rows.iter().enumerate()
                    .filter(|(_, r)| r.as_ref().is_some_and(|r| !r.dead && r.due <= now))
                    .map(|(i, _)| i).take(CLAIM_BATCH as usize).collect();
                if due.is_empty() {
                    let next = rows.iter().flatten().filter(|r| !r.dead).map(|r| r.due).min();
                    match next { Some(t) => { now = t; continue } None => break }
                }
                for i in due {
                    prop_assert!(!settled_ok[i], "claimed after a settled 2xx");
                    let r = rows[i].as_mut().unwrap();
                    r.attempts += 1;
                    r.due = now + LEASE.as_secs();
                    // Past the script's end, the function answers 2xx.
                    let s = scripts[i].get(cursor[i]).copied().unwrap_or(Step::Ok);
                    cursor[i] += 1;
                    let outcome = match s {
                        Step::Ok => { delivered[i] += 1; Outcome::Delivered }
                        Step::Status5xx => Outcome::Failed("the function answered 500".into()),
                        Step::Timeout => Outcome::Failed("timed out".into()),
                        Step::Crash => { delivered[i] += 1; continue }
                    };
                    match settle(r.attempts, &outcome) {
                        Settle::Delete => { rows[i] = None; settled_ok[i] = true; }
                        Settle::Retry(after) => {
                            prop_assert!(r.attempts < MAX_ATTEMPTS, "no retry after the last attempt");
                            r.due = now + after.as_secs();
                        }
                        Settle::Dead => {
                            prop_assert!(r.attempts >= MAX_ATTEMPTS, "dead before the last attempt");
                            r.dead = true;
                        }
                    }
                }
                now += 1;
            }
            for (i, r) in rows.iter().enumerate() {
                match r {
                    None => prop_assert!(settled_ok[i] && delivered[i] >= 1),
                    Some(r) => {
                        prop_assert!(r.dead, "every event ends delivered or dead");
                        prop_assert!(r.attempts >= MAX_ATTEMPTS, "dead only after {} attempts", MAX_ATTEMPTS);
                    }
                }
            }
        }
    }
}
