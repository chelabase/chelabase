//! The dispatcher's sessions, passes and supervisor (see [`super`]).
//!
//! **The connection.** Each target's session connects to the branch's compute as
//! [`LOGIN`] (`chelabase_functions_dispatch`), whose connection info (address, the compute's
//! TLS name, the branch's password) the orchestrator's internal listener hands to this
//! service's bearer only (`/internal/functions/branches/{id}/connection`), as it hands rest
//! `authenticator`'s. TLS is verified against the platform CA as the compute's name, and the
//! session must show it is the bare login (no superuser or other attribute that could pass
//! rights on, no member, no membership) before anything runs
//! (`chelabase_pgsession::connect_dedicated`). The worker's install grants the login exactly
//! `USAGE` on `chelabase_functions` and `SELECT, UPDATE, DELETE` on the outbox, and nothing
//! else (an allowlist its grant reset keeps): it reads no member table, can't call
//! `enqueue()`, and can't `SET ROLE` to anything.
//!
//! Every statement names its objects qualified, under `search_path = pg_catalog, pg_temp`
//! (pinned on the role and on the connection), with the role's `event_triggers = off`; before
//! each pass a catalog guard checks that the outbox is the install's (the owner's plain
//! table, without triggers, rules, policies, row security or inheritance) and that the
//! compute is on the branch's timeline. So no object a member can create runs in the session.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chelabase_metadata::{
    BranchFunction, BranchRecord, BranchStatus, MetadataClient, TriggerDelivery, TriggerRecord,
};
use chelabase_pgsession::{ConnError, ConnSource, Service, connect_dedicated};
use chelabase_query::server_tls::ServerTls;
use chelabase_types::BranchId;
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;
use tracing::{debug, warn};
use uuid::Uuid;

use super::{
    CLAIM_BATCH, Claimed, Deliverable, LEASE, MAX_EVENT_BYTES, OUTBOX_CAP, Outcome, Plan,
    RETENTION, SUPERVISE_EVERY, SWEEP_EVERY, Settle, event_body, plan, settle, vet,
};

/// The channel `enqueue()` notifies after each insert.
pub const CHANNEL: &str = "chelabase_functions";
/// The dispatcher's login, made and granted by the worker's install.
pub const LOGIN: &str = "chelabase_functions_dispatch";
/// The most databases one service dispatches at once.
pub const MAX_TARGETS: usize = 500;
/// A failed session waits this long before it connects again.
pub const RETRY: Duration = Duration::from_secs(5);

/// What the dispatcher reads and records in the metadata.
#[async_trait::async_trait]
pub trait EventsMetadata: Send + Sync + 'static {
    async fn branch(&self, branch: BranchId) -> Result<Option<BranchRecord>, String>;
    async fn triggers(&self, branch: BranchId) -> Result<Vec<TriggerRecord>, String>;
    async fn functions(&self, branch: BranchId) -> Result<Vec<BranchFunction>, String>;
    /// (ready branch, Postgres database) pairs with an installed, enabled table trigger.
    async fn targets(&self, limit: i64) -> Result<Vec<(BranchId, String)>, String>;
    async fn record(&self, branch: BranchId, deliveries: &[TriggerDelivery]) -> Result<(), String>;
}

#[async_trait::async_trait]
impl EventsMetadata for MetadataClient {
    async fn branch(&self, branch: BranchId) -> Result<Option<BranchRecord>, String> {
        self.get_branch(branch)
            .await
            .map_err(|e| format!("reading the branch: {e}"))
    }
    async fn triggers(&self, branch: BranchId) -> Result<Vec<TriggerRecord>, String> {
        self.list_triggers(branch)
            .await
            .map_err(|e| format!("listing the triggers: {e}"))
    }
    async fn functions(&self, branch: BranchId) -> Result<Vec<BranchFunction>, String> {
        self.list_for_branch(branch)
            .await
            .map_err(|e| format!("listing the functions: {e}"))
    }
    async fn targets(&self, limit: i64) -> Result<Vec<(BranchId, String)>, String> {
        self.dispatch_targets(limit)
            .await
            .map_err(|e| format!("listing the databases to dispatch: {e}"))
    }
    async fn record(&self, branch: BranchId, deliveries: &[TriggerDelivery]) -> Result<(), String> {
        self.record_trigger_deliveries(branch, deliveries)
            .await
            .map(drop)
            .map_err(|e| format!("recording deliveries: {e}"))
    }
}

/// Where an event goes: the function's `fetch` handler, in this process
/// (`crate::app::AppState`), marked `chelabase-event: db`. Its status, or why there was none.
#[async_trait::async_trait]
pub trait Invoker: Send + Sync + 'static {
    async fn deliver(&self, branch: BranchId, function: &str, body: Vec<u8>)
    -> Result<u16, String>;
}

/// What a pass leaves for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// The target is done: the branch isn't ready, or no installed, enabled trigger is left.
    Stop,
    /// A whole batch was claimed: pass again at once.
    More,
    /// Nothing more is due before this long (a notification may come first).
    Idle(Duration),
}

/// One target's connection.
pub struct Session {
    pub branch: BranchId,
    pub database: String,
    /// The branch's timeline when the session opened: the compute's, by the guard.
    pub timeline: String,
    client: Client,
    notifications: tokio::sync::mpsc::UnboundedReceiver<tokio_postgres::Notification>,
    last_sweep: Option<Instant>,
}

/// Why a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woken {
    Notified,
    TimedOut,
}

/// The catalog guard (see the module docs): `(outbox as the install makes it, compute on the
/// branch's timeline)`.
const GUARD_SQL: &str = "SELECT EXISTS (SELECT FROM pg_catalog.pg_class c
    JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) c.relowner AND r.oid OPERATOR(pg_catalog.=) n.nspowner
    WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase_functions' AND c.relname OPERATOR(pg_catalog.=) 'outbox'
      AND c.relkind OPERATOR(pg_catalog.=) 'r' AND r.rolname OPERATOR(pg_catalog.=) 'chelabase_functions_owner'
      AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity
      AND NOT EXISTS (SELECT FROM pg_catalog.pg_trigger t WHERE t.tgrelid OPERATOR(pg_catalog.=) c.oid)
      AND NOT EXISTS (SELECT FROM pg_catalog.pg_rewrite w WHERE w.ev_class OPERATOR(pg_catalog.=) c.oid)
      AND NOT EXISTS (SELECT FROM pg_catalog.pg_policy p WHERE p.polrelid OPERATOR(pg_catalog.=) c.oid)
      AND NOT EXISTS (SELECT FROM pg_catalog.pg_inherits i
        WHERE i.inhrelid OPERATOR(pg_catalog.=) c.oid OR i.inhparent OPERATOR(pg_catalog.=) c.oid)),
  pg_catalog.current_setting('neon.timeline_id', true) IS NOT DISTINCT FROM $1::pg_catalog.text";

/// The sweep's cleanup: rows of another timeline, and rows of any trigger neither delivered
/// nor held (`$2`); the trigger id of each.
const CLEANUP_SQL: &str = "DELETE FROM chelabase_functions.outbox
  WHERE timeline_id IS DISTINCT FROM $1::pg_catalog.text
     OR trigger_id OPERATOR(pg_catalog.<>) ALL ($2::pg_catalog.text[]::pg_catalog.uuid[])
  RETURNING trigger_id::pg_catalog.text";

/// The bound (B1): dead rows only, past the newest `$1` dead ones or dead for longer than `$2`
/// seconds; the trigger id of each. A pending row is never touched.
const BOUND_SQL: &str = "DELETE FROM chelabase_functions.outbox
  WHERE status OPERATOR(pg_catalog.=) 'dead'
    AND (id OPERATOR(pg_catalog.<=) (SELECT d.id FROM chelabase_functions.outbox d WHERE d.status OPERATOR(pg_catalog.=) 'dead'
           ORDER BY d.id DESC OFFSET $1::pg_catalog.int8 LIMIT 1)
      OR created_at OPERATOR(pg_catalog.<) (pg_catalog.clock_timestamp() OPERATOR(pg_catalog.-) pg_catalog.make_interval(secs => $2::pg_catalog.float8)))
  RETURNING trigger_id::pg_catalog.text";

/// Settles the rows `$1`..: deletes the delivered and dropped ones (`$1` alone), or sets
/// status, delay and the attempt taken back of the others.
const DELETE_SQL: &str = "DELETE FROM chelabase_functions.outbox WHERE id OPERATOR(pg_catalog.=) ANY ($1::pg_catalog.int8[])";
const UPDATE_SQL: &str = "UPDATE chelabase_functions.outbox o SET status = v.status,
    attempts = o.attempts OPERATOR(pg_catalog.-) v.back,
    next_attempt_at = pg_catalog.clock_timestamp() OPERATOR(pg_catalog.+) pg_catalog.make_interval(secs => v.after)
  FROM ROWS FROM (pg_catalog.unnest($1::pg_catalog.int8[]), pg_catalog.unnest($2::pg_catalog.float8[]),
      pg_catalog.unnest($3::pg_catalog.text[]), pg_catalog.unnest($4::pg_catalog.int4[])) AS v (id, after, status, back)
  WHERE o.id OPERATOR(pg_catalog.=) v.id";

/// Seconds until the next pending row of a delivered trigger (`$2`) is due; NULL for none.
const NEXT_DUE_SQL: &str = "SELECT pg_catalog.date_part('epoch', pg_catalog.min(o.next_attempt_at) OPERATOR(pg_catalog.-) pg_catalog.clock_timestamp())
  FROM chelabase_functions.outbox o
  WHERE o.status OPERATOR(pg_catalog.=) 'pending' AND o.timeline_id OPERATOR(pg_catalog.=) $1::pg_catalog.text
    AND o.trigger_id OPERATOR(pg_catalog.=) ANY ($2::pg_catalog.text[]::pg_catalog.uuid[])";

/// The claim: up to `$3` due rows of the delivered triggers (`$2`) on the timeline `$1`,
/// skip-locked, their attempt counted and leased for `$7` seconds; each as `(id, its JSON)`.
/// The primary key of each delivered table (`$4`..`$6`: trigger id, schema, table) is found by
/// name in the catalogs (which needs no privilege on the table).
fn claim_sql() -> String {
    format!(
        "WITH c AS (SELECT o.id FROM chelabase_functions.outbox o
    WHERE o.status OPERATOR(pg_catalog.=) 'pending' AND o.next_attempt_at OPERATOR(pg_catalog.<=) pg_catalog.clock_timestamp()
      AND o.timeline_id OPERATOR(pg_catalog.=) $1::pg_catalog.text
      AND o.trigger_id OPERATOR(pg_catalog.=) ANY ($2::pg_catalog.text[]::pg_catalog.uuid[])
    ORDER BY o.id LIMIT $3::pg_catalog.int8 FOR UPDATE SKIP LOCKED),
  w (trigger_id, schema_name, table_name) AS (SELECT u.t::pg_catalog.uuid, u.s, u.n
    FROM ROWS FROM (pg_catalog.unnest($4::pg_catalog.text[]), pg_catalog.unnest($5::pg_catalog.text[]),
      pg_catalog.unnest($6::pg_catalog.text[])) AS u (t, s, n)),
  k AS (SELECT w.trigger_id, a.attname::pg_catalog.text AS name
    FROM w JOIN pg_catalog.pg_namespace n ON n.nspname OPERATOR(pg_catalog.=) w.schema_name
    JOIN pg_catalog.pg_class r ON r.relnamespace OPERATOR(pg_catalog.=) n.oid AND r.relname OPERATOR(pg_catalog.=) w.table_name
    JOIN pg_catalog.pg_index i ON i.indrelid OPERATOR(pg_catalog.=) r.oid AND i.indisprimary
    JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) r.oid
      AND a.attnum OPERATOR(pg_catalog.=) ANY (i.indkey::pg_catalog.int2[]))
UPDATE chelabase_functions.outbox o SET attempts = o.attempts OPERATOR(pg_catalog.+) 1,
  next_attempt_at = pg_catalog.clock_timestamp() OPERATOR(pg_catalog.+) pg_catalog.make_interval(secs => $7::pg_catalog.float8)
FROM c WHERE o.id OPERATOR(pg_catalog.=) c.id
RETURNING o.id, pg_catalog.jsonb_build_object(
  'id', o.id, 'trigger_id', o.trigger_id, 'attempts', o.attempts, 'timeline_id', o.timeline_id,
  'big', pg_catalog.octet_length(o.payload::pg_catalog.text) OPERATOR(pg_catalog.>) {MAX_EVENT_BYTES},
  'payload', CASE WHEN pg_catalog.octet_length(o.payload::pg_catalog.text) OPERATOR(pg_catalog.>) {MAX_EVENT_BYTES}
    THEN o.payload OPERATOR(pg_catalog.-) 'record' OPERATOR(pg_catalog.-) 'old_record' ELSE o.payload END,
  'pk', (SELECT pg_catalog.jsonb_object_agg(k.name, coalesce(o.payload OPERATOR(pg_catalog.->) 'record' OPERATOR(pg_catalog.->) k.name,
      o.payload OPERATOR(pg_catalog.->) 'old_record' OPERATOR(pg_catalog.->) k.name))
    FROM k WHERE k.trigger_id OPERATOR(pg_catalog.=) o.trigger_id))::pg_catalog.text"
    )
}

/// The per-trigger tally of a pass, for the metadata.
#[derive(Default)]
struct Tally(HashMap<Uuid, TriggerDelivery>);

impl Tally {
    fn entry(&mut self, trigger: Uuid) -> &mut TriggerDelivery {
        self.0.entry(trigger).or_insert(TriggerDelivery {
            id: trigger,
            delivered: false,
            dead: 0,
            dropped: 0,
            error: None,
        })
    }

    fn settled(&mut self, trigger: Uuid, outcome: &Outcome, s: Settle) {
        let entry = self.entry(trigger);
        match outcome {
            Outcome::Delivered => entry.delivered = true,
            Outcome::Failed(why) => entry.error = Some(why.clone()),
        }
        if s == Settle::Dead {
            entry.dead += 1;
        }
    }

    /// Rows removed undelivered, counted for the triggers the metadata still has.
    fn dropped(&mut self, trigger: Uuid, known: &[TriggerRecord]) {
        if known.iter().any(|t| t.id == trigger) {
            self.entry(trigger).dropped += 1;
        }
    }
}

fn pg(what: &str) -> impl Fn(tokio_postgres::Error) -> String + '_ {
    move |e| match e.as_db_error() {
        Some(db) => format!("{what}: {}", db.message()),
        None => format!("{what}: {e}"),
    }
}

fn ids(ids: impl IntoIterator<Item = Uuid>) -> Vec<String> {
    ids.into_iter().map(|id| id.to_string()).collect()
}

/// The dispatcher: its metadata, where its login's connection info comes from, the platform
/// CA, and where events go.
pub struct Dispatcher<M, S> {
    meta: Arc<M>,
    source: S,
    tls: Option<ServerTls>,
    invoker: Arc<dyn Invoker>,
}

impl<M: EventsMetadata, S: ConnSource> Dispatcher<M, S> {
    /// `tls`: computes are verified against it (`None` only for plain test targets).
    pub fn new(meta: Arc<M>, source: S, tls: Option<ServerTls>, invoker: Arc<dyn Invoker>) -> Self {
        Dispatcher {
            meta,
            source,
            tls,
            invoker,
        }
    }

    /// The branch's timeline, its triggers and the plan for `db`, or `None` when the branch
    /// isn't ready (or gone).
    async fn current(
        &self,
        branch: BranchId,
        db: &str,
    ) -> Result<Option<(String, Vec<TriggerRecord>, Plan)>, String> {
        let Some(timeline) = self
            .meta
            .branch(branch)
            .await?
            .filter(|b| b.status == BranchStatus::Ready)
            .and_then(|b| b.timeline_id)
        else {
            return Ok(None);
        };
        let triggers = self.meta.triggers(branch).await?;
        let functions = self.meta.functions(branch).await?;
        let plan = plan(db, &triggers, &functions);
        Ok(Some((timeline, triggers, plan)))
    }

    /// Connects to `db` of the ready `branch` as [`LOGIN`] and listens; `None` when there is
    /// nothing to dispatch there (the branch isn't ready or gone, or nothing is installed).
    pub async fn open(&self, branch: BranchId, db: &str) -> Result<Option<Session>, String> {
        let Some((timeline, _, plan)) = self.current(branch, db).await? else {
            return Ok(None);
        };
        if !plan.run {
            return Ok(None);
        }
        let info = match self.source.fetch(branch.as_uuid()).await {
            Ok(info) => info,
            Err(ConnError::UnknownBranch) => return Ok(None),
            Err(e) => return Err(format!("the dispatch login's connection info: {e}")),
        };
        let conn = connect_dedicated(&info, db, self.tls.as_ref(), Service::Functions)
            .await
            .map_err(|e| format!("connecting as {LOGIN}: {e}"))?;
        conn.client
            .batch_execute(&format!(
                "SET statement_timeout = '30s'; SET lock_timeout = '2s'; LISTEN {CHANNEL}"
            ))
            .await
            .map_err(pg("listening"))?;
        Ok(Some(Session {
            branch,
            database: db.to_owned(),
            timeline,
            client: conn.client,
            notifications: conn.notifications,
            last_sweep: None,
        }))
    }

    /// One pass (see [`super`]); a sweep when [`SWEEP_EVERY`] has passed since the last one
    /// (or this is the session's first).
    pub async fn pass(&self, s: &mut Session) -> Result<Pass, String> {
        let sweep = s.last_sweep.is_none_or(|t| t.elapsed() >= SWEEP_EVERY);
        let (branch, db) = (s.branch, s.database.clone());
        let Some((timeline, known, plan)) = self.current(branch, &db).await? else {
            return Ok(Pass::Stop);
        };
        if timeline != s.timeline {
            return Err("the branch moved to another timeline; connecting again".into());
        }
        if !plan.run {
            return Ok(Pass::Stop);
        }
        let guard = s
            .client
            .query_one(GUARD_SQL, &[&s.timeline])
            .await
            .map_err(pg("the guard"))?;
        if !guard.get::<_, bool>(0) {
            return Err("the outbox is missing or not as the install makes it".into());
        }
        if !guard.get::<_, bool>(1) {
            return Err("the compute is on another timeline than the branch".into());
        }
        let mut tally = Tally::default();
        if sweep {
            s.last_sweep = Some(Instant::now());
            let kept = ids(plan
                .deliver
                .iter()
                .map(|d| d.trigger_id)
                .chain(plan.hold.iter().copied()));
            let mut removed = s
                .client
                .query(CLEANUP_SQL, &[&s.timeline, &kept])
                .await
                .map_err(pg("cleaning the outbox"))?;
            let retention = RETENTION.as_secs_f64();
            removed.extend(
                s.client
                    .query(BOUND_SQL, &[&OUTBOX_CAP, &retention])
                    .await
                    .map_err(pg("bounding the outbox"))?,
            );
            for row in removed {
                if let Ok(id) = row.get::<_, String>(0).parse() {
                    tally.dropped(id, &known);
                }
            }
        }
        let wanted = ids(plan.deliver.iter().map(|d| d.trigger_id));
        let (tids, schemas, tables): (Vec<String>, Vec<String>, Vec<String>) = (
            ids(plan.deliver.iter().map(|d| d.trigger_id)),
            plan.deliver.iter().map(|d| d.schema.clone()).collect(),
            plan.deliver.iter().map(|d| d.table.clone()).collect(),
        );
        let lease = LEASE.as_secs_f64();
        let params: [&(dyn ToSql + Sync); 7] = [
            &s.timeline,
            &wanted,
            &CLAIM_BATCH,
            &tids,
            &schemas,
            &tables,
            &lease,
        ];
        let claimed = s
            .client
            .query(&claim_sql(), &params)
            .await
            .map_err(pg("claiming events"))?;
        let full = claimed.len() as i64 >= CLAIM_BATCH;
        let mut settled: Vec<(i64, Settle)> = Vec::with_capacity(claimed.len());
        let mut stop = false;
        for row in &claimed {
            let id: i64 = row.get(0);
            // N3: a row that doesn't parse is untrusted and deleted, never leased forever.
            let Ok(row) = serde_json::from_str::<Claimed>(&row.get::<_, String>(1)) else {
                warn!(branch_id = %branch, database = %db, row = id, "an outbox row was dropped: it doesn't parse");
                settled.push((id, Settle::Delete));
                continue;
            };
            let found = plan.deliver.iter().find(|d| d.trigger_id == row.trigger_id);
            let (event, d) = match (vet(&row, found, &s.timeline, &db), found) {
                (Ok(event), Some(d)) => (event, d),
                (Err(why), _) => {
                    warn!(branch_id = %branch, database = %db, row = id, "an outbox row was dropped: {why}");
                    settled.push((id, Settle::Delete));
                    continue;
                }
                (Ok(_), None) => unreachable!("vet refuses a row without its trigger"),
            };
            // S3: the branch, the trigger and its function as they are now, per row.
            match self.current(branch, &db).await? {
                Some((now, latest, p)) if now == s.timeline => {
                    if !p.deliver.iter().any(|x| x.trigger_id == d.trigger_id) {
                        if p.hold.contains(&d.trigger_id) {
                            settled.push((id, Settle::Release));
                        } else {
                            tally.dropped(d.trigger_id, &latest);
                            settled.push((id, Settle::Delete));
                        }
                        continue;
                    }
                }
                // Gone, not ready, or moved: the rest keep their leases.
                _ => {
                    stop = true;
                    break;
                }
            }
            let outcome = self.invoke(branch, d, &event).await;
            let st = settle(row.attempts, &outcome);
            debug!(branch_id = %branch, database = %db, row = id, ?st, "event settled");
            tally.settled(d.trigger_id, &outcome, st);
            settled.push((id, st));
        }
        self.settle(s, &settled).await?;
        let deliveries: Vec<TriggerDelivery> = tally.0.into_values().collect();
        if !deliveries.is_empty() {
            self.meta.record(branch, &deliveries).await?;
        }
        if stop {
            return Ok(Pass::Stop);
        }
        if full {
            return Ok(Pass::More);
        }
        let next: Option<f64> = s
            .client
            .query_one(NEXT_DUE_SQL, &[&s.timeline, &wanted])
            .await
            .map_err(pg("reading the next due event"))?
            .get(0);
        let until_sweep =
            SWEEP_EVERY.saturating_sub(s.last_sweep.map_or(SWEEP_EVERY, |t| t.elapsed()));
        let idle = next.map_or(until_sweep, |secs| {
            Duration::from_secs_f64(secs.max(0.0)).min(until_sweep)
        });
        Ok(Pass::Idle(idle))
    }

    async fn invoke(&self, branch: BranchId, d: &Deliverable, event: &super::Event) -> Outcome {
        match self
            .invoker
            .deliver(branch, &d.function, event_body(event))
            .await
        {
            Ok(status) if (200..300).contains(&status) => Outcome::Delivered,
            Ok(status) => Outcome::Failed(format!("the function answered {status}")),
            Err(e) => Outcome::Failed(e),
        }
    }

    async fn settle(&self, s: &Session, settled: &[(i64, Settle)]) -> Result<(), String> {
        let deletes: Vec<i64> = settled
            .iter()
            .filter(|(_, st)| *st == Settle::Delete)
            .map(|(id, _)| *id)
            .collect();
        if !deletes.is_empty() {
            s.client
                .execute(DELETE_SQL, &[&deletes])
                .await
                .map_err(pg("settling events"))?;
        }
        let mut rows = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (id, st) in settled {
            let (after, status, back) = match st {
                Settle::Retry(after) => (after.as_secs_f64(), "pending", 0),
                Settle::Dead => (0.0, "dead", 0),
                Settle::Release => (0.0, "pending", 1),
                Settle::Delete => continue,
            };
            rows.0.push(*id);
            rows.1.push(after);
            rows.2.push(status.to_owned());
            rows.3.push(back);
        }
        if !rows.0.is_empty() {
            s.client
                .execute(UPDATE_SQL, &[&rows.0, &rows.1, &rows.2, &rows.3])
                .await
                .map_err(pg("settling events"))?;
        }
        Ok(())
    }

    /// Waits up to `idle` for a notification on the session.
    pub async fn wait(&self, s: &mut Session, idle: Duration) -> Result<Woken, String> {
        let woken = tokio::select! {
            n = s.notifications.recv() => match n {
                Some(_) => Woken::Notified,
                None => return Err("the connection closed".into()),
            },
            _ = tokio::time::sleep(idle) => Woken::TimedOut,
        };
        // One pass serves every notification that came meanwhile.
        while s.notifications.try_recv().is_ok() {}
        Ok(woken)
    }

    /// One target's session: passes, woken by notifications or the sweep, until one says
    /// stop. `Ok` when the target is done.
    async fn session(&self, branch: BranchId, db: &str) -> Result<(), String> {
        let Some(mut s) = self.open(branch, db).await? else {
            return Ok(());
        };
        loop {
            match self.pass(&mut s).await? {
                Pass::Stop => return Ok(()),
                Pass::More => {}
                Pass::Idle(idle) => {
                    self.wait(&mut s, idle).await?;
                }
            }
        }
    }

    /// One target's task: sessions until one ends the target; a failed one waits [`RETRY`].
    pub async fn run_target(self: Arc<Self>, branch: BranchId, db: String) {
        loop {
            match self.session(branch, &db).await {
                Ok(()) => return,
                Err(e) => {
                    warn!(branch_id = %branch, database = %db, "function events: {e}");
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    }

    /// The supervisor: every [`SUPERVISE_EVERY`], a task for each target without one, up to
    /// [`MAX_TARGETS`] at once.
    pub async fn supervise(self: Arc<Self>) {
        let mut tasks: HashMap<(BranchId, String), tokio::task::JoinHandle<()>> = HashMap::new();
        let mut every = tokio::time::interval(SUPERVISE_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            tasks.retain(|_, t| !t.is_finished());
            let targets = match self.meta.targets(MAX_TARGETS as i64).await {
                Ok(targets) => targets,
                Err(e) => {
                    warn!("function events: {e}");
                    continue;
                }
            };
            for target in targets {
                if tasks.len() >= MAX_TARGETS || tasks.contains_key(&target) {
                    continue;
                }
                let (branch, db) = target.clone();
                tasks.insert(target, tokio::spawn(self.clone().run_target(branch, db)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The statements as the login runs them: every name qualified and every operator
    /// `pg_catalog`'s; the bound touches dead rows only (B1); the claim is skip-locked and
    /// scoped to the delivered triggers and the timeline; nothing reads a member table.
    #[test]
    fn events_statements_are_locked_down() {
        let claim = claim_sql();
        for sql in [
            GUARD_SQL,
            CLEANUP_SQL,
            BOUND_SQL,
            DELETE_SQL,
            UPDATE_SQL,
            NEXT_DUE_SQL,
            claim.as_str(),
        ] {
            // `SET col = …` assigns; it is no operator.
            let sql = ["status = ", "attempts = ", "next_attempt_at = "]
                .iter()
                .fold(sql.to_string(), |s, a| s.replace(a, ""));
            let plain_ops = sql
                .split_whitespace()
                .filter(|w| ["=", "<", ">", "<=", ">=", "<>", "+", "-", "->", "||"].contains(w))
                .count();
            assert_eq!(plain_ops, 0, "an unqualified operator in {sql}");
            assert!(!sql.contains("public."), "{sql}");
        }
        assert!(BOUND_SQL.contains("WHERE status OPERATOR(pg_catalog.=) 'dead'"));
        assert!(
            !BOUND_SQL.contains("'pending'"),
            "the bound never deletes a pending row"
        );
        assert!(claim.contains("FOR UPDATE SKIP LOCKED"));
        assert!(claim.contains("o.status OPERATOR(pg_catalog.=) 'pending'"));
        assert!(claim.contains("o.timeline_id OPERATOR(pg_catalog.=) $1::pg_catalog.text"));
        assert!(GUARD_SQL.contains("'chelabase_functions_owner'"));
        assert_eq!(CHANNEL, "chelabase_functions");
    }

    fn record(id: Uuid) -> TriggerRecord {
        TriggerRecord {
            id,
            branch_id: BranchId::new(),
            function_id: Uuid::new_v4(),
            source: chelabase_metadata::TriggerSource::Table,
            enabled: false,
            state: chelabase_metadata::TriggerState::Installed,
            enabled_since: chrono::Utc::now(),
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

    /// B1: what a sweep or a re-check removes is counted for the triggers the metadata still
    /// has (a deleted trigger has no row to count on); dead ones once each.
    #[test]
    fn events_tally_counts_dropped_and_dead() {
        let (a, gone) = (Uuid::new_v4(), Uuid::new_v4());
        let known = [record(a)];
        let mut t = Tally::default();
        t.dropped(a, &known);
        t.dropped(a, &known);
        t.dropped(gone, &known);
        t.settled(
            a,
            &Outcome::Failed("the function answered 500".into()),
            Settle::Dead,
        );
        t.settled(a, &Outcome::Delivered, Settle::Delete);
        let d = &t.0[&a];
        assert_eq!((d.dropped, d.dead, d.delivered), (2, 1, true));
        assert_eq!(d.error.as_deref(), Some("the function answered 500"));
        assert!(!t.0.contains_key(&gone));
    }
}
