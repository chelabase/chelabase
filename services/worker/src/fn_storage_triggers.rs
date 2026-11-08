//! Edge functions m14: storage triggers. Each matching object event on a trigger's branch is
//! delivered to its function, at least once, as `POST /internal/invoke/{branch_id}/{name}` on
//! the functions service with the worker's bearer and the body `{"type": "storage", "event":
//! <ObjectEvent>}`. The functions service marks it and the runtime hands the event to the
//! function's `fetch` with `chelabase-event: storage`; run-as env is the functions service's.
//!
//! Modelled on [`crate::storage_webhooks`]. Each enabled storage trigger on a live branch
//! ([`chelabase_metadata::MetadataClient::storage_trigger_targets`]) has its own durable
//! consumer on the object events stream
//! ([`storage_objects::fn_trigger_consumer`]: `fn-trigger-<id>-<enabled_since_ms>`, filtered to
//! the database, from `enabled_since`), so a re-enabled trigger is a new consumer that never
//! replays the disabled window, and a branch's copy starts at its own creation. [`run`] reads
//! the targets every [`POLL`]: a new trigger starts, a disabled, deleted or tombstoned one stops
//! and its consumer is deleted ([`reconcile`]). A disabled *function* keeps its trigger: the
//! functions service refuses the invocation, so its events wait (retried) up to [`MAX_AGE`].
//!
//! Each message ([`Delivery::process`]): an event of another branch, Postgres database or
//! database, or one the filters don't match, is acknowledged unsent ([`matches`]); one
//! [`MAX_AGE`] old is dropped and counted; otherwise the function is invoked. A 2xx
//! acknowledges; anything else NAKs with the webhooks' backoff and jitter
//! ([`crate::storage_webhooks::backoff`]). Every outcome reaches the trigger's row through
//! `record_trigger_deliveries` (`last_delivered_at`, `last_delivery_error`, `dropped_count`).
//!
//! **Invariant (drops).** An event of the trigger's subject that is gone from the stream before
//! the trigger settled it (the per-database cap, [`storage_objects::MAX_EVENTS_PER_DATABASE`],
//! or the stream's own limits) is counted in `dropped_count` exactly once, while one worker
//! runs the trigger. JetStream keeps no per-subject sequence and says nothing about what it
//! evicts, so each trigger also reads a watcher ([`storage_objects::fn_trigger_watcher`]) at
//! the subject's head and keeps the arrivals of its own (branch, Postgres database, filters)
//! its consumer hasn't reached ([`Gaps`]): an arrival with `cap` newer arrivals of the database
//! after it is gone (the cap keeps the newest);
//! a sequence the consumer skips past is gone; a NAKed event awaiting redelivery is gone once
//! `cap` events arrive after it. What was evicted while no worker ran the trigger is not
//! counted. `fn_storage_triggers_cap_evictions_are_counted_as_dropped` and its neighbours test
//! it.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chelabase_metadata::{MetadataClient, StorageTriggerTarget, TriggerDelivery};
use chelabase_nats::storage_objects::{self, ObjectEvent, ObjectEventType};
use chelabase_types::BranchId;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use tracing::{info, warn};

use crate::fn_schedules::Invoker;
use crate::storage_webhooks::backoff;

/// An event is retried for this long after it happened, then dropped (the stream keeps it an
/// hour longer).
pub const MAX_AGE: chrono::TimeDelta = chrono::TimeDelta::hours(24);
/// How often the triggers are read again.
pub const POLL: Duration = Duration::from_secs(10);
/// How long a delivered message waits for its ack before redelivery: above the invoke
/// request's own timeout (60 s), itself above the functions service's default 30 s
/// (`CHELABASE_FUNCTIONS_TIMEOUT_MS`).
pub const ACK_WAIT: Duration = Duration::from_secs(90);
/// Deliveries in flight per trigger.
const CONCURRENCY: usize = 4;

/// The invocation's body: what the runtime unwraps for the function's `fetch`.
pub fn event_body(event: &ObjectEvent) -> serde_json::Value {
    serde_json::json!({ "type": "storage", "event": event })
}

/// Whether `event` is the trigger's: its database, branch and Postgres database, one of its
/// events, and its bucket and name prefix when set (a move matches on where the object went or
/// where it came from).
pub fn matches(t: &StorageTriggerTarget, event: &ObjectEvent) -> bool {
    if event.database_id != t.database_id.as_uuid()
        || event.branch_id != t.branch_id.as_uuid()
        || event.pg_database != t.pg_database
        || event.kind == ObjectEventType::Test
        || !t.events.iter().any(|e| e == event.kind.as_str())
    {
        return false;
    }
    let at = |bucket: &str, name: &str| {
        t.bucket.as_deref().is_none_or(|b| b == bucket)
            && t.prefix.as_deref().is_none_or(|p| name.starts_with(p))
    };
    at(&event.bucket, &event.name) || event.from.as_ref().is_some_and(|f| at(&f.bucket, &f.name))
}

/// What to do with a message after an attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Next {
    /// Delivered, or nothing to deliver: acknowledge.
    Ack,
    /// Failed: redeliver after the delay.
    Retry(Duration),
    /// [`MAX_AGE`] old: acknowledged unsent, and counted.
    Dropped,
}

/// Where outcomes go (the trigger's row; a fake in tests).
#[async_trait::async_trait]
pub trait Recorder: Send + Sync + 'static {
    async fn record(&self, branch: BranchId, delivery: TriggerDelivery) -> Result<(), String>;
}

#[async_trait::async_trait]
impl Recorder for MetadataClient {
    async fn record(&self, branch: BranchId, delivery: TriggerDelivery) -> Result<(), String> {
        self.record_trigger_deliveries(branch, std::slice::from_ref(&delivery))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Delivers messages to the triggers' functions: the invoker, where outcomes go, and the count
/// of events dropped after [`MAX_AGE`].
pub struct Delivery<R> {
    invoker: Arc<dyn Invoker>,
    recorder: R,
    pub dropped: AtomicU64,
}

impl<R: Recorder> Delivery<R> {
    pub fn new(invoker: Arc<dyn Invoker>, recorder: R) -> Self {
        Self {
            invoker,
            recorder,
            dropped: AtomicU64::new(0),
        }
    }

    async fn record(&self, t: &StorageTriggerTarget, delivery: TriggerDelivery) {
        if let Err(e) = self.recorder.record(t.branch_id, delivery).await {
            warn!(trigger = %t.id, "storage trigger: recording an outcome: {e}");
        }
    }

    /// Counts `n` events gone from the stream before the trigger settled them.
    pub async fn record_evicted(&self, t: &StorageTriggerTarget, n: u64) {
        if n == 0 {
            return;
        }
        warn!(trigger = %t.id, evicted = n, "storage trigger events evicted by the stream's limits before delivery");
        self.record(t, outcome(t, false, n, None)).await;
    }

    /// One delivery attempt (number `attempt`, 1 = the first) of the message `payload` to
    /// trigger `t`, at `now`.
    pub async fn process(
        &self,
        t: &StorageTriggerTarget,
        payload: &[u8],
        attempt: u64,
        now: DateTime<Utc>,
        jitter: f64,
    ) -> Next {
        let Ok(event) = serde_json::from_slice::<ObjectEvent>(payload) else {
            warn!(trigger = %t.id, "storage trigger: an unreadable event skipped");
            return Next::Ack;
        };
        if !matches(t, &event) {
            return Next::Ack;
        }
        if now - event.time >= MAX_AGE {
            self.count_drop(t, &event);
            self.record(t, outcome(t, false, 1, None)).await;
            return Next::Dropped;
        }
        let error = match self
            .invoker
            .invoke(t.branch_id, &t.function_name, event_body(&event))
            .await
        {
            Ok(status) if (200..300).contains(&status) => None,
            Ok(status) => Some(format!("the function answered {status}")),
            Err(e) => Some(format!("the functions service didn't answer: {e}")),
        };
        let Some(error) = error else {
            self.record(t, outcome(t, true, 0, None)).await;
            return Next::Ack;
        };
        // Retried until a redelivery finds it [`MAX_AGE`] old (above).
        self.record(t, outcome(t, false, 0, Some(error))).await;
        Next::Retry(backoff(attempt, jitter))
    }

    fn count_drop(&self, t: &StorageTriggerTarget, event: &ObjectEvent) {
        let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        warn!(trigger = %t.id, event = %event.id, dropped_total = total, "storage trigger event dropped after 24 h");
    }
}

fn outcome(
    t: &StorageTriggerTarget,
    delivered: bool,
    dropped: u64,
    error: Option<String>,
) -> TriggerDelivery {
    TriggerDelivery {
        id: t.id,
        delivered,
        dead: 0,
        dropped: dropped.min(i64::MAX as u64) as i64,
        error,
    }
}

/// One trigger's view of its subject's arrivals (the module's drop invariant). Arrivals are
/// numbered in the order the watcher sees them (ordinals); the subject keeps only its newest
/// `cap` events, so an event with `cap` arrivals after it is gone.
pub struct Gaps {
    cap: u64,
    /// The newest ordinal.
    latest: u64,
    /// The newest stream sequence the consumer has had a first delivery of.
    delivered: u64,
    /// Arrivals the consumer hasn't reached: (stream sequence, ordinal), oldest first.
    waiting: VecDeque<(u64, u64)>,
    /// NAKed events awaiting redelivery: stream sequence → ordinal.
    retrying: HashMap<u64, u64>,
}

impl Gaps {
    /// `delivered`: the consumer's newest delivered stream sequence when the watcher starts
    /// (the watcher reads from the next).
    pub fn new(cap: u64, delivered: u64) -> Self {
        Self {
            cap: cap.max(1),
            latest: 0,
            delivered,
            waiting: VecDeque::new(),
            retrying: HashMap::new(),
        }
    }

    /// The watcher saw stream sequence `seq` arrive, the trigger's own event or not
    /// (`relevant`: every event counts toward the cap, only the trigger's are counted when
    /// gone): how many of the trigger's events are now known gone.
    pub fn arrived(&mut self, seq: u64, relevant: bool) -> u64 {
        self.latest += 1;
        if relevant && seq > self.delivered {
            self.waiting.push_back((seq, self.latest));
        }
        self.evict()
    }

    fn gone(&self, ordinal: u64) -> bool {
        ordinal.saturating_add(self.cap) <= self.latest
    }

    fn evict(&mut self) -> u64 {
        let mut n = 0;
        while self.waiting.front().is_some_and(|&(_, o)| self.gone(o)) {
            self.waiting.pop_front();
            n += 1;
        }
        let before = self.retrying.len();
        let (cap, latest) = (self.cap, self.latest);
        self.retrying.retain(|_, o| o.saturating_add(cap) > latest);
        n + (before - self.retrying.len()) as u64
    }

    /// The consumer's first delivery of `seq`: how many waiting events it skipped (gone), and
    /// `seq`'s ordinal when the watcher had seen it.
    pub fn first_delivered(&mut self, seq: u64) -> (u64, Option<u64>) {
        self.delivered = self.delivered.max(seq);
        let mut skipped = 0;
        while self.waiting.front().is_some_and(|&(s, _)| s < seq) {
            self.waiting.pop_front();
            skipped += 1;
        }
        let ordinal = match self.waiting.front() {
            Some(&(s, o)) if s == seq => {
                self.waiting.pop_front();
                Some(o)
            }
            _ => None,
        };
        (skipped, ordinal)
    }

    /// A redelivery of `seq`: in flight again, so no longer counted if evicted; its ordinal.
    pub fn redelivered(&mut self, seq: u64) -> Option<u64> {
        self.retrying.remove(&seq)
    }

    /// `seq` was NAKed: it waits for redelivery (an unknown ordinal counts from the newest
    /// arrival, so it is never counted early). How many events that leaves known gone (itself,
    /// when it already is).
    pub fn unsettled(&mut self, seq: u64, ordinal: Option<u64>) -> u64 {
        self.retrying.insert(seq, ordinal.unwrap_or(self.latest));
        self.evict()
    }
}

/// Deletes consumers (JetStream; a fake in tests).
#[async_trait::async_trait]
pub trait Bus: Send + Sync {
    async fn delete_consumer(&self, name: &str) -> Result<(), String>;
}

#[async_trait::async_trait]
impl Bus for async_nats::jetstream::Context {
    async fn delete_consumer(&self, name: &str) -> Result<(), String> {
        self.delete_consumer_from_stream(name, storage_objects::STREAM)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// A trigger's running task and the target it delivers to.
pub struct Running {
    pub task: tokio::task::JoinHandle<()>,
    live: tokio::sync::watch::Sender<Arc<StorageTriggerTarget>>,
}

type Start = dyn Fn(tokio::sync::watch::Receiver<Arc<StorageTriggerTarget>>) -> tokio::task::JoinHandle<()>
    + Send
    + Sync;

/// Brings the running tasks in line with `targets`: every running consumer no longer wanted
/// (its trigger disabled, re-enabled since, deleted, or its branch or database tombstoned)
/// stops and is deleted; every wanted one runs (`start`), a running one getting the newest
/// target.
pub async fn reconcile<B: Bus>(
    bus: &B,
    running: &mut HashMap<String, Running>,
    targets: Vec<StorageTriggerTarget>,
    start: &Start,
) {
    let wanted: HashMap<String, Arc<StorageTriggerTarget>> = targets
        .into_iter()
        .map(|t| {
            (
                storage_objects::fn_trigger_consumer_name(t.id, t.enabled_since),
                Arc::new(t),
            )
        })
        .collect();
    let mut stop: Vec<String> = running
        .keys()
        .filter(|n| !wanted.contains_key(*n))
        .cloned()
        .collect();
    stop.sort();
    for name in stop {
        if let Some(r) = running.remove(&name) {
            r.task.abort();
        }
        if let Err(e) = bus.delete_consumer(&name).await {
            warn!(consumer = %name, "storage trigger consumer not deleted (it expires unused): {e}");
        }
    }
    for (name, target) in wanted {
        match running.get(&name) {
            Some(r) if !r.task.is_finished() => {
                r.live.send_replace(target);
            }
            _ => {
                let (tx, rx) = tokio::sync::watch::channel(target);
                let task = start(rx);
                running.insert(name, Running { task, live: tx });
            }
        }
    }
}

/// Runs the trigger tasks for as long as the process runs (see the module docs).
pub async fn run(
    metadata: MetadataClient,
    js: async_nats::jetstream::Context,
    invoker: Arc<dyn Invoker>,
) {
    let delivery = Arc::new(Delivery::new(invoker, metadata.clone()));
    let start = {
        let (js, delivery) = (js.clone(), delivery.clone());
        move |rx| tokio::spawn(trigger_loop(js.clone(), delivery.clone(), rx))
    };
    let mut running = HashMap::new();
    loop {
        match metadata.storage_trigger_targets().await {
            Ok(targets) => reconcile(&js, &mut running, targets, &start).await,
            Err(e) => warn!("storage triggers: reading the triggers: {e}"),
        }
        tokio::time::sleep(POLL).await;
    }
}

/// One trigger's consumer, consumed until the task is stopped (again after [`POLL`] when it
/// fails: the stream not there yet, NATS away).
async fn trigger_loop(
    js: async_nats::jetstream::Context,
    delivery: Arc<Delivery<MetadataClient>>,
    live: tokio::sync::watch::Receiver<Arc<StorageTriggerTarget>>,
) {
    loop {
        if let Err(e) = consume(&js, &delivery, &live).await {
            let id = live.borrow().id;
            warn!(trigger = %id, "storage trigger consumer: {e}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn consume(
    js: &async_nats::jetstream::Context,
    delivery: &Arc<Delivery<MetadataClient>>,
    live: &tokio::sync::watch::Receiver<Arc<StorageTriggerTarget>>,
) -> anyhow::Result<()> {
    let target = live.borrow().clone();
    let config = storage_objects::fn_trigger_consumer(
        target.id,
        target.database_id.as_uuid(),
        target.enabled_since,
        ACK_WAIT,
    );
    let mut consumer = js
        .create_consumer_on_stream(config, storage_objects::STREAM)
        .await
        .map_err(|e| anyhow::anyhow!("creating the consumer: {e}"))?;
    let delivered = consumer
        .info()
        .await
        .map_err(|e| anyhow::anyhow!("reading the consumer: {e}"))?
        .delivered
        .stream_sequence;
    let cap = storage_objects::MAX_EVENTS_PER_DATABASE as u64;
    let gaps = Arc::new(Mutex::new(Gaps::new(cap, delivered)));
    let watcher = js
        .create_consumer_on_stream(
            storage_objects::fn_trigger_watcher(
                target.id,
                target.database_id.as_uuid(),
                delivered + 1,
            ),
            storage_objects::STREAM,
        )
        .await
        .map_err(|e| anyhow::anyhow!("creating the watcher: {e}"))?;
    let messages = consumer
        .messages()
        .await
        .map_err(|e| anyhow::anyhow!("pulling: {e}"))?;
    let mut arrivals = watcher
        .messages()
        .await
        .map_err(|e| anyhow::anyhow!("pulling the watcher: {e}"))?;
    info!(trigger = %target.id, "storage trigger delivering");

    let watch = async {
        while let Some(m) = arrivals.next().await {
            let Ok(m) = m else { continue };
            let Ok(seq) = m.info().map(|i| i.stream_sequence) else {
                continue;
            };
            let current = live.borrow().clone();
            // An unreadable event is the trigger's for counting: it is never delivered either.
            let relevant = serde_json::from_slice::<ObjectEvent>(&m.payload)
                .map_or(true, |e| matches(&current, &e));
            let gone = gaps.lock().expect("gaps").arrived(seq, relevant);
            delivery.record_evicted(&current, gone).await;
        }
    };
    let deliver = messages
        .map(|message| {
            // In the order the consumer delivers (before any concurrency): first deliveries
            // come in stream order.
            let message = message.ok()?;
            let info = message.info().ok()?;
            let (seq, first) = (info.stream_sequence, info.delivered <= 1);
            let attempt = info.delivered.max(1) as u64;
            let mut g = gaps.lock().expect("gaps");
            let (skipped, ordinal) = if first {
                g.first_delivered(seq)
            } else {
                (0, g.redelivered(seq))
            };
            drop(g);
            Some((message, seq, attempt, skipped, ordinal))
        })
        .for_each_concurrent(CONCURRENCY, |item| async {
            let Some((message, seq, attempt, skipped, ordinal)) = item else {
                return;
            };
            let current = live.borrow().clone();
            delivery.record_evicted(&current, skipped).await;
            let jitter = (rand::random::<f64>() - 0.5) * 0.4;
            let next = delivery
                .process(&current, &message.payload, attempt, Utc::now(), jitter)
                .await;
            let ack = match next {
                Next::Retry(delay) => {
                    let gone = gaps.lock().expect("gaps").unsettled(seq, ordinal);
                    delivery.record_evicted(&current, gone).await;
                    message
                        .ack_with(async_nats::jetstream::AckKind::Nak(Some(delay)))
                        .await
                }
                Next::Ack | Next::Dropped => message.ack().await,
            };
            if let Err(e) = ack {
                warn!(trigger = %current.id, "storage trigger ack: {e}");
            }
        });
    tokio::select! {
        () = deliver => Ok(()),
        () = watch => Err(anyhow::anyhow!("the watcher stopped")),
    }
}
