//! RT1 browser fix round (fr), the coder's edge cases beyond `fr_status`: the join answer
//! itself is the holder's state, P9's late joiners while the compute is stopping, a stop during
//! a retry wait, and the status heartbeat's fencing (W12).

mod fakes;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chelabase_nats::rt::{COMPUTE_STOPPING, ComputeStopping, ROWS};
use fakes::*;
use realtime::bus::{Bus, BusError, BusStream, MemoryBus};
use realtime::channel::TableChannel;
use realtime::check::{CheckError, Checker, ComputeFacts};
use realtime::lease::{HEARTBEAT, MemoryKv, TTL};
use realtime::protocol::StatusReason;
use realtime::replication::{ReplicationSource, SourceDeps, SourceSettings};
use realtime::slots::{Replicator, SlotError};
use realtime::source::{ChangeSource, ChannelEvent, JoinInfo, WatchHandle};
use tokio::sync::mpsc;
use tokio::time::Instant;

type Status = (bool, Option<StatusReason>);

const LIVE: Status = (true, None);
const BOUND: Duration = Duration::from_secs(HEARTBEAT.as_secs() + 1);

/// The in-memory bus, recording when each database status went out and from which holder.
struct RecordingBus {
    inner: Arc<MemoryBus>,
    statuses: Mutex<Vec<(Instant, String)>>,
}

#[async_trait]
impl Bus for RecordingBus {
    async fn publish(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<(), BusError> {
        if subject.starts_with(ROWS)
            && subject.ends_with(".0")
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload)
            && v["type"] == "status"
        {
            self.statuses.lock().unwrap().push((
                Instant::now(),
                v["holder"].as_str().unwrap_or_default().to_string(),
            ));
        }
        self.inner.publish(subject, reply, payload).await
    }
    async fn subscribe(&self, subject: &str) -> Result<BusStream, BusError> {
        self.inner.subscribe(subject).await
    }
    fn new_inbox(&self) -> String {
        self.inner.new_inbox()
    }
}

struct World {
    bus: Arc<RecordingBus>,
    leases: Arc<MemoryKv>,
    slots: Arc<MemoryKv>,
    checker: Arc<FakeChecker>,
}

struct Inst {
    src: Arc<ReplicationSource>,
    repl: Arc<FakeReplicator>,
}

impl World {
    fn new() -> Self {
        Self {
            bus: Arc::new(RecordingBus {
                inner: MemoryBus::new(),
                statuses: Mutex::new(vec![]),
            }),
            leases: MemoryKv::new(TTL),
            slots: MemoryKv::new(TTL),
            checker: FakeChecker::new(),
        }
    }
    async fn instance(&self, id: &str) -> Inst {
        let repl = FakeReplicator::new();
        let src = ReplicationSource::start(SourceDeps {
            instance_id: id.into(),
            bus: self.bus.clone(),
            leases: self.leases.clone(),
            slots: self.slots.clone(),
            checker: self.checker.clone() as Arc<dyn Checker>,
            replicator: repl.clone() as Arc<dyn Replicator>,
            settings: SourceSettings::default(),
        })
        .await
        .expect("the source starts");
        Inst { src, repl }
    }
    fn statuses_from(&self, holder: &str) -> Vec<Instant> {
        self.bus
            .statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, h)| h == holder)
            .map(|(at, _)| *at)
            .collect()
    }
    async fn compute_stopping(&self) {
        self.bus
            .publish(
                &format!("{COMPUTE_STOPPING}.{BRANCH}"),
                None,
                serde_json::to_vec(&ComputeStopping { branch_id: BRANCH })
                    .unwrap()
                    .into(),
            )
            .await
            .unwrap();
    }
}

fn compute_up() -> Result<ComputeFacts, CheckError> {
    Ok(ComputeFacts {
        read_only: false,
        max_replication_slots: 20,
        rt_slots: 0,
        db_oid: DB_OID,
        version_oid: Some(VERSION_OID),
    })
}

struct Viewer {
    _handle: WatchHandle,
    info: JoinInfo,
    rx: mpsc::Receiver<ChannelEvent>,
    last: Status,
    resynced: bool,
    history: Vec<Status>,
}

impl Viewer {
    fn pump(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                ChannelEvent::Status { live, reason } => {
                    self.last = (live, reason);
                    self.history.push(self.last);
                }
                ChannelEvent::Resync { .. } => self.resynced = true,
                ChannelEvent::Rows(_) => {}
            }
        }
    }
    async fn shows(&mut self, within: Duration, want: Status) -> bool {
        let deadline = Instant::now() + within;
        loop {
            self.pump();
            if self.last == want {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn join(inst: &Inst) -> Viewer {
    let (tx, rx) = mpsc::channel(256);
    let ch = TableChannel {
        database_id: DB,
        branch_id: BRANCH,
        pg_database: "app".into(),
        schema: "public".into(),
        table: "items".into(),
    };
    let (handle, info) = inst
        .src
        .watch(&ch, &role("billing"), tx)
        .await
        .expect("joined");
    let last = (info.live, info.reason);
    Viewer {
        _handle: handle,
        info,
        rx,
        last,
        resynced: false,
        history: vec![last],
    }
}

#[tokio::test(start_paused = true)]
async fn a_rejoin_on_the_holders_instance_is_answered_live_at_once() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut first = join(&a).await;
    assert!(first.shows(Duration::from_secs(2), LIVE).await);
    drop(first);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let again = join(&a).await;
    assert_eq!(
        (again.info.live, again.info.reason),
        LIVE,
        "the join answer itself is the holder's state (no heartbeat wait)"
    );
}

#[tokio::test(start_paused = true)]
async fn late_joiners_while_the_compute_is_stopping_are_told_compute_stopping_until_a_slot() {
    let w = World::new();
    let a = w.instance("a").await;
    let b = w.instance("b").await;
    let mut first = join(&a).await;
    assert!(first.shows(Duration::from_secs(2), LIVE).await);
    w.checker
        .set_compute(Err(CheckError::Unavailable("connection route 409".into())));
    w.compute_stopping().await;
    let stopping = (false, Some(StatusReason::ComputeStopping));
    assert!(
        first.shows(Duration::from_secs(1), stopping).await,
        "{:?}",
        first.history
    );
    // The last watcher leaves: the instance forgets the database's channel.
    drop(first);
    tokio::time::sleep(Duration::from_secs(1)).await;
    // The compute answers again, but slot creation hangs: no holder has spoken yet.
    a.repl.plan(StartPlan::Block);
    b.repl.plan(StartPlan::Block);
    w.checker.set_compute(compute_up());
    let mut local = join(&a).await;
    let mut remote = join(&b).await;
    for v in [&local, &remote] {
        assert_eq!(
            (v.info.live, v.info.reason),
            stopping,
            "P9: a late join while no slot came back is answered compute_stopping"
        );
    }
    tokio::time::sleep(BOUND).await;
    for v in [&mut local, &mut remote] {
        v.pump();
        assert_eq!(v.last, stopping, "{:?}", v.history);
    }
    // The blocked creation times out (`waiting`), and the retry streams: resync, then live.
    for v in [&mut local, &mut remote] {
        assert!(
            v.shows(Duration::from_secs(60), LIVE).await,
            "{:?}",
            v.history
        );
        assert!(v.resynced, "R8: the slot's resync");
    }
    // The database has been spoken for: a later join is no longer told compute_stopping.
    drop(local);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let later = join(&a).await;
    assert_ne!((later.info.live, later.info.reason), stopping);
}

#[tokio::test(start_paused = true)]
async fn a_compute_stop_during_a_no_slot_retry_tells_compute_stopping() {
    let w = World::new();
    let a = w.instance("a").await;
    let b = w.instance("b").await;
    a.repl.plan(StartPlan::Fail(SlotError::NoSlot));
    let mut local = join(&a).await;
    let no_slot = (false, Some(StatusReason::NoSlot));
    assert!(local.shows(Duration::from_secs(2), no_slot).await);
    let mut remote = join(&b).await;
    assert!(remote.shows(BOUND, no_slot).await, "{:?}", remote.history);
    w.checker
        .set_compute(Err(CheckError::Unavailable("connection route 409".into())));
    w.compute_stopping().await;
    for v in [&mut local, &mut remote] {
        assert!(
            v.shows(
                Duration::from_secs(1),
                (false, Some(StatusReason::ComputeStopping))
            )
            .await,
            "P9 from a retry wait: {:?}",
            v.history
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_fenced_holders_status_heartbeat_never_goes_out() {
    let w = World::new();
    let a = w.instance("a").await;
    a.repl.plan(StartPlan::Fail(SlotError::NoSlot));
    let t0 = Instant::now();
    let mut v = join(&a).await;
    let no_slot = (false, Some(StatusReason::NoSlot));
    assert!(v.shows(Duration::from_secs(2), no_slot).await);
    // Renewals at t0 + 3, 6, …; the store goes down after the one at t0 + 6, so the local
    // deadline is t0 + 6 + 7 = t0 + 13 (the retry after no_slot is due at t0 + 30).
    tokio::time::sleep_until(t0 + Duration::from_millis(7_500)).await;
    w.leases.set_down(true);
    tokio::time::sleep_until(t0 + Duration::from_secs(12)).await;
    let before = w.statuses_from("a");
    assert!(
        before.len() >= 3,
        "precondition: the holder re-tells no_slot while it waits to retry: {before:?}"
    );
    assert!(
        v.shows(Duration::from_secs(3), (false, Some(StatusReason::Waiting)))
            .await,
        "the fenced holder's watchers stop showing its state: {:?}",
        v.history
    );
    tokio::time::sleep(Duration::from_secs(40)).await;
    let fence = t0 + Duration::from_secs(13);
    let after: Vec<Instant> = w
        .statuses_from("a")
        .into_iter()
        .filter(|at| *at >= fence)
        .collect();
    assert!(
        after.is_empty(),
        "W12: nothing goes out over NATS past the deadline: {:?}",
        after.iter().map(|at| *at - t0).collect::<Vec<_>>()
    );
    v.pump();
    assert_eq!(
        v.last,
        (false, Some(StatusReason::Waiting)),
        "{:?}",
        v.history
    );
}
