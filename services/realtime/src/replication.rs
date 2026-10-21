//! The change source (spec §3.3, §4, §5.1; R2, R8, R16, R17, R19, R22, W7, W8, W10, W12, W21,
//! W25, P9, P14, S4, S5, S16, S25, S26, S29, S30): [`ReplicationSource`] implements T7's
//! [`ChangeSource`].
//!
//! **Watching (every instance):**
//! - `watch` resolves (schema, table) as the viewer's identity (`None` → `not_found`), probes
//!   (a failed probe → `forbidden`, W2), and classifies the relation ([`crate::partition::join_kind`]:
//!   views and friends join with `live: false, reason: "not_a_table"`). A read-only compute joins
//!   with `live: false, reason: "read_only"` and never gets a slot (S26). Otherwise the join
//!   answers the database's current state on this instance (`live: false, reason: "waiting"`
//!   until a slot streams), heartbeats `chelabase.rt.watch.<branch_id>.<db_oid>` every
//!   [`crate::lease::HEARTBEAT`] and tries the lease.
//! - The instance subscribes `chelabase.rt.rows.<branch_id>.<db_oid>.*` for the databases it
//!   watches and filters each relation's windows for its own viewers
//!   ([`crate::filter::TableFilter`]), in order per relation. `.0` carries the database's own
//!   messages (status, resync, schema change); relation OIDs are never 0.
//! - Watchers watch the lease key: a deletion (a release or an expiry's delete marker) → a
//!   takeover attempt once the connection route answers (S25); the new holder's slot sends
//!   `resync` + `status {live: true}` (R17, W12).
//!
//! **Holding (the lease holder of a (branch, database)):**
//! - Takes a slot index (S5), checks the `rt_%` count, creates the slot within
//!   [`crate::slots::CREATE_TIMEOUT`] (`waiting` on timeout, W7; errors per
//!   [`crate::slots::classify`]), and streams. A slot that starts after watchers joined →
//!   `resync` + `status {live: true}` to every watcher (R8).
//! - Decodes, merges (100 ms), fans partitions out to their ancestors (R16, W25), publishes each
//!   relation's window on its `rows` subject, then confirms the window's end LSN; keepalives are
//!   confirmed when idle (W8); the lag watchdog drops and recreates the slot.
//! - A schema change (W10) pauses the database's channels, reprobes and re-resolves before any
//!   key of that window is released: the window is published as one schema message, every
//!   channel re-resolves its table and gets `changed` (its keys are never released).
//! - Publishing failing for [`SourceSettings::publish_failure`] drops the slot (its connection),
//!   and watchers get `status {live: false, reason: "unavailable"}` (R17).
//! - `chelabase.rt.compute.stopping.<branch_id>` (P9): the holder closes the replication
//!   connection at once and releases the lease; watchers get `compute_stopping`; takeover waits
//!   until the connection route answers 200 (S25, backoff 1 s → 30 s), then `resync` + `status
//!   {live: true}`.
//! - Stops [`crate::lease::IDLE_AFTER`] after the last watching heartbeat (S29); its own
//!   instance's watchers count without NATS, and they get its status frames directly (NATS
//!   down still reaches them as `unavailable`).
//! - Re-tells its current state every [`crate::lease::HEARTBEAT`] in every state (streaming,
//!   waiting to retry after `no_slot` or an error), never at or past its fencing deadline, so
//!   every watcher converges to it; a database channel made on the holder's own instance
//!   starts from it. A fenced holder tells only its own instance's watchers `waiting`.
//! - On any window of a database, that database's R12 cache is flushed (every relation's).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chelabase_nats::rt::{COMPUTE_STOPPING, ComputeStopping, ROWS, WATCH};
use chelabase_query::realtime::Relation;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tokio_postgres::types::PgLsn;
use uuid::Uuid;

use crate::ack::Acks;
use crate::bus::Bus;
use crate::channel::TableChannel;
use crate::check::{
    CheckError, CheckRequest, CheckTarget, Checker, ComputeFacts, Limiter, Resolved,
    VisibilityCache,
};
use crate::decoder::{self, Txn};
use crate::filter::{FilterSettings, TableFilter};
use crate::lease::{
    HEARTBEAT, IDLE_AFTER, KvError, KvEvent, KvStore, Lease, RENEW_EVERY, TTL, lease_key,
};
use crate::merge::{Merger, RelWindow, WINDOW, Window};
use crate::partition::{JoinKind, Parent, fan_out, join_kind};
use crate::protocol::StatusReason;
use crate::schema::is_schema_change;
use crate::slots::{
    CREATE_TIMEOUT, Replicator, SlotError, SlotIndex, SlotTarget, WalMessage, WalStream, cap,
    over_cap, retry_delay, take_index,
};
use crate::source::{
    ChangeSource, ChannelEvent, CheckIdentity, JoinInfo, JoinRefusal, WatchHandle,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSettings {
    /// `CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE`.
    pub max_slots_per_compute: u32,
    pub filter: FilterSettings,
    /// R17: publishing failing this long drops the slot.
    pub publish_failure: Duration,
}

impl Default for SourceSettings {
    /// 10 slots, [`FilterSettings::default`], 30 s.
    fn default() -> Self {
        Self {
            max_slots_per_compute: 10,
            filter: FilterSettings::default(),
            publish_failure: Duration::from_secs(30),
        }
    }
}

/// What the source runs on.
pub struct SourceDeps {
    pub instance_id: String,
    pub bus: Arc<dyn Bus>,
    pub leases: Arc<dyn KvStore>,
    pub slots: Arc<dyn KvStore>,
    pub checker: Arc<dyn Checker>,
    pub replicator: Arc<dyn Replicator>,
    pub settings: SourceSettings,
}

/// S25's wait for the connection route: 1 s, doubling, at most 30 s.
const COMPUTE_BACKOFF_FIRST: Duration = Duration::from_secs(1);
const COMPUTE_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How often the holder looks at its idle clock and retries a failed publish.
const TICK: Duration = Duration::from_secs(1);
/// A publish of the database's own status to other instances never holds the holder longer.
const STATUS_PUBLISH_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a `compute.stopping` event answers late joins (`compute_stopping`) on a database
/// no holder has spoken for since (P9).
const STOPPING_REMEMBERED: Duration = Duration::from_secs(300);

/// A (branch, Postgres database) by its OID.
type DbKey = (Uuid, u32);

/// What a watcher shows: `live`, and why not.
type Status = (bool, Option<StatusReason>);

const WAITING: Status = (false, Some(StatusReason::Waiting));

/// The database's own messages on `rows.<branch>.<db>.0`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DbMsg {
    Status {
        holder: String,
        live: bool,
        reason: Option<StatusReason>,
    },
    Resync {
        holder: String,
        reason: String,
    },
    /// W10: a window that changed the schema (or `all`): every channel re-resolves and
    /// refetches.
    Schema {
        holder: String,
    },
}

/// One relation's window on `rows.<branch>.<db>.<rel>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowMsg {
    xids: Vec<u64>,
    changed: bool,
    inserts: Vec<Vec<String>>,
    updates: Vec<Vec<String>>,
    deletes: Vec<Vec<String>>,
}

fn rows_subject(key: DbKey, rel: u32) -> String {
    format!("{ROWS}.{}.{}.{rel}", key.0.hyphenated(), key.1)
}

fn watch_subject(key: DbKey) -> String {
    format!("{WATCH}.{}.{}", key.0.hyphenated(), key.1)
}

/// Why a holder stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// P9: release at once, watchers get `compute_stopping`.
    ComputeStopping,
    /// SIGTERM: release.
    Shutdown,
    /// S29: no watcher for 30 s: release.
    Idle,
    /// W12: past the local deadline: stop, release nothing (the key expires).
    Fenced,
}

/// One watch (a socket's channel).
struct WatchRec {
    /// `None`: a view or a read-only compute (nothing streams).
    db: Option<DbKey>,
    rel: u32,
    channel: TableChannel,
    identity: CheckIdentity,
    sink: mpsc::Sender<ChannelEvent>,
    /// Shared with the watch's viewer in its table's filter: an event dropped on the full sink
    /// (a status, a resync) owes the viewer a resync, as a dropped window does.
    overflow: Arc<AtomicBool>,
    /// What a join answered when nothing streams (not a table, read-only).
    fixed: Option<StatusReason>,
}

enum TableWork {
    Window {
        window: RelWindow,
        xids: Vec<u64>,
        at: Instant,
    },
    Schema,
}

/// One watched relation of a database on this instance.
struct Table {
    filter: Arc<tokio::sync::Mutex<TableFilter>>,
    queue: mpsc::UnboundedSender<TableWork>,
    viewers: HashSet<u64>,
    worker: AbortHandle,
}

/// A database this instance watches.
struct Db {
    target: CheckTarget,
    status: Status,
    tables: HashMap<u32, Table>,
    limiter: Arc<Limiter>,
    cache: Arc<VisibilityCache>,
    tasks: Vec<AbortHandle>,
}

/// A database this instance holds the lease of.
struct HolderRec {
    stop: watch::Sender<Option<Stop>>,
    /// Dropped with the record when the holder ends: its receivers (subscribed while the
    /// record exists) wake, whenever that happens.
    done: watch::Sender<()>,
    task: Option<AbortHandle>,
    /// The holder's current state, once it told one: a database channel made on this
    /// instance while it holds starts from it (a re-join after the last watcher left).
    status: Option<Status>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    watches: HashMap<u64, WatchRec>,
    dbs: HashMap<DbKey, Db>,
    holders: HashMap<DbKey, HolderRec>,
    /// P9: branches whose compute is stopping (since when), with the databases a holder has
    /// told a state for since: until then, a database channel made here starts at
    /// `compute_stopping`.
    stopping: HashMap<Uuid, (Instant, HashSet<u32>)>,
}

impl State {
    /// What a database channel made now starts at: this instance's holder's state, else
    /// `compute_stopping` after the branch's `compute.stopping`, else `waiting` (a holder
    /// elsewhere re-publishes its state every [`HEARTBEAT`]).
    fn seed(&self, key: DbKey) -> Status {
        if let Some(s) = self.holders.get(&key).and_then(|h| h.status) {
            return s;
        }
        match self.stopping.get(&key.0) {
            Some((_, told)) if !told.contains(&key.1) => {
                (false, Some(StatusReason::ComputeStopping))
            }
            _ => WAITING,
        }
    }
}

struct Inner {
    deps: SourceDeps,
    state: Mutex<State>,
    /// Every task the source spawned (aborted by [`ReplicationSource::abort`]).
    tasks: Mutex<Vec<AbortHandle>>,
    /// Shutting down: no lease is taken any more.
    closing: AtomicBool,
    me: Weak<Inner>,
}

pub struct ReplicationSource {
    inner: Arc<Inner>,
}

impl ReplicationSource {
    /// Starts the source's background work (the `compute.stopping` subscription included).
    pub async fn start(deps: SourceDeps) -> anyhow::Result<Arc<Self>> {
        let stopping = deps
            .bus
            .subscribe(&format!("{COMPUTE_STOPPING}.*"))
            .await
            .map_err(|e| anyhow::anyhow!("subscribing {COMPUTE_STOPPING}.*: {e}"))?;
        let inner = Arc::new_cyclic(|me| Inner {
            deps,
            state: Mutex::new(State::default()),
            tasks: Mutex::new(Vec::new()),
            closing: AtomicBool::new(false),
            me: me.clone(),
        });
        let i = inner.clone();
        inner.spawn(async move { i.compute_stopping(stopping).await });
        Ok(Arc::new(Self { inner }))
    }
    /// SIGTERM: closes every replication connection and releases every lease and slot index at
    /// its own revision.
    pub async fn shutdown(&self) {
        self.inner.closing.store(true, Ordering::SeqCst);
        let dones: Vec<watch::Receiver<()>> = {
            let state = self.inner.state.lock().unwrap();
            // No takeover once the releases below delete the keys.
            for db in state.dbs.values() {
                for t in &db.tasks {
                    t.abort();
                }
            }
            state
                .holders
                .values()
                .map(|h| {
                    h.stop.send_replace(Some(Stop::Shutdown));
                    h.done.subscribe()
                })
                .collect()
        };
        // One deadline for every holder (never one each, in turn).
        let deadline = Instant::now() + Duration::from_secs(5);
        for mut done in dones {
            let _ = tokio::time::timeout_at(deadline, done.changed()).await;
        }
        self.abort();
    }
    /// As a crash: stops every task at once, releasing nothing.
    pub fn abort(&self) {
        let tasks: Vec<AbortHandle> = std::mem::take(&mut *self.inner.tasks.lock().unwrap());
        for t in tasks {
            t.abort();
        }
        let mut state = self.inner.state.lock().unwrap();
        for (_, db) in state.dbs.drain() {
            for t in db.tasks {
                t.abort();
            }
            for (_, table) in db.tables {
                table.worker.abort();
            }
        }
        for (_, h) in state.holders.drain() {
            if let Some(t) = h.task {
                t.abort();
            }
        }
    }
}

impl Inner {
    fn arc(&self) -> Arc<Inner> {
        self.me
            .upgrade()
            .expect("the source is alive while it runs")
    }

    fn spawn(&self, f: impl std::future::Future<Output = ()> + Send + 'static) -> AbortHandle {
        let handle = tokio::spawn(f).abort_handle();
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|t| !t.is_finished());
        tasks.push(handle.clone());
        handle
    }

    // --- watchers' side ---------------------------------------------------------------

    /// Sends `ev` to every watcher of `key` on this instance (never awaited) and, for a status,
    /// records it as the database's state.
    fn broadcast(&self, key: DbKey, ev: ChannelEvent) {
        let mut state = self.state.lock().unwrap();
        let State {
            watches,
            dbs,
            stopping,
            ..
        } = &mut *state;
        if let ChannelEvent::Status { reason, .. } = &ev
            && *reason != Some(StatusReason::ComputeStopping)
            && let Some((_, told)) = stopping.get_mut(&key.0)
        {
            // A holder spoke for the database since its compute stopped.
            told.insert(key.1);
        }
        let Some(db) = dbs.get_mut(&key) else {
            return;
        };
        if let ChannelEvent::Status { live, reason } = &ev {
            if db.status == (*live, *reason) {
                return;
            }
            db.status = (*live, *reason);
        }
        for w in watches.values().filter(|w| w.db == Some(key)) {
            if let Err(mpsc::error::TrySendError::Full(_)) = w.sink.try_send(ev.clone()) {
                w.overflow.store(true, Ordering::SeqCst);
            }
        }
    }

    fn status_of(&self, key: DbKey) -> Status {
        let state = self.state.lock().unwrap();
        state
            .dbs
            .get(&key)
            .map_or_else(|| state.seed(key), |d| d.status)
    }

    /// The database's channel, made on its first watcher: its rows subscription, heartbeats
    /// and lease attempts.
    fn ensure_db(&self, key: DbKey, target: &CheckTarget) {
        let mut state = self.state.lock().unwrap();
        if state.dbs.contains_key(&key) {
            return;
        }
        let me = self.arc();
        let t1 = {
            let me = me.clone();
            self.spawn(async move { me.dispatch(key).await })
        };
        let t2 = {
            let me = me.clone();
            self.spawn(async move { me.heartbeat(key).await })
        };
        let t3 = {
            let me = me.clone();
            let target = target.clone();
            self.spawn(async move { me.lease_task(key, target).await })
        };
        let status = state.seed(key);
        state.dbs.insert(
            key,
            Db {
                target: target.clone(),
                status,
                tables: HashMap::new(),
                limiter: Limiter::new(self.deps.settings.filter.concurrency),
                cache: VisibilityCache::new(WINDOW),
                tasks: vec![t1, t2, t3],
            },
        );
    }

    /// The relation's table on `key` (made on its first watcher).
    fn ensure_table(
        &self,
        key: DbKey,
        resolved: &Resolved,
    ) -> Option<(
        Arc<tokio::sync::Mutex<TableFilter>>,
        mpsc::UnboundedSender<TableWork>,
    )> {
        let mut state = self.state.lock().unwrap();
        let db = state.dbs.get_mut(&key)?;
        let oid = resolved.relation.oid;
        if let Some(t) = db.tables.get(&oid) {
            return Some((t.filter.clone(), t.queue.clone()));
        }
        let mut filter = TableFilter::new(
            db.target.clone(),
            resolved.relation.clone(),
            self.deps.checker.clone(),
            db.limiter.clone(),
            db.cache.clone(),
            self.deps.settings.filter.clone(),
        );
        filter.set_changed_only(
            join_kind(resolved.relkind, resolved.inheritance_parent) == JoinKind::Changed,
        );
        let filter = Arc::new(tokio::sync::Mutex::new(filter));
        let (queue, rx) = mpsc::unbounded_channel();
        let worker = {
            let me = self.arc();
            let filter = filter.clone();
            self.spawn(async move { me.table_worker(key, filter, rx).await })
        };
        db.tables.insert(
            oid,
            Table {
                filter: filter.clone(),
                queue: queue.clone(),
                viewers: HashSet::new(),
                worker,
            },
        );
        Some((filter, queue))
    }

    /// Drops a watch (its handle went): out of its table, the table and the database when they
    /// have no watcher left (a holder keeps running until its idle clock says otherwise).
    fn unwatch(&self, id: u64) {
        let mut state = self.state.lock().unwrap();
        let Some(rec) = state.watches.remove(&id) else {
            return;
        };
        let Some(key) = rec.db else {
            return;
        };
        let Some(db) = state.dbs.get_mut(&key) else {
            return;
        };
        if let Some(table) = db.tables.get_mut(&rec.rel) {
            table.viewers.remove(&id);
            if table.viewers.is_empty() {
                let table = db.tables.remove(&rec.rel).expect("present");
                table.worker.abort();
            } else {
                let filter = table.filter.clone();
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(async move { filter.lock().await.remove(id) });
                }
            }
        }
        if db.tables.is_empty() {
            let db = state.dbs.remove(&key).expect("present");
            for t in db.tasks {
                t.abort();
            }
        }
    }

    /// The database's messages: each relation's windows to its table, in order; its status,
    /// resyncs and schema changes to every channel.
    async fn dispatch(self: Arc<Self>, key: DbKey) {
        let pattern = format!("{ROWS}.{}.{}.*", key.0.hyphenated(), key.1);
        let mut sub = loop {
            match self.deps.bus.subscribe(&pattern).await {
                Ok(s) => break s,
                Err(e) => {
                    tracing::warn!(error = %e, "subscribing a database's rows failed; retrying");
                    tokio::time::sleep(TICK).await;
                }
            }
        };
        while let Some(m) = sub.next().await {
            let Some(rel) = m
                .subject
                .rsplit('.')
                .next()
                .and_then(|r| r.parse::<u32>().ok())
            else {
                continue;
            };
            // R12: any window of the database (a policy may read any of its tables).
            let cache = self
                .state
                .lock()
                .unwrap()
                .dbs
                .get(&key)
                .map(|d| d.cache.clone());
            if let Some(c) = cache {
                c.flush();
            }
            if rel == 0 {
                let Ok(msg) = serde_json::from_slice::<DbMsg>(&m.payload) else {
                    continue;
                };
                match msg {
                    DbMsg::Status {
                        holder,
                        live,
                        reason,
                    } if holder != self.deps.instance_id => {
                        self.broadcast(key, ChannelEvent::Status { live, reason });
                    }
                    DbMsg::Resync { holder, reason } if holder != self.deps.instance_id => {
                        self.broadcast(key, ChannelEvent::Resync { reason });
                    }
                    DbMsg::Schema { .. } => {
                        let queues: Vec<mpsc::UnboundedSender<TableWork>> = self
                            .state
                            .lock()
                            .unwrap()
                            .dbs
                            .get(&key)
                            .map(|d| d.tables.values().map(|t| t.queue.clone()).collect())
                            .unwrap_or_default();
                        for q in queues {
                            let _ = q.send(TableWork::Schema);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let Ok(w) = serde_json::from_slice::<WindowMsg>(&m.payload) else {
                continue;
            };
            let queue = self
                .state
                .lock()
                .unwrap()
                .dbs
                .get(&key)
                .and_then(|d| d.tables.get(&rel))
                .map(|t| t.queue.clone());
            if let Some(q) = queue {
                let _ = q.send(TableWork::Window {
                    window: RelWindow {
                        rel,
                        changed: w.changed,
                        inserts: w.inserts,
                        updates: w.updates,
                        deletes: w.deletes,
                    },
                    xids: w.xids,
                    at: Instant::now(),
                });
            }
        }
    }

    /// One table's windows, in order (S30).
    async fn table_worker(
        self: Arc<Self>,
        key: DbKey,
        filter: Arc<tokio::sync::Mutex<TableFilter>>,
        mut rx: mpsc::UnboundedReceiver<TableWork>,
    ) {
        while let Some(work) = rx.recv().await {
            let mut f = filter.lock().await;
            match work {
                TableWork::Window { window, xids, at } => {
                    f.deliver(&window, &xids, at).await;
                }
                TableWork::Schema => {
                    let old = f.relation().oid;
                    drop(f);
                    self.rebind(key, old).await;
                    let mut f = filter.lock().await;
                    let rel = f.relation().oid;
                    let changed = RelWindow {
                        rel,
                        changed: true,
                        ..RelWindow::default()
                    };
                    f.deliver(&changed, &[], Instant::now()).await;
                }
            }
        }
    }

    /// Re-resolves the table now keyed `old` on `key` as one of its viewers (W10, S16): a new
    /// OID (a recreated or renamed table) moves the table and its watches to it. `Some` with
    /// the resolution (`None` inside: the table is gone), `None` when it couldn't be asked.
    async fn rebind(&self, key: DbKey, old: u32) -> Option<Option<Resolved>> {
        let (target, channel, identity) = {
            let state = self.state.lock().unwrap();
            let db = state.dbs.get(&key)?;
            let table = db.tables.get(&old)?;
            let viewer = table.viewers.iter().next()?;
            let rec = state.watches.get(viewer)?;
            (db.target.clone(), rec.channel.clone(), rec.identity.clone())
        };
        let resolved = self
            .deps
            .checker
            .resolve(&target, &identity, &channel.schema, &channel.table)
            .await
            .ok()?;
        if let Some(r) = &resolved {
            self.move_table(key, old, r).await;
        }
        Some(resolved)
    }

    /// The table keyed `old` follows `resolved` (its OID and key).
    async fn move_table(&self, key: DbKey, old: u32, resolved: &Resolved) {
        let new = resolved.relation.oid;
        let filter = {
            let mut state = self.state.lock().unwrap();
            let State { watches, dbs, .. } = &mut *state;
            let Some(db) = dbs.get_mut(&key) else {
                return;
            };
            if new != old {
                if db.tables.contains_key(&new) {
                    // Another channel already follows that relation: leave both as they are.
                    return;
                }
                let Some(table) = db.tables.remove(&old) else {
                    return;
                };
                for id in &table.viewers {
                    if let Some(w) = watches.get_mut(id) {
                        w.rel = new;
                    }
                }
                let filter = table.filter.clone();
                db.tables.insert(new, table);
                filter
            } else {
                match db.tables.get(&old) {
                    Some(t) => t.filter.clone(),
                    None => return,
                }
            }
        };
        let mut f = filter.lock().await;
        f.set_relation(resolved.relation.clone());
        f.set_changed_only(
            join_kind(resolved.relkind, resolved.inheritance_parent) == JoinKind::Changed,
        );
    }

    /// `chelabase.rt.watch.<branch>.<db>` every [`HEARTBEAT`] while this instance watches.
    async fn heartbeat(self: Arc<Self>, key: DbKey) {
        let subject = watch_subject(key);
        let payload = Bytes::from(self.deps.instance_id.clone());
        let mut tick = tokio::time::interval(HEARTBEAT);
        loop {
            tick.tick().await;
            let _ = self.deps.bus.publish(&subject, None, payload.clone()).await;
        }
    }

    /// S25: until the connection route answers (backoff 1 s → 30 s). `None`: a read-only
    /// compute (never a slot).
    async fn wait_for_compute(&self, target: &CheckTarget) -> Option<ComputeFacts> {
        let mut backoff = COMPUTE_BACKOFF_FIRST;
        loop {
            match self.deps.checker.compute(target).await {
                Ok(f) if f.read_only => return None,
                Ok(f) => return Some(f),
                Err(_) => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(COMPUTE_BACKOFF_MAX);
                }
            }
        }
    }

    /// Takes the lease whenever it's free (and the compute answers), holds it, and tries again
    /// after the holder stops; a deletion of the key (a release, an expiry's marker) wakes it.
    async fn lease_task(self: Arc<Self>, key: DbKey, target: CheckTarget) {
        let lkey = lease_key(key.0, key.1);
        let mut kv_watch = self.deps.leases.watch(&lkey).await.ok();
        loop {
            // A holder of ours: wait for it to stop.
            let done = {
                let state = self.state.lock().unwrap();
                state.holders.get(&key).map(|h| h.done.subscribe())
            };
            if let Some(mut done) = done {
                let _ = done.changed().await;
                continue;
            }
            if self.wait_for_compute(&target).await.is_none() {
                self.broadcast(
                    key,
                    ChannelEvent::Status {
                        live: false,
                        reason: Some(StatusReason::ReadOnly),
                    },
                );
                return;
            }
            if self.closing.load(Ordering::SeqCst) {
                return;
            }
            match Lease::acquire(self.deps.leases.as_ref(), &lkey, &self.deps.instance_id).await {
                Ok(Some(lease)) => {
                    self.start_holder(key, target.clone(), lease);
                }
                Ok(None) => {
                    // Held elsewhere: until its key is deleted (or a TTL, in case a marker was
                    // missed).
                    let deleted = async {
                        match kv_watch.as_mut() {
                            Some(w) => {
                                while let Some(ev) = w.next().await {
                                    if matches!(ev, KvEvent::Deleted { .. }) {
                                        return true;
                                    }
                                }
                                false
                            }
                            None => std::future::pending().await,
                        }
                    };
                    match tokio::time::timeout(TTL, deleted).await {
                        Ok(true) | Err(_) => {}
                        Ok(false) => kv_watch = self.deps.leases.watch(&lkey).await.ok(),
                    }
                    if kv_watch.is_none() {
                        kv_watch = self.deps.leases.watch(&lkey).await.ok();
                    }
                }
                Err(_) => tokio::time::sleep(TICK).await,
            }
        }
    }

    // --- holding ----------------------------------------------------------------------

    fn start_holder(&self, key: DbKey, target: CheckTarget, lease: Lease) {
        let (stop_tx, stop_rx) = watch::channel(None);
        let mut state = self.state.lock().unwrap();
        state.holders.insert(
            key,
            HolderRec {
                stop: stop_tx,
                done: watch::channel(()).0,
                task: None,
                status: None,
            },
        );
        let me = self.arc();
        let task = self.spawn(async move {
            me.hold(key, target, lease, stop_rx).await;
        });
        if let Some(h) = state.holders.get_mut(&key) {
            h.task = Some(task);
        }
    }

    /// The holder sends a status (and resyncs) to every watcher: this instance's directly, the
    /// others' over NATS, and records a status as its current state. Nothing at or past the
    /// local fencing deadline (W12): another holder may stream by now.
    async fn tell(&self, key: DbKey, ev: ChannelEvent, deadline: &watch::Receiver<Instant>) {
        if Instant::now() >= *deadline.borrow() {
            return;
        }
        if let ChannelEvent::Status { live, reason } = &ev
            && let Some(h) = self.state.lock().unwrap().holders.get_mut(&key)
        {
            h.status = Some((*live, *reason));
        }
        let msg = match &ev {
            ChannelEvent::Status { live, reason } => DbMsg::Status {
                holder: self.deps.instance_id.clone(),
                live: *live,
                reason: *reason,
            },
            ChannelEvent::Resync { reason } => DbMsg::Resync {
                holder: self.deps.instance_id.clone(),
                reason: reason.clone(),
            },
            ChannelEvent::Rows(_) => return,
        };
        self.broadcast(key, ev);
        let payload = Bytes::from(serde_json::to_vec(&msg).expect("serializes"));
        let subject = rows_subject(key, 0);
        tokio::select! {
            biased;
            _ = fenced(deadline.clone()) => {}
            _ = tokio::time::timeout(
                STATUS_PUBLISH_TIMEOUT,
                self.deps.bus.publish(&subject, None, payload),
            ) => {}
        }
    }

    /// The holder re-tells its current state every [`HEARTBEAT`], whatever it is (streaming,
    /// retrying after `no_slot` or an error, creating a slot): late joiners on other
    /// instances learn it, and every watcher converges to it. `lock` orders it with the
    /// holder's own transitions (a re-told state never lands after a newer one); nothing goes
    /// out at or past the fencing deadline.
    async fn status_heartbeat(
        self: Arc<Self>,
        key: DbKey,
        lock: Arc<tokio::sync::Mutex<()>>,
        deadline: watch::Receiver<Instant>,
    ) {
        let mut tick = tokio::time::interval(HEARTBEAT);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let _told = lock.lock().await;
            if Instant::now() >= *deadline.borrow() {
                return;
            }
            let status = self
                .state
                .lock()
                .unwrap()
                .holders
                .get(&key)
                .and_then(|h| h.status);
            if let Some((live, reason)) = status {
                self.tell(key, ChannelEvent::Status { live, reason }, &deadline)
                    .await;
            }
        }
    }

    /// The holder of `key`: slots, streaming, publishing, until told to stop or fenced.
    async fn hold(
        self: Arc<Self>,
        key: DbKey,
        target: CheckTarget,
        lease: Lease,
        mut stop: watch::Receiver<Option<Stop>>,
    ) {
        let (deadline_tx, deadline_rx) = watch::channel(lease.deadline());
        let lease = Arc::new(tokio::sync::Mutex::new(Some(lease)));
        let index: Arc<tokio::sync::Mutex<Option<SlotIndex>>> = Arc::default();
        let renewal = {
            let me = self.clone();
            let lease = lease.clone();
            let index = index.clone();
            self.spawn(async move { me.renew(lease, index, deadline_tx).await })
        };
        let last_seen = Arc::new(Mutex::new(Instant::now()));
        let heard = {
            let me = self.clone();
            let last_seen = last_seen.clone();
            self.spawn(async move {
                let Ok(mut beats) = me.deps.bus.subscribe(&watch_subject(key)).await else {
                    return;
                };
                while beats.next().await.is_some() {
                    *last_seen.lock().unwrap() = Instant::now();
                }
            })
        };
        let told = Arc::new(tokio::sync::Mutex::new(()));
        let beat = {
            let me = self.clone();
            let told = told.clone();
            let deadline = deadline_rx.clone();
            self.spawn(async move { me.status_heartbeat(key, told, deadline).await })
        };
        let mut h = Holding {
            src: self.clone(),
            key,
            target: target.clone(),
            stop: &mut stop,
            deadline: deadline_rx,
            last_seen,
            index: index.clone(),
            told,
        };
        let why = h.run().await;
        beat.abort();
        match why {
            // P9, whatever the holder was doing (streaming, or waiting to retry).
            Stop::ComputeStopping => {
                h.status(false, Some(StatusReason::ComputeStopping)).await;
            }
            // W12: another holder may stream by now. This instance's watchers stop showing
            // live; nothing goes out over NATS (the new holder tells the others).
            Stop::Fenced => {
                if let Some(rec) = self.state.lock().unwrap().holders.get_mut(&key) {
                    rec.status = Some(WAITING);
                }
                self.broadcast(
                    key,
                    ChannelEvent::Status {
                        live: false,
                        reason: Some(StatusReason::Waiting),
                    },
                );
            }
            Stop::Idle | Stop::Shutdown => {}
        }
        renewal.abort();
        heard.abort();
        // The slot (its connection) is gone by now; the index and the lease go with it, at
        // their own revisions, unless fenced (someone else may hold them by now).
        if why != Stop::Fenced {
            if let Some(i) = index.lock().await.take() {
                let _ = i.release(self.deps.slots.as_ref()).await;
            }
            if let Some(l) = lease.lock().await.take() {
                let _ = l.release(self.deps.leases.as_ref()).await;
            }
        }
        // Dropping the record wakes whoever waits for this holder to end.
        let rec = self.state.lock().unwrap().holders.remove(&key);
        drop(rec);
    }

    /// Renews the lease (and the slot index with it) every [`RENEW_EVERY`]; a renewal at a
    /// revision that isn't the key's fences the holder at once.
    async fn renew(
        self: Arc<Self>,
        lease: Arc<tokio::sync::Mutex<Option<Lease>>>,
        index: Arc<tokio::sync::Mutex<Option<SlotIndex>>>,
        deadline: watch::Sender<Instant>,
    ) {
        loop {
            tokio::time::sleep(RENEW_EVERY).await;
            {
                let mut l = lease.lock().await;
                let Some(l) = l.as_mut() else {
                    return;
                };
                match l.renew(self.deps.leases.as_ref()).await {
                    Ok(()) => {
                        deadline.send_replace(l.deadline());
                    }
                    Err(KvError::WrongRevision) => {
                        deadline.send_replace(Instant::now());
                        return;
                    }
                    Err(_) => {}
                }
            }
            let mut i = index.lock().await;
            if let Some(idx) = i.as_mut()
                && idx.renew(self.deps.slots.as_ref()).await == Err(KvError::WrongRevision)
            {
                // Lost (expired and taken): this slot no longer counts against the cap.
                *i = None;
            }
        }
    }

    // --- compute stopping ---------------------------------------------------------------

    async fn compute_stopping(self: Arc<Self>, mut sub: crate::bus::BusStream) {
        while let Some(m) = sub.next().await {
            let Ok(ev) = serde_json::from_slice::<ComputeStopping>(&m.payload) else {
                continue;
            };
            // The payload's branch must be the subject's.
            if m.subject.rsplit('.').next() != Some(ev.branch_id.hyphenated().to_string().as_str())
            {
                continue;
            }
            let mut state = self.state.lock().unwrap();
            // Late joins answer `compute_stopping` until a holder speaks for their database.
            state
                .stopping
                .retain(|_, (at, _)| at.elapsed() < STOPPING_REMEMBERED);
            state
                .stopping
                .insert(ev.branch_id, (Instant::now(), HashSet::new()));
            for (key, h) in &state.holders {
                if key.0 == ev.branch_id {
                    h.stop.send_replace(Some(Stop::ComputeStopping));
                }
            }
        }
    }
}

/// One window's messages: (subject, payload), published in order.
type Outgoing = VecDeque<(String, Bytes)>;

/// Why a slot ended.
enum SlotEnd {
    Stopped(Stop),
    /// The walsender went (a compute restart) or the stream failed.
    Ended,
    /// The lag watchdog (W8): a fresh slot at once.
    Lagging,
    /// R17: publishing failed too long.
    PublishFailed,
}

/// One holder's run.
struct Holding<'a> {
    src: Arc<Inner>,
    key: DbKey,
    target: CheckTarget,
    stop: &'a mut watch::Receiver<Option<Stop>>,
    deadline: watch::Receiver<Instant>,
    last_seen: Arc<Mutex<Instant>>,
    index: Arc<tokio::sync::Mutex<Option<SlotIndex>>>,
    /// Held while the holder tells something (shared with its status heartbeat).
    told: Arc<tokio::sync::Mutex<()>>,
}

impl Holding<'_> {
    /// A stop that is due: asked for, fenced, or idle.
    fn due(&mut self) -> Option<Stop> {
        if let Some(s) = *self.stop.borrow() {
            return Some(s);
        }
        if Instant::now() >= *self.deadline.borrow() {
            return Some(Stop::Fenced);
        }
        // This instance's own watchers count without NATS.
        if self.src.state.lock().unwrap().dbs.contains_key(&self.key) {
            *self.last_seen.lock().unwrap() = Instant::now();
        }
        if self.last_seen.lock().unwrap().elapsed() >= IDLE_AFTER {
            return Some(Stop::Idle);
        }
        None
    }

    /// Sleeps `d`, or less when a stop comes due.
    async fn wait(&mut self, d: Duration) -> Option<Stop> {
        let until = Instant::now() + d;
        loop {
            if let Some(s) = self.due() {
                return Some(s);
            }
            let now = Instant::now();
            if now >= until {
                return None;
            }
            let next = (now + TICK).min(until).min(*self.deadline.borrow());
            tokio::select! {
                _ = self.stop.changed() => {}
                _ = tokio::time::sleep_until(next) => {}
            }
        }
    }

    async fn tell(&self, ev: ChannelEvent) {
        let _told = self.told.lock().await;
        self.src.tell(self.key, ev, &self.deadline).await;
    }

    async fn status(&self, live: bool, reason: Option<StatusReason>) {
        self.tell(ChannelEvent::Status { live, reason }).await;
    }

    async fn give_back_index(&self) {
        if let Some(i) = self.index.lock().await.take() {
            let _ = i.release(self.src.deps.slots.as_ref()).await;
        }
    }

    async fn run(&mut self) -> Stop {
        let src = self.src.clone();
        let deps = &src.deps;
        let mut attempt: u32 = 0;
        let mut compute_backoff = COMPUTE_BACKOFF_FIRST;
        loop {
            if let Some(s) = self.due() {
                return s;
            }
            let facts = match deps.checker.compute(&self.target).await {
                Ok(f) => {
                    compute_backoff = COMPUTE_BACKOFF_FIRST;
                    f
                }
                Err(_) => {
                    self.status(false, Some(StatusReason::Unavailable)).await;
                    let d = compute_backoff;
                    compute_backoff = (compute_backoff * 2).min(COMPUTE_BACKOFF_MAX);
                    if let Some(s) = self.wait(d).await {
                        return s;
                    }
                    continue;
                }
            };
            if facts.read_only {
                self.status(false, Some(StatusReason::ReadOnly)).await;
                return Stop::Idle;
            }
            let cap = cap(
                deps.settings.max_slots_per_compute,
                facts.max_replication_slots,
            );
            let failed = |e: SlotError| e;
            let outcome: Result<Box<dyn WalStream>, SlotError> = async {
                let taken = take_index(deps.slots.as_ref(), self.key.0, cap, &deps.instance_id)
                    .await
                    .map_err(|e| SlotError::Unavailable(e.to_string()))?;
                let Some(index) = taken else {
                    return Err(SlotError::NoSlot);
                };
                *self.index.lock().await = Some(index);
                // S5's second check, read after the index is taken: another instance may have
                // made a slot meanwhile.
                let recount = deps
                    .checker
                    .compute(&self.target)
                    .await
                    .map_err(|e| SlotError::Unavailable(e.to_string()))?;
                if over_cap(recount.rt_slots, cap) {
                    return Err(SlotError::NoSlot);
                }
                let slot = decoder::new_slot_name();
                let slot_target = SlotTarget {
                    branch_id: self.key.0,
                    pg_database: self.target.pg_database.clone(),
                    db_oid: self.key.1,
                };
                let start = deps.replicator.start(&slot_target, &slot);
                let mut stop = self.stop.clone();
                tokio::select! {
                    r = tokio::time::timeout(CREATE_TIMEOUT, start) => match r {
                        Ok(r) => r,
                        Err(_) => Err(SlotError::Waiting),
                    },
                    _ = stop.wait_for(Option::is_some) => Err(SlotError::Unavailable("stopping".into())),
                    _ = fenced(self.deadline.clone()) => Err(SlotError::Unavailable("fenced".into())),
                }
            }
            .await
            .map_err(failed);
            let stream = match outcome {
                Ok(s) => s,
                Err(e) => {
                    self.give_back_index().await;
                    if let Some(s) = self.due() {
                        return s;
                    }
                    self.status(false, Some(e.reason())).await;
                    attempt += 1;
                    if let Some(s) = self.wait(retry_delay(attempt)).await {
                        return s;
                    }
                    continue;
                }
            };
            attempt = 0;
            // R8: the slot started after the watchers joined: what they show may be stale.
            self.tell(ChannelEvent::Resync {
                reason: "slot".into(),
            })
            .await;
            self.status(true, None).await;
            let end = self.stream(stream, facts.version_oid).await;
            self.give_back_index().await;
            match end {
                // `hold` tells a stop's status.
                SlotEnd::Stopped(s) => return s,
                SlotEnd::Lagging => continue,
                SlotEnd::Ended => {
                    self.status(false, Some(StatusReason::Waiting)).await;
                }
                SlotEnd::PublishFailed => {
                    self.status(false, Some(StatusReason::Unavailable)).await;
                    attempt += 1;
                    if let Some(s) = self.wait(retry_delay(attempt)).await {
                        return s;
                    }
                }
            }
        }
    }

    /// Streams one slot until it ends; dropping `stream` (on return) closes its connection.
    async fn stream(
        &mut self,
        mut stream: Box<dyn WalStream>,
        version_oid: Option<u32>,
    ) -> SlotEnd {
        let src = self.src.clone();
        let deps = &src.deps;
        let mut acks = Acks::new(PgLsn::from(0));
        let mut sent = PgLsn::from(0);
        let mut merger = Merger::new(deps.settings.filter.max_keys);
        let mut parents: HashMap<u32, Vec<Parent>> = HashMap::new();
        // Windows waiting to be published, in order: their messages and their end LSN.
        let mut pending: VecDeque<(Outgoing, Option<PgLsn>)> = VecDeque::new();
        let mut failing_since: Option<Instant> = None;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stop = self.stop.clone();
        let fence = fenced(self.deadline.clone());
        tokio::pin!(fence);
        loop {
            let due = merger.due_at();
            tokio::select! {
                biased;
                _ = stop.changed() => {
                    if let Some(s) = *stop.borrow() {
                        return SlotEnd::Stopped(s);
                    }
                }
                _ = &mut fence => return SlotEnd::Stopped(Stop::Fenced),
                _ = tick.tick() => {
                    if let Some(s) = self.due() {
                        return SlotEnd::Stopped(s);
                    }
                    // The index was lost (expired and taken elsewhere): this slot no longer
                    // counts against the cap, so it ends and a fresh one takes an index.
                    if self.index.lock().await.is_none() {
                        return SlotEnd::Ended;
                    }
                }
                _ = async { tokio::time::sleep_until(due.expect("guarded")).await }, if due.is_some() => {
                    if let Some(w) = merger.take() {
                        match self.messages(&w, version_oid, &mut parents).await {
                            Some(msgs) => pending.push_back((msgs, w.end_lsn)),
                            None => return SlotEnd::Ended,
                        }
                    }
                }
                msg = stream.next() => match msg {
                    None | Some(Err(_)) => return SlotEnd::Ended,
                    Some(Ok(WalMessage::Data { wal_end, data })) => {
                        let mut txn: Txn = decoder::parse(&data);
                        // An unreadable message has no LSN of its own: it holds confirmation
                        // back at the walsender's position for it until its window is out.
                        if txn.lsn.is_none() {
                            txn.lsn = Some(wal_end);
                        }
                        let now = Instant::now();
                        if let Some(lsn) = txn.lsn {
                            acks.received(lsn, now);
                        }
                        merger.push(now, &txn);
                    }
                    Some(Ok(WalMessage::Keepalive { wal_end, reply })) => {
                        acks.keepalive(wal_end);
                        if acks.lagging(wal_end, Instant::now()) {
                            return SlotEnd::Lagging;
                        }
                        if reply || acks.confirmed() > sent {
                            sent = acks.confirmed();
                            if stream.confirm(sent).await.is_err() {
                                return SlotEnd::Ended;
                            }
                        }
                    }
                },
            }
            // Publish what's pending, in order; confirm what got out.
            while let Some((msgs, end)) = pending.front_mut() {
                let mut ok = true;
                while let Some((subject, payload)) = msgs.front() {
                    // W12: nothing goes out past the local deadline, even when a publish (or the
                    // window's catalog reads) outlived it: another holder may stream by now.
                    let published = tokio::select! {
                        biased;
                        _ = fenced(self.deadline.clone()) => None,
                        r = deps.bus.publish(subject, None, payload.clone()) => Some(r),
                    };
                    match published {
                        None => return SlotEnd::Stopped(Stop::Fenced),
                        Some(Ok(())) => {
                            msgs.pop_front();
                        }
                        Some(Err(_)) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= deps.settings.publish_failure {
                        return SlotEnd::PublishFailed;
                    }
                    break;
                }
                failing_since = None;
                if let Some(end) = *end {
                    acks.published(end);
                }
                pending.pop_front();
            }
            if acks.confirmed() > sent {
                sent = acks.confirmed();
                if stream.confirm(sent).await.is_err() {
                    return SlotEnd::Ended;
                }
            }
        }
    }

    /// A closed window's messages: one schema message (W10), or each fanned-out relation's
    /// window. `None` when a relation's parents can't be read (the slot is restarted: a
    /// window must never go out without its ancestors).
    async fn messages(
        &self,
        w: &Window,
        version_oid: Option<u32>,
        parents: &mut HashMap<u32, Vec<Parent>>,
    ) -> Option<Outgoing> {
        let deps = &self.src.deps;
        let mut out = VecDeque::new();
        if is_schema_change(w, version_oid) {
            parents.clear();
            let msg = DbMsg::Schema {
                holder: deps.instance_id.clone(),
            };
            out.push_back((
                rows_subject(self.key, 0),
                Bytes::from(serde_json::to_vec(&msg).expect("serializes")),
            ));
            return Some(out);
        }
        for r in &w.rels {
            if let std::collections::hash_map::Entry::Vacant(e) = parents.entry(r.rel) {
                e.insert(deps.checker.parents(&self.target, r.rel).await.ok()?);
            }
        }
        for r in fan_out(w, parents, deps.settings.filter.max_keys) {
            let msg = WindowMsg {
                xids: w.xids.clone(),
                changed: r.changed,
                inserts: r.inserts,
                updates: r.updates,
                deletes: r.deletes,
            };
            out.push_back((
                rows_subject(self.key, r.rel),
                Bytes::from(serde_json::to_vec(&msg).expect("serializes")),
            ));
        }
        Some(out)
    }
}

/// Resolves once the local fencing deadline (moved by every renewal) has passed (W12).
async fn fenced(mut deadline: watch::Receiver<Instant>) {
    loop {
        let at = *deadline.borrow_and_update();
        tokio::select! {
            _ = tokio::time::sleep_until(at) => {
                if Instant::now() >= *deadline.borrow() {
                    return;
                }
            }
            changed = deadline.changed() => {
                if changed.is_err() {
                    // The renewals stopped: the last deadline stands.
                    let at = *deadline.borrow();
                    tokio::time::sleep_until(at).await;
                    return;
                }
            }
        }
    }
}

fn refusal(e: &CheckError) -> JoinRefusal {
    match e {
        CheckError::OwnerLevel | CheckError::Failed(_) => JoinRefusal::Forbidden,
        CheckError::Timeout | CheckError::Unavailable(_) => JoinRefusal::Unavailable,
    }
}

fn pk_of(rel: &Relation) -> Option<Vec<String>> {
    (!rel.key.is_empty()).then(|| rel.key.iter().map(|c| c.name.clone()).collect())
}

impl Inner {
    /// Resolution and the probe as `id` (the join's, and a reprobe's).
    async fn probe(
        &self,
        target: &CheckTarget,
        id: &CheckIdentity,
        ch: &TableChannel,
    ) -> Result<(ComputeFacts, Resolved, bool), JoinRefusal> {
        let checker = &self.deps.checker;
        let facts = checker
            .compute(target)
            .await
            .map_err(|_| JoinRefusal::Unavailable)?;
        let resolved = checker
            .resolve(target, id, &ch.schema, &ch.table)
            .await
            .map_err(|e| refusal(&e))?
            .ok_or(JoinRefusal::NotFound)?;
        let answer = checker
            .check(
                target,
                id,
                &CheckRequest {
                    rel_oid: resolved.relation.oid,
                    xids: Vec::new(),
                    keys: None,
                    convert: Vec::new(),
                },
            )
            .await
            .map_err(|e| refusal(&e))?;
        if !answer.probe_ok {
            return Err(JoinRefusal::Forbidden);
        }
        Ok((facts, resolved, answer.bypass))
    }

    /// The filter of watch `id`'s table (`None`: nothing streams for it).
    fn filter_of(&self, id: u64) -> Option<Arc<tokio::sync::Mutex<TableFilter>>> {
        let state = self.state.lock().unwrap();
        let w = state.watches.get(&id)?;
        let key = w.db?;
        state
            .dbs
            .get(&key)
            .and_then(|d| d.tables.get(&w.rel))
            .map(|t| t.filter.clone())
    }

    fn handle(&self, id: u64) -> WatchHandle {
        let me = self.me.clone();
        WatchHandle::new(id, move || {
            if let Some(me) = me.upgrade() {
                me.unwatch(id);
            }
        })
    }
}

#[async_trait]
impl ChangeSource for ReplicationSource {
    async fn watch(
        &self,
        ch: &TableChannel,
        id: &CheckIdentity,
        sink: mpsc::Sender<ChannelEvent>,
    ) -> Result<(WatchHandle, JoinInfo), JoinRefusal> {
        let inner = &self.inner;
        let target = CheckTarget {
            database_id: ch.database_id,
            branch_id: ch.branch_id,
            pg_database: ch.pg_database.clone(),
        };
        let (facts, resolved, bypass) = inner.probe(&target, id, ch).await?;
        let rel_oid = resolved.relation.oid;
        let pk = pk_of(&resolved.relation);
        let key: DbKey = (ch.branch_id, facts.db_oid);
        let fixed =
            if join_kind(resolved.relkind, resolved.inheritance_parent) == JoinKind::NotATable {
                Some(StatusReason::NotATable)
            } else if facts.read_only {
                Some(StatusReason::ReadOnly)
            } else {
                None
            };
        let overflow = Arc::new(AtomicBool::new(false));
        let watch_id = {
            let mut state = inner.state.lock().unwrap();
            state.next_id += 1;
            let watch_id = state.next_id;
            state.watches.insert(
                watch_id,
                WatchRec {
                    db: fixed.is_none().then_some(key),
                    rel: rel_oid,
                    channel: ch.clone(),
                    identity: id.clone(),
                    sink: sink.clone(),
                    overflow: overflow.clone(),
                    fixed,
                },
            );
            watch_id
        };
        let handle = inner.handle(watch_id);
        if let Some(reason) = fixed {
            return Ok((
                handle,
                JoinInfo {
                    pk,
                    live: false,
                    reason: Some(reason),
                    db_oid: facts.db_oid,
                    rel_oid,
                },
            ));
        }
        inner.ensure_db(key, &target);
        let Some((filter, _queue)) = inner.ensure_table(key, &resolved) else {
            return Err(JoinRefusal::Unavailable);
        };
        {
            let mut state = inner.state.lock().unwrap();
            let rel = state
                .watches
                .get(&watch_id)
                .map(|w| w.rel)
                .unwrap_or(rel_oid);
            if let Some(t) = state.dbs.get_mut(&key).and_then(|d| d.tables.get_mut(&rel)) {
                t.viewers.insert(watch_id);
            }
        }
        filter
            .lock()
            .await
            .add_shared(watch_id, id.clone(), bypass, sink, overflow);
        let (live, reason) = inner.status_of(key);
        Ok((
            handle,
            JoinInfo {
                pk,
                live,
                reason,
                db_oid: facts.db_oid,
                rel_oid,
            },
        ))
    }

    async fn view(&self, h: &WatchHandle, keys: Vec<String>) {
        // Confirmed now, by the group's own check (R19 as ruled), before any later window.
        let Some(filter) = self.inner.filter_of(h.id()) else {
            return;
        };
        // The check runs without the table's filter: its other viewers (renewals, windows)
        // never wait behind it.
        let Some(ask) = ({
            let mut f = filter.lock().await;
            let ask = f.ask_keys(h.id(), &keys, false);
            if ask.is_none() {
                f.clear_view(h.id());
            }
            ask
        }) else {
            return;
        };
        let seen = ask.run().await;
        filter.lock().await.set_view(h.id(), &ask, keys, seen);
    }

    async fn renew(&self, h: &WatchHandle, token: Arc<str>) {
        let filter = {
            let mut state = self.inner.state.lock().unwrap();
            match state.watches.get_mut(&h.id()) {
                Some(w) => w.identity.token = token.clone(),
                None => return,
            }
            drop(state);
            self.inner.filter_of(h.id())
        };
        if let Some(filter) = filter {
            filter.lock().await.renew(h.id(), token);
        }
    }

    async fn visible(&self, h: &WatchHandle, keys: &[String]) -> Result<Vec<bool>, ()> {
        let filter = {
            let state = self.inner.state.lock().unwrap();
            let w = state.watches.get(&h.id()).ok_or(())?;
            let key = w.db.ok_or(())?;
            state
                .dbs
                .get(&key)
                .and_then(|d| d.tables.get(&w.rel))
                .map(|t| t.filter.clone())
                .ok_or(())?
        };
        // As `view`: the check runs without the table's filter.
        let ask = filter.lock().await.ask_keys(h.id(), keys, true).ok_or(())?;
        ask.run().await
    }

    async fn reprobe(&self, h: &WatchHandle) -> Result<JoinInfo, JoinRefusal> {
        let inner = &self.inner;
        let (channel, identity, db, old_rel, fixed) = {
            let state = inner.state.lock().unwrap();
            let w = state.watches.get(&h.id()).ok_or(JoinRefusal::NotFound)?;
            (w.channel.clone(), w.identity.clone(), w.db, w.rel, w.fixed)
        };
        let target = CheckTarget {
            database_id: channel.database_id,
            branch_id: channel.branch_id,
            pg_database: channel.pg_database.clone(),
        };
        let (facts, resolved, _bypass) = inner.probe(&target, &identity, &channel).await?;
        let rel_oid = resolved.relation.oid;
        let pk = pk_of(&resolved.relation);
        let Some(key) = db else {
            return Ok(JoinInfo {
                pk,
                live: false,
                reason: fixed,
                db_oid: facts.db_oid,
                rel_oid,
            });
        };
        // A renamed or recreated table: the channel follows the new OID.
        // The group's classification is the next window's own probe's (R2): nothing to keep.
        inner.move_table(key, old_rel, &resolved).await;
        let (live, reason) = inner.status_of(key);
        Ok(JoinInfo {
            pk,
            live,
            reason,
            db_oid: facts.db_oid,
            rel_oid,
        })
    }

    async fn shutdown(&self) {
        ReplicationSource::shutdown(self).await;
    }

    fn abort(&self) {
        ReplicationSource::abort(self);
    }
}
