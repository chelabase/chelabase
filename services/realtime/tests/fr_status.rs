//! RT1 browser fix round (fr): a watcher's `live` status converges to the holder's actual state.
//!
//! **Invariant:** within [`BOUND`] (the 5 s per-database status re-publish on
//! `chelabase.rt.rows.<branch>.<db>.0`, [`HEARTBEAT`], plus 1 s) of the holder's state settling
//! or of a watcher joining, every current watcher, local or remote, joined before or after the
//! slot started, re-joined after a leave, on any table of the database, shows the holder's
//! state: its join answer (`JoinInfo.live/reason`) followed by every `status` event it got ends at
//! - `live: true` while a slot streams;
//! - `live: false` with the holder's reason while it doesn't (`no_slot` while the holder retries
//!   after `no_slot`; `waiting` once a holder lost its lease and stopped).
//!
//! No watcher stays `waiting` while the holder is live. Found by RT1's browser pass: every
//! Studio join said "Live updates paused · waiting for a slot" while splices flowed (a watcher
//! that joins after its instance dropped the database's entry gets the default `waiting`, and
//! the holder's re-publish is ignored by its own instance).
//!
//! The examples name each path; the property test runs join/leave/slot-restart/no-slot/wait
//! sequences over two instances and two tables of one database.

mod fakes;

use std::sync::Arc;
use std::time::Duration;

use fakes::*;
use proptest::prelude::*;
use realtime::bus::MemoryBus;
use realtime::channel::TableChannel;
use realtime::check::Checker;
use realtime::lease::{HEARTBEAT, KvStore, Lease, MemoryKv, RENEW_EVERY, TTL, lease_key};
use realtime::protocol::StatusReason;
use realtime::replication::{ReplicationSource, SourceDeps, SourceSettings};
use realtime::slots::{Replicator, SlotError};
use realtime::source::{ChangeSource, ChannelEvent, JoinInfo, WatchHandle};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// The convergence bound: the per-database status re-publish (every [`HEARTBEAT`]) plus 1 s.
const BOUND: Duration = Duration::from_secs(HEARTBEAT.as_secs() + 1);

const ORDERS: u32 = 16_500;

type Status = (bool, Option<StatusReason>);

const LIVE: Status = (true, None);

fn channel(t: &str) -> TableChannel {
    TableChannel {
        database_id: DB,
        branch_id: BRANCH,
        pg_database: "app".into(),
        schema: "public".into(),
        table: t.into(),
    }
}

struct Inst {
    src: Arc<ReplicationSource>,
    repl: Arc<FakeReplicator>,
}

struct World {
    bus: Arc<MemoryBus>,
    leases: Arc<MemoryKv>,
    slots: Arc<MemoryKv>,
    checker: Arc<FakeChecker>,
}

impl World {
    fn new() -> Self {
        let checker = FakeChecker::new();
        checker.set_resolve(
            "public",
            "orders",
            Ok(Some(resolved(relation(ORDERS, "orders")))),
        );
        Self {
            bus: MemoryBus::new(),
            leases: MemoryKv::new(TTL),
            slots: MemoryKv::new(TTL),
            checker,
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
}

/// One watcher: what its join said, then every status it was sent.
struct Viewer {
    /// Dropping it leaves.
    _handle: WatchHandle,
    info: JoinInfo,
    rx: mpsc::Receiver<ChannelEvent>,
    /// The status it shows now.
    last: Status,
    /// Every status it showed, in order (the join's first).
    history: Vec<Status>,
}

impl Viewer {
    fn pump(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            if let ChannelEvent::Status { live, reason } = ev {
                self.last = (live, reason);
                self.history.push(self.last);
            }
        }
    }
}

async fn join(inst: &Inst, table: &str) -> Viewer {
    let (tx, rx) = mpsc::channel(256);
    let joined = inst.src.watch(&channel(table), &role("billing"), tx).await;
    assert!(joined.is_ok(), "joined: {:?}", joined.as_ref().err());
    let (handle, info) = joined.unwrap();
    let last = (info.live, info.reason);
    Viewer {
        _handle: handle,
        info,
        rx,
        last,
        history: vec![last],
    }
}

/// Whether `v` shows `want` within `within` (polled every 10 ms).
async fn shows(v: &mut Viewer, within: Duration, want: Status) -> bool {
    let deadline = Instant::now() + within;
    loop {
        v.pump();
        if v.last == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A viewer of `items` on `inst` once the slot streams.
async fn streaming(inst: &Inst) -> Viewer {
    let mut v = join(inst, "items").await;
    assert!(
        shows(&mut v, Duration::from_secs(2), LIVE).await,
        "precondition: the slot streams: {:?}",
        v.history
    );
    v
}

fn open_sessions(insts: &[&Inst]) -> usize {
    insts
        .iter()
        .flat_map(|i| i.repl.sessions())
        .filter(|s| !s.closed())
        .count()
}

// --- the named paths ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_viewer_rejoining_a_streaming_table_is_told_live() {
    let w = World::new();
    let a = w.instance("a").await;
    let first = streaming(&a).await;
    drop(first);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        open_sessions(&[&a]),
        1,
        "precondition: the holder keeps streaming (30 s idle)"
    );
    let mut second = join(&a, "items").await;
    assert!(
        shows(&mut second, BOUND, LIVE).await,
        "a re-join on the holder's own instance while its slot streams is told live within \
         {BOUND:?}: join answered {:?}, then {:?}",
        second.info,
        second.history
    );
}

#[tokio::test(start_paused = true)]
async fn a_second_table_joined_after_the_first_left_is_told_live() {
    let w = World::new();
    let a = w.instance("a").await;
    let first = streaming(&a).await;
    drop(first);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(open_sessions(&[&a]), 1, "precondition: still streaming");
    let mut orders = join(&a, "orders").await;
    assert!(
        shows(&mut orders, BOUND, LIVE).await,
        "another table of the streaming database is told live within {BOUND:?}: join \
         answered {:?}, then {:?}",
        orders.info,
        orders.history
    );
}

/// Guard (passes on the base): the database's entry still exists.
#[tokio::test(start_paused = true)]
async fn a_second_table_joined_while_the_first_watches_is_told_live() {
    let w = World::new();
    let a = w.instance("a").await;
    let _items = streaming(&a).await;
    let mut orders = join(&a, "orders").await;
    assert!(
        shows(&mut orders, BOUND, LIVE).await,
        "join answered {:?}, then {:?}",
        orders.info,
        orders.history
    );
}

/// Guard (passes on the base through the 5 s re-publish).
#[tokio::test(start_paused = true)]
async fn a_join_and_a_rejoin_on_another_instance_are_told_live() {
    let w = World::new();
    let a = w.instance("a").await;
    let _held = streaming(&a).await;
    let b = w.instance("b").await;
    let mut first = join(&b, "items").await;
    assert!(
        shows(&mut first, BOUND, LIVE).await,
        "join answered {:?}, then {:?}",
        first.info,
        first.history
    );
    drop(first);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut again = join(&b, "orders").await;
    assert!(
        shows(&mut again, BOUND, LIVE).await,
        "join answered {:?}, then {:?}",
        again.info,
        again.history
    );
}

/// Guard (passes on the base): a slot restart while watchers stay, local and remote.
#[tokio::test(start_paused = true)]
async fn a_holder_restart_tells_every_watcher_live_again() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut local = streaming(&a).await;
    let b = w.instance("b").await;
    let mut remote = join(&b, "orders").await;
    assert!(
        shows(&mut remote, BOUND, LIVE).await,
        "{:?}",
        remote.history
    );
    a.repl.session().unwrap().end();
    tokio::time::sleep(BOUND).await;
    assert_eq!(a.repl.starts(), 2, "precondition: a fresh slot");
    for v in [&mut local, &mut remote] {
        v.pump();
        assert_eq!(v.last, LIVE, "{:?}", v.history);
        assert!(
            v.history.contains(&(false, Some(StatusReason::Waiting))),
            "the gap was shown: {:?}",
            v.history
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_remote_late_joiner_during_no_slot_is_told_no_slot() {
    let w = World::new();
    let a = w.instance("a").await;
    a.repl.plan(StartPlan::Fail(SlotError::NoSlot));
    let mut first = join(&a, "items").await;
    assert!(
        shows(
            &mut first,
            Duration::from_secs(2),
            (false, Some(StatusReason::NoSlot))
        )
        .await,
        "precondition: {:?}",
        first.history
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    let b = w.instance("b").await;
    let mut late = join(&b, "items").await;
    assert!(
        shows(&mut late, BOUND, (false, Some(StatusReason::NoSlot))).await,
        "a watcher on another instance joining while the holder retries after no_slot is told \
         no_slot within {BOUND:?} (not left at waiting until the 30 s retry): join answered \
         {:?}, then {:?}",
        late.info,
        late.history
    );
}

#[tokio::test(start_paused = true)]
async fn a_local_rejoin_during_no_slot_is_told_no_slot() {
    let w = World::new();
    let a = w.instance("a").await;
    a.repl.plan(StartPlan::Fail(SlotError::NoSlot));
    let mut first = join(&a, "items").await;
    assert!(
        shows(
            &mut first,
            Duration::from_secs(2),
            (false, Some(StatusReason::NoSlot))
        )
        .await,
        "precondition: {:?}",
        first.history
    );
    drop(first);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut again = join(&a, "items").await;
    assert!(
        shows(&mut again, BOUND, (false, Some(StatusReason::NoSlot))).await,
        "a re-join on the holder's instance while it retries after no_slot is told no_slot \
         within {BOUND:?}: join answered {:?}, then {:?}",
        again.info,
        again.history
    );
}

#[tokio::test(start_paused = true)]
async fn a_holder_that_lost_its_lease_tells_its_watchers_it_stopped() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut v = streaming(&a).await;
    let session = a.repl.session().unwrap();
    // Another holder's revision replaces ours, and it keeps renewing (it streams elsewhere,
    // without anything reaching this instance).
    let key = lease_key(BRANCH, DB_OID);
    let (_, rev) = w.leases.get(&key).await.unwrap().unwrap();
    w.leases.delete_expect(&key, rev).await.unwrap();
    let mut other = Lease::acquire(w.leases.as_ref(), &key, "other")
        .await
        .unwrap()
        .expect("the other holder takes the lease");
    let kv = w.leases.clone();
    let _renewing = tokio::spawn(async move {
        loop {
            tokio::time::sleep(RENEW_EVERY).await;
            let _ = other.renew(kv.as_ref()).await;
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session.closed() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(session.closed(), "precondition: fenced at the next renewal");
    assert!(
        shows(&mut v, BOUND, (false, Some(StatusReason::Waiting))).await,
        "a fenced holder's watchers stop showing live within {BOUND:?}: {:?}",
        v.history
    );
}

// --- the property -------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    /// A watcher joins on instance `b` (else `a`), table `orders` (else `items`).
    Join {
        b: bool,
        orders: bool,
    },
    /// The `n`-th current watcher (modulo) leaves.
    Leave(usize),
    /// The streaming slot's walsender goes (a compute restart): the holder restarts it.
    EndSlot,
    /// As [`Op::EndSlot`], and the holder's next slot creation fails with `no_slot` (retried
    /// 30 s later).
    NoSlotRestart,
    Wait(u64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<bool>(), any::<bool>()).prop_map(|(b, orders)| Op::Join { b, orders }),
        3 => (0usize..8).prop_map(Op::Leave),
        1 => Just(Op::EndSlot),
        1 => Just(Op::NoSlotRestart),
        2 => (0u64..=40).prop_map(Op::Wait),
    ]
}

/// What changes the holder's state: slot creations and closed sessions, per instance.
fn fingerprint(insts: &[&Inst]) -> Vec<(usize, Vec<bool>)> {
    insts
        .iter()
        .map(|i| {
            (
                i.repl.starts(),
                i.repl.sessions().iter().map(|s| s.closed()).collect(),
            )
        })
        .collect()
}

async fn run(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let w = World::new();
    let a = w.instance("a").await;
    let b = w.instance("b").await;
    let mut viewers: Vec<(String, Viewer)> = vec![];
    for (step, op) in ops.iter().enumerate() {
        match op {
            Op::Join { b: on_b, orders } => {
                let inst = if *on_b { &b } else { &a };
                let table = if *orders { "orders" } else { "items" };
                let v = join(inst, table).await;
                let label = format!(
                    "step {step}: {} on {}",
                    table,
                    if *on_b { "b" } else { "a" }
                );
                viewers.push((label, v));
            }
            Op::Leave(n) => {
                if !viewers.is_empty() {
                    let i = n % viewers.len();
                    viewers.remove(i);
                }
            }
            Op::EndSlot | Op::NoSlotRestart => {
                for inst in [&a, &b] {
                    if let Some(s) = inst.repl.sessions().into_iter().find(|s| !s.closed()) {
                        if matches!(op, Op::NoSlotRestart) {
                            inst.repl.plan(StartPlan::Fail(SlotError::NoSlot));
                        }
                        s.end();
                    }
                }
            }
            Op::Wait(secs) => tokio::time::sleep(Duration::from_secs(*secs)).await,
        }
        // Settle, then check only if nothing changed the holder's state for a whole bound.
        tokio::time::sleep(BOUND).await;
        let before = fingerprint(&[&a, &b]);
        tokio::time::sleep(BOUND).await;
        if fingerprint(&[&a, &b]) != before || viewers.is_empty() {
            continue;
        }
        let want: Status = if open_sessions(&[&a, &b]) > 0 {
            LIVE
        } else {
            // With watchers and a stable state, the only way to have no slot here is a holder
            // waiting to retry after `no_slot`.
            (false, Some(StatusReason::NoSlot))
        };
        for (label, v) in viewers.iter_mut() {
            v.pump();
            prop_assert_eq!(
                v.last,
                want,
                "{} (join answered {:?}, then {:?}) after {:?}",
                label,
                v.info,
                v.history,
                &ops[..=step]
            );
        }
    }
    a.src.abort();
    b.src.abort();
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn every_watcher_converges_to_the_holders_state(ops in proptest::collection::vec(op(), 1..12)) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(run(ops))?;
    }
}
