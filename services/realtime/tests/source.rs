//! RT1 T8: the change source end to end on fakes (spec §3.3, §4; R2, R8, R16, R17, R22, W7, W8,
//! W10, W12, W21, W25, P9, P14, S4, S5, S25, S26, S29, S30): [`ReplicationSource`] on the
//! in-memory bus and KV, the fake checker and the fake walsender, on tokio's paused clock.

mod fakes;

use std::sync::Arc;
use std::time::Duration;

use chelabase_nats::rt::{COMPUTE_STOPPING, ComputeStopping};
use fakes::*;
use realtime::bus::{Bus, MemoryBus};
use realtime::channel::TableChannel;
use realtime::check::{CheckError, Checker};
use realtime::lease::{KvStore, MemoryKv, TTL, lease_key};
use realtime::partition::Parent;
use realtime::protocol::{Change, ChangeOp, StatusReason};
use realtime::replication::{ReplicationSource, SourceDeps, SourceSettings};
use realtime::slots::{Replicator, SlotError};
use realtime::source::{ChangeSource, ChannelEvent, JoinInfo, JoinRefusal, WatchHandle};
use tokio::sync::mpsc;
use tokio::time::Instant;

const REL2: u32 = 16_500;
const ROOT: u32 = 16_600;
const LEAF: u32 = 16_601;

fn table(t: &str) -> TableChannel {
    table_in("app", t)
}

fn table_in(db: &str, t: &str) -> TableChannel {
    TableChannel {
        database_id: DB,
        branch_id: BRANCH,
        pg_database: db.into(),
        schema: "public".into(),
        table: t.into(),
        bucket: None,
    }
}

fn change_json(rel: u32, t: &str, op: &str, keys: &[&str]) -> String {
    if op == "c" {
        return format!(r#"{{"rel":{rel},"s":"public","t":"{t}","op":"c"}}"#);
    }
    let k: Vec<String> = keys.iter().map(|k| format!(r#"["{k}"]"#)).collect();
    format!(
        r#"{{"rel":{rel},"s":"public","t":"{t}","op":"{op}","k":[{}]}}"#,
        k.join(",")
    )
}

fn msg(lsn: u64, xid: u64, changes: &[String]) -> String {
    format!(
        r#"{{"v":1,"lsn":"{:X}/{:X}","xid":{xid},"changes":[{}]}}"#,
        lsn >> 32,
        lsn & 0xffff_ffff,
        changes.join(",")
    )
}

fn insert(lsn: u64, xid: u64, rel: u32, keys: &[&str]) -> String {
    msg(lsn, xid, &[change_json(rel, "items", "i", keys)])
}

fn keyed(op: ChangeOp, keys: &[&str]) -> ChannelEvent {
    ChannelEvent::Rows(vec![Change {
        op,
        keys: Some(keys.iter().map(|k| ks(k)).collect()),
    }])
}

fn changed() -> ChannelEvent {
    ChannelEvent::Rows(vec![Change {
        op: ChangeOp::Changed,
        keys: None,
    }])
}

fn live() -> ChannelEvent {
    ChannelEvent::Status {
        live: true,
        reason: None,
    }
}

fn paused(reason: StatusReason) -> ChannelEvent {
    ChannelEvent::Status {
        live: false,
        reason: Some(reason),
    }
}

fn is_resync(ev: &ChannelEvent) -> bool {
    matches!(ev, ChannelEvent::Resync { .. })
}

/// Events until one matches `pred` (included), or `Err` with what came within `within`.
async fn until(
    rx: &mut mpsc::Receiver<ChannelEvent>,
    within: Duration,
    pred: impl Fn(&ChannelEvent) -> bool,
) -> Result<Vec<ChannelEvent>, Vec<ChannelEvent>> {
    let deadline = Instant::now() + within;
    let mut seen = vec![];
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ev)) => {
                let hit = pred(&ev);
                seen.push(ev);
                if hit {
                    return Ok(seen);
                }
            }
            Ok(None) | Err(_) => return Err(seen),
        }
    }
}

/// Whether `cond` holds within `within` (polled every 10 ms).
async fn eventually(within: Duration, cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// One instance's source and its fakes; instances of one test share the bus and the KVs.
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
        Self {
            bus: MemoryBus::new(),
            leases: MemoryKv::new(TTL),
            slots: MemoryKv::new(TTL),
            checker: FakeChecker::new(),
        }
    }
    async fn instance(&self, id: &str) -> Inst {
        self.instance_with(id, SourceSettings::default()).await
    }
    async fn instance_with(&self, id: &str, settings: SourceSettings) -> Inst {
        let repl = FakeReplicator::new();
        let src = ReplicationSource::start(SourceDeps {
            instance_id: id.into(),
            bus: self.bus.clone(),
            leases: self.leases.clone(),
            slots: self.slots.clone(),
            checker: self.checker.clone() as Arc<dyn Checker>,
            replicator: repl.clone() as Arc<dyn Replicator>,
            settings,
        })
        .await
        .expect("the source starts");
        Inst { src, repl }
    }
}

struct Watching {
    handle: WatchHandle,
    info: JoinInfo,
    rx: mpsc::Receiver<ChannelEvent>,
}

async fn watch(
    inst: &Inst,
    ch: &TableChannel,
    who: realtime::source::CheckIdentity,
) -> Result<Watching, JoinRefusal> {
    let (tx, rx) = mpsc::channel(256);
    let (handle, info) = inst.src.watch(ch, &who, tx).await?;
    Ok(Watching { handle, info, rx })
}

/// Watches `items` as `billing` (bypass) and waits until the slot streams.
async fn streaming(w: &World, inst: &Inst) -> Watching {
    w.checker.insert_rows(REL, &["1", "2", "3", "4", "5"]);
    let joined = watch(inst, &table("items"), role("billing")).await;
    assert!(joined.is_ok(), "joined: {:?}", joined.as_ref().err());
    let mut watching = joined.unwrap();
    let seen = until(&mut watching.rx, Duration::from_secs(2), |e| *e == live()).await;
    assert!(seen.is_ok(), "the slot streams: {seen:?}");
    watching
}

#[tokio::test(start_paused = true)]
async fn a_join_resolves_probes_and_creates_a_slot() {
    let w = World::new();
    let a = w.instance("a").await;
    let joined = watch(&a, &table("items"), role("billing")).await;
    assert!(joined.is_ok(), "{:?}", joined.as_ref().err());
    let mut j = joined.unwrap();
    assert_eq!(
        j.info,
        JoinInfo {
            pk: Some(vec!["id".into()]),
            live: false,
            reason: Some(StatusReason::Waiting),
            db_oid: DB_OID,
            rel_oid: REL,
        },
        "the slot isn't streaming yet when the join answers"
    );
    let seen = until(&mut j.rx, Duration::from_secs(2), |e| *e == live()).await;
    assert!(seen.is_ok(), "{seen:?}");
    assert!(
        seen.unwrap().iter().any(is_resync),
        "a slot made after the join: resync, then live (R8)"
    );
    assert_eq!(a.repl.starts(), 1);
    assert_eq!(
        a.repl.start_targets()[0],
        realtime::slots::SlotTarget {
            branch_id: BRANCH,
            pg_database: "app".into(),
            db_oid: DB_OID,
        }
    );
    let slot = a.repl.session().unwrap().slot.clone();
    assert!(slot.starts_with("rt_"), "{slot}");
    assert!(
        w.leases
            .get(&lease_key(BRANCH, DB_OID))
            .await
            .unwrap()
            .is_some(),
        "the instance holds the lease"
    );
    let _ = j.handle;
}

#[tokio::test(start_paused = true)]
async fn refused_joins() {
    let w = World::new();
    let a = w.instance("a").await;
    let missing = watch(&a, &table("nope"), role("billing")).await;
    assert_eq!(missing.err(), Some(JoinRefusal::NotFound));

    w.checker.set_resolve(
        "public",
        "broken",
        Err(CheckError::Unavailable("compute".into())),
    );
    assert_eq!(
        watch(&a, &table("broken"), role("billing")).await.err(),
        Some(JoinRefusal::Unavailable)
    );

    // W2: no USAGE on the schema, no SELECT: the probe's real read fails.
    w.checker.fail_probe(Some("nosy"), true);
    assert_eq!(
        watch(&a, &table("items"), role("nosy")).await.err(),
        Some(JoinRefusal::Forbidden)
    );
    w.checker
        .fail_checks(Some("admin"), Some(CheckError::OwnerLevel));
    assert_eq!(
        watch(&a, &table("items"), role("admin")).await.err(),
        Some(JoinRefusal::Forbidden),
        "R3: an owner-level role"
    );
    assert_eq!(a.repl.starts(), 0, "nothing joined, no slot");
}

#[tokio::test(start_paused = true)]
async fn views_join_as_not_a_table_and_never_get_a_slot() {
    let w = World::new();
    let mut view = resolved(relation(REL2, "v"));
    view.relkind = 'v';
    w.checker.set_resolve("public", "v", Ok(Some(view)));
    let a = w.instance("a").await;
    let j = watch(&a, &table("v"), role("billing")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let j = j.unwrap();
    assert_eq!(
        (j.info.live, j.info.reason),
        (false, Some(StatusReason::NotATable))
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(a.repl.starts(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_read_only_compute_never_tries_a_slot() {
    let w = World::new();
    w.checker.update_compute(|f| f.read_only = true);
    let a = w.instance("a").await;
    let j = watch(&a, &table("items"), role("billing")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let j = j.unwrap();
    assert_eq!(
        (j.info.live, j.info.reason),
        (false, Some(StatusReason::ReadOnly)),
        "S26, R22"
    );
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(a.repl.starts(), 0);
}

#[tokio::test(start_paused = true)]
async fn changes_reach_the_watcher_and_are_confirmed_after_publish() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut j = streaming(&w, &a).await;
    let session = a.repl.session().unwrap();
    session.data(0x100, &insert(0x100, 7, REL, &["1", "9"]));
    let seen = until(&mut j.rx, Duration::from_secs(1), |e| {
        matches!(e, ChannelEvent::Rows(_))
    })
    .await;
    assert_eq!(
        seen.map(|s| s.last().cloned()),
        Ok(Some(keyed(ChangeOp::Insert, &["1"]))),
        "9 doesn't exist when checked"
    );
    assert!(
        eventually(Duration::from_secs(1), || session.last_confirm()
            == Some(0x100.into()))
        .await,
        "confirmed once published: {:?}",
        session.confirms()
    );
}

#[tokio::test(start_paused = true)]
async fn an_idle_slot_confirms_keepalives() {
    let w = World::new();
    let a = w.instance("a").await;
    let _j = streaming(&w, &a).await;
    let session = a.repl.session().unwrap();
    session.keepalive(0x9000);
    assert!(
        eventually(Duration::from_secs(1), || session.last_confirm()
            == Some(0x9000.into()))
        .await,
        "W8: a quiet watched database still advances: {:?}",
        session.confirms()
    );
}

#[tokio::test(start_paused = true)]
async fn nothing_is_confirmed_past_an_unpublished_window_and_the_slot_goes_after_30_s() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut j = streaming(&w, &a).await;
    let session = a.repl.session().unwrap();
    w.bus.set_down(true);
    let started = Instant::now();
    session.data(0x200, &insert(0x200, 8, REL, &["1"]));
    session.keepalive(0x300);
    let seen = until(&mut j.rx, Duration::from_secs(35), |e| {
        *e == paused(StatusReason::Unavailable)
    })
    .await;
    assert!(seen.is_ok(), "R17: watchers are told: {seen:?}");
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(29) && took <= Duration::from_secs(32),
        "{took:?}"
    );
    assert!(session.closed(), "the slot (its connection) is dropped");
    assert!(
        session.confirms().iter().all(|c| u64::from(*c) < 0x200),
        "never past the unpublished transaction: {:?}",
        session.confirms()
    );
}

#[tokio::test(start_paused = true)]
async fn the_lag_watchdog_drops_and_recreates_the_slot() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut j = streaming(&w, &a).await;
    let session = a.repl.session().unwrap();
    w.bus.set_down(true);
    session.data(0x100, &insert(0x100, 8, REL, &["1"]));
    session.keepalive(0x100 + (65 << 20));
    assert!(
        eventually(Duration::from_secs(2), || session.closed()).await,
        "64 MB behind with a transaction in flight: dropped at once, not after 30 s"
    );
    w.bus.set_down(false);
    let seen = until(&mut j.rx, Duration::from_secs(40), is_resync).await;
    assert!(seen.is_ok(), "a fresh slot means a gap: {seen:?}");
    assert!(eventually(Duration::from_secs(40), || a.repl.starts() == 2).await);
}

#[tokio::test(start_paused = true)]
async fn a_blocked_creation_reports_waiting_closes_and_retries() {
    let w = World::new();
    let a = w.instance("a").await;
    a.repl.plan(StartPlan::Block);
    w.checker.insert_rows(REL, &["1"]);
    let started = Instant::now();
    let j = watch(&a, &table("items"), role("billing")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let mut j = j.unwrap();
    assert_eq!(j.info.reason, Some(StatusReason::Waiting));
    assert!(
        eventually(Duration::from_secs(11), || a.repl.abandoned() == 1).await,
        "W7: the creation is given up after 10 s"
    );
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(10) && took < Duration::from_millis(10_500),
        "{took:?}"
    );
    assert_eq!(a.repl.starts(), 1);
    let seen = until(&mut j.rx, Duration::from_secs(40), |e| *e == live()).await;
    assert!(seen.is_ok(), "{seen:?}");
    assert!(seen.unwrap().iter().any(is_resync));
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(40),
        "retried 30 s after giving up: {took:?}"
    );
    assert_eq!(a.repl.starts(), 2);
}

#[tokio::test(start_paused = true)]
async fn slot_errors_are_reported_and_retried() {
    for (err, reason) in [
        (SlotError::NoConnect, StatusReason::NoConnect),
        (SlotError::Unsupported, StatusReason::Unsupported),
        (SlotError::NoSlot, StatusReason::NoSlot),
    ] {
        let w = World::new();
        let a = w.instance("a").await;
        a.repl.plan(StartPlan::Fail(err.clone()));
        let j = watch(&a, &table("items"), role("billing")).await;
        assert!(j.is_ok(), "{err:?}: {:?}", j.as_ref().err());
        let mut j = j.unwrap();
        let seen = until(&mut j.rx, Duration::from_secs(2), |e| *e == paused(reason)).await;
        assert!(seen.is_ok(), "{err:?}: {seen:?}");
        let seen = until(&mut j.rx, Duration::from_secs(35), |e| *e == live()).await;
        assert!(seen.is_ok(), "{err:?} retried: {seen:?}");
        assert_eq!(a.repl.starts(), 2, "{err:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_slot_cap_spans_the_computes_databases() {
    let w = World::new();
    // 2 slots on the compute: realtime may hold 1.
    w.checker.update_compute(|f| f.max_replication_slots = 2);
    w.checker.set_db_oid("other", 16_385);
    let a = w.instance("a").await;
    let mut first = streaming(&w, &a).await;
    let second = watch(&a, &table_in("other", "items"), role("billing")).await;
    assert!(second.is_ok(), "{:?}", second.as_ref().err());
    let mut second = second.unwrap();
    let seen = until(&mut second.rx, Duration::from_secs(2), |e| {
        *e == paused(StatusReason::NoSlot)
    })
    .await;
    assert!(
        seen.is_ok() || second.info.reason == Some(StatusReason::NoSlot),
        "{seen:?}"
    );
    assert_eq!(a.repl.starts(), 1, "S5: no free index, no slot");
    let _ = first.rx.try_recv();
}

#[tokio::test(start_paused = true)]
async fn the_rt_count_is_a_second_check() {
    let w = World::new();
    // Ten rt_ slots already on the compute (taken outside the index): over the cap of 10.
    w.checker.update_compute(|f| f.rt_slots = 10);
    let a = w.instance("a").await;
    let j = watch(&a, &table("items"), role("billing")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let mut j = j.unwrap();
    let seen = until(&mut j.rx, Duration::from_secs(2), |e| {
        *e == paused(StatusReason::NoSlot)
    })
    .await;
    assert!(seen.is_ok(), "{seen:?}");
    assert_eq!(a.repl.starts(), 0);
    assert_eq!(
        realtime::slots::take_index(w.slots.as_ref(), BRANCH, 1, "probe")
            .await
            .map(|i| i.is_some()),
        Ok(true),
        "the index was given back"
    );
}

#[tokio::test(start_paused = true)]
async fn a_schema_change_reprobes_and_re_resolves_before_releasing_keys() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut j = streaming(&w, &a).await;
    let session = a.repl.session().unwrap();
    // DROP TABLE items; CREATE TABLE items (…) in one transaction: a new OID, and the version
    // bump in the same transaction.
    w.checker.set_resolve(
        "public",
        "items",
        Ok(Some(resolved(relation(REL2, "items")))),
    );
    w.checker.insert_rows(REL2, &["5"]);
    w.checker.clear_calls();
    session.data(
        0x400,
        &msg(
            0x400,
            9,
            &[
                change_json(REL, "items", "i", &["1"]),
                change_json(VERSION_OID, "schema_version", "u", &["t"]),
            ],
        ),
    );
    let seen = until(&mut j.rx, Duration::from_secs(2), |e| {
        *e == changed() || is_resync(e)
    })
    .await;
    assert!(seen.is_ok(), "the window becomes a refetch: {seen:?}");
    assert!(
        !seen
            .unwrap()
            .iter()
            .any(|e| matches!(e, ChannelEvent::Rows(c) if c.iter().any(|c| c.keys.is_some()))),
        "no key of the old table's window is released"
    );
    let calls = w.checker.calls();
    let resolved_at = calls.iter().position(|c| matches!(c, Call::Resolve { .. }));
    let first_check = calls.iter().position(|c| matches!(c, Call::Check { .. }));
    assert!(resolved_at.is_some(), "re-resolved: {calls:?}");
    assert!(
        first_check.is_none_or(|c| resolved_at.unwrap() < c),
        "before any check of that window: {calls:?}"
    );
    // The channel now follows the new OID.
    session.data(0x500, &insert(0x500, 10, REL2, &["5"]));
    let seen = until(&mut j.rx, Duration::from_secs(2), |e| {
        matches!(e, ChannelEvent::Rows(_))
    })
    .await;
    assert_eq!(
        seen.map(|s| s.last().cloned()),
        Ok(Some(keyed(ChangeOp::Insert, &["5"])))
    );
}

#[tokio::test(start_paused = true)]
async fn an_all_transaction_is_a_schema_change() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut j = streaming(&w, &a).await;
    w.checker.clear_calls();
    a.repl
        .session()
        .unwrap()
        .data(0x600, r#"{"v":1,"lsn":"0/600","xid":11,"all":true}"#);
    let seen = until(&mut j.rx, Duration::from_secs(2), |e| *e == changed()).await;
    assert!(seen.is_ok(), "{seen:?}");
    assert!(
        w.checker
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Resolve { .. })),
        "W10: all hides a schema_version change"
    );
}

#[tokio::test(start_paused = true)]
async fn a_partition_change_reaches_the_roots_watchers_checked_against_the_root() {
    let w = World::new();
    w.checker
        .set_resolve("public", "root", Ok(Some(resolved(relation(ROOT, "root")))));
    w.checker.set_parents(
        LEAF,
        vec![Parent {
            oid: ROOT,
            keyed: true,
        }],
    );
    w.checker.insert_rows(ROOT, &["3"]);
    let a = w.instance("a").await;
    let j = watch(&a, &table("root"), role("billing")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let mut j = j.unwrap();
    assert!(
        until(&mut j.rx, Duration::from_secs(2), |e| *e == live())
            .await
            .is_ok()
    );
    a.repl.session().unwrap().data(
        0x700,
        &msg(0x700, 12, &[change_json(LEAF, "leaf", "i", &["3"])]),
    );
    let seen = until(&mut j.rx, Duration::from_secs(2), |e| {
        matches!(e, ChannelEvent::Rows(_))
    })
    .await;
    assert_eq!(
        seen.map(|s| s.last().cloned()),
        Ok(Some(keyed(ChangeOp::Insert, &["3"])))
    );
    assert!(
        w.checker
            .checks()
            .iter()
            .all(|c| matches!(c, Call::Check { rel, .. } if *rel == ROOT)),
        "the root's own RLS applies: {:?}",
        w.checker.checks()
    );
}

#[tokio::test(start_paused = true)]
async fn windows_are_delivered_in_order() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut j = streaming(&w, &a).await;
    let session = a.repl.session().unwrap();
    // The first window waits ~500 ms for its xid; the second is visible at once.
    w.checker.invisible_for(20, 10);
    session.data(0x800, &insert(0x800, 20, REL, &["1"]));
    tokio::time::sleep(Duration::from_millis(150)).await;
    session.data(0x900, &insert(0x900, 21, REL, &["2"]));
    let seen = until(&mut j.rx, Duration::from_secs(3), |e| {
        *e == keyed(ChangeOp::Insert, &["2"])
    })
    .await;
    let rows: Vec<ChannelEvent> = seen
        .unwrap_or_else(|s| s)
        .into_iter()
        .filter(|e| matches!(e, ChannelEvent::Rows(_)))
        .collect();
    assert_eq!(
        rows,
        vec![
            keyed(ChangeOp::Insert, &["1"]),
            keyed(ChangeOp::Insert, &["2"])
        ],
        "S30: in order per channel"
    );
}

#[tokio::test(start_paused = true)]
async fn compute_stopping_closes_at_once_and_waits_for_the_compute() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut j = streaming(&w, &a).await;
    let session = a.repl.session().unwrap();
    w.checker
        .set_compute(Err(CheckError::Unavailable("connection route 409".into())));
    let started = Instant::now();
    w.bus
        .publish(
            &format!("{COMPUTE_STOPPING}.{BRANCH}"),
            None,
            serde_json::to_vec(&ComputeStopping { branch_id: BRANCH })
                .unwrap()
                .into(),
        )
        .await
        .unwrap();
    assert!(
        eventually(Duration::from_millis(200), || session.closed()).await,
        "P9: the replication connection closes at once"
    );
    let seen = until(&mut j.rx, Duration::from_secs(1), |e| {
        *e == paused(StatusReason::ComputeStopping)
    })
    .await;
    assert!(seen.is_ok(), "{seen:?}");
    assert!(
        eventually(Duration::from_secs(1), || {
            futures_util::FutureExt::now_or_never(w.leases.get(&lease_key(BRANCH, DB_OID)))
                .is_some_and(|r| matches!(r, Ok(None)))
        })
        .await,
        "the lease is released"
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        a.repl.starts(),
        1,
        "S25: no slot while the compute isn't back"
    );
    w.checker.set_compute(Ok(realtime::check::ComputeFacts {
        read_only: false,
        max_replication_slots: 20,
        rt_slots: 0,
        db_oid: DB_OID,
        version_oid: Some(VERSION_OID),
    }));
    let seen = until(&mut j.rx, Duration::from_secs(40), |e| *e == live()).await;
    assert!(seen.is_ok(), "{seen:?}");
    assert!(seen.unwrap().iter().any(is_resync));
    assert_eq!(a.repl.starts(), 2);
    assert!(started.elapsed() >= Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn a_compute_restart_is_a_resync() {
    let w = World::new();
    let a = w.instance("a").await;
    let mut j = streaming(&w, &a).await;
    a.repl.session().unwrap().end();
    let seen = until(&mut j.rx, Duration::from_secs(40), |e| *e == live()).await;
    assert!(seen.is_ok(), "{seen:?}");
    assert!(
        seen.unwrap().iter().any(is_resync),
        "the slot is gone: a gap"
    );
    assert_eq!(a.repl.starts(), 2);
}

#[tokio::test(start_paused = true)]
async fn one_holder_per_database_and_takeover_after_a_crash() {
    let w = World::new();
    let a = w.instance("a").await;
    let _ja = streaming(&w, &a).await;
    let b = w.instance("b").await;
    let jb = watch(&b, &table("items"), role("reader")).await;
    assert!(jb.is_ok(), "{:?}", jb.as_ref().err());
    let mut jb = jb.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        b.repl.starts(),
        0,
        "A holds the lease: B never creates a slot"
    );
    a.repl
        .session()
        .unwrap()
        .data(0x100, &insert(0x100, 7, REL, &["2"]));
    let seen = until(&mut jb.rx, Duration::from_secs(2), |e| {
        matches!(e, ChannelEvent::Rows(_))
    })
    .await;
    assert_eq!(
        seen.map(|s| s.last().cloned()),
        Ok(Some(keyed(ChangeOp::Insert, &["2"]))),
        "B's viewer gets A's decoded keys, filtered on B"
    );
    let crashed = Instant::now();
    a.src.abort();
    let seen = until(&mut jb.rx, Duration::from_secs(15), |e| *e == live()).await;
    assert!(seen.is_ok(), "{seen:?}");
    assert!(seen.unwrap().iter().any(is_resync));
    assert_eq!(b.repl.starts(), 1);
    assert!(
        crashed.elapsed() <= Duration::from_secs(11),
        "takeover within ~10 s: {:?}",
        crashed.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn the_holder_stops_30_s_after_the_last_watcher() {
    let w = World::new();
    let a = w.instance("a").await;
    let ja = streaming(&w, &a).await;
    let b = w.instance("b").await;
    let jb = watch(&b, &table("items"), role("reader")).await;
    assert!(jb.is_ok(), "{:?}", jb.as_ref().err());
    let jb = jb.unwrap();
    drop(ja);
    let session = a.repl.session().unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(!session.closed(), "B's heartbeats keep A decoding");
    let left = Instant::now();
    drop(jb);
    assert!(eventually(Duration::from_secs(40), || session.closed()).await);
    let took = left.elapsed();
    assert!(
        took >= Duration::from_secs(24) && took <= Duration::from_secs(37),
        "30 s after the last heartbeat (sent every 5 s, so 25–30 s after B left): {took:?}"
    );
    assert!(
        eventually(Duration::from_secs(1), || {
            futures_util::FutureExt::now_or_never(w.leases.get(&lease_key(BRANCH, DB_OID)))
                .is_some_and(|r| matches!(r, Ok(None)))
        })
        .await,
        "released"
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_releases_and_closes() {
    let w = World::new();
    let a = w.instance("a").await;
    let _j = streaming(&w, &a).await;
    let session = a.repl.session().unwrap();
    a.src.shutdown().await;
    assert!(
        session.closed(),
        "the temporary slot goes with the connection"
    );
    assert_eq!(
        w.leases.get(&lease_key(BRANCH, DB_OID)).await,
        Ok(None),
        "CAS release at once, not after expiry"
    );
    assert_eq!(
        realtime::slots::take_index(w.slots.as_ref(), BRANCH, 1, "probe")
            .await
            .map(|i| i.map(|i| i.n())),
        Ok(Some(0)),
        "the slot index is free"
    );
}

#[tokio::test(start_paused = true)]
async fn a_fenced_holder_stops_decoding_by_its_deadline() {
    let w = World::new();
    let a = w.instance("a").await;
    let _j = streaming(&w, &a).await;
    let session = a.repl.session().unwrap();
    let down = Instant::now();
    w.leases.set_down(true);
    assert!(eventually(Duration::from_secs(9), || session.closed()).await);
    let took = down.elapsed();
    assert!(
        took <= Duration::from_secs(7) + Duration::from_millis(100),
        "W12: by the local deadline (last renewal + 10 s − 3 s): {took:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn view_and_visible_reach_the_filter() {
    let w = World::new();
    w.checker.insert_rows(REL, &["1", "2"]);
    w.checker.hide(Some("reader"), &["2"]);
    let a = w.instance("a").await;
    let j = watch(&a, &table("items"), role("reader")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let mut j = j.unwrap();
    assert_eq!(
        a.src.visible(&j.handle, &[ks("1"), ks("2")]).await,
        Ok(vec![true, false]),
        "R15: the setter's own check, never the fake's `true`"
    );
    assert!(
        until(&mut j.rx, Duration::from_secs(2), |e| *e == live())
            .await
            .is_ok()
    );
    // 1 is visible and on screen; 2 is hidden from the reader and the client guessed it.
    a.src.view(&j.handle, vec![ks("1"), ks("2")]).await;
    w.checker.delete_rows(REL, &["1", "2"]);
    a.repl.session().unwrap().data(
        0x100,
        &msg(
            0x100,
            7,
            &[change_json(REL, "items", "d", &["1", "2", "3"])],
        ),
    );
    let seen = until(&mut j.rx, Duration::from_secs(2), |e| {
        matches!(e, ChannelEvent::Rows(_))
    })
    .await;
    assert_eq!(
        seen.map(|s| s.last().cloned()),
        Ok(Some(keyed(ChangeOp::Delete, &["1"]))),
        "R19 as ruled: keyed only for a key on screen that was seen visible; a guessed key \
         never learns that a hidden row existed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_guessed_view_key_never_learns_of_a_hidden_update() {
    let w = World::new();
    w.checker.insert_rows(REL, &["1", "2"]);
    w.checker.hide(Some("reader"), &["2"]);
    let a = w.instance("a").await;
    let j = watch(&a, &table("items"), role("reader")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let mut j = j.unwrap();
    assert!(
        until(&mut j.rx, Duration::from_secs(2), |e| *e == live())
            .await
            .is_ok()
    );
    a.src.view(&j.handle, vec![ks("1"), ks("2")]).await;
    // 1 was visible at `view` and an update hides it (W26); 2 was never visible.
    w.checker.hide(Some("reader"), &["1"]);
    a.repl.session().unwrap().data(
        0x100,
        &msg(0x100, 7, &[change_json(REL, "items", "u", &["1", "2"])]),
    );
    let seen = until(&mut j.rx, Duration::from_secs(2), |e| {
        matches!(e, ChannelEvent::Rows(_))
    })
    .await;
    assert_eq!(
        seen.map(|s| s.last().cloned()),
        Ok(Some(keyed(ChangeOp::Delete, &["1"])))
    );
}

#[tokio::test(start_paused = true)]
async fn a_renewed_token_reaches_the_owner_check() {
    let w = World::new();
    w.checker.insert_rows(REL, &["1"]);
    let a = w.instance("a").await;
    let j = watch(&a, &table("items"), owner("token-at-join")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let mut j = j.unwrap();
    assert!(
        until(&mut j.rx, Duration::from_secs(2), |e| *e == live())
            .await
            .is_ok()
    );
    a.src
        .renew(&j.handle, std::sync::Arc::from("token-renewed"))
        .await;
    w.checker.clear_calls();
    let before = w.checker.check_tokens(None).len();
    a.repl
        .session()
        .unwrap()
        .data(0x100, &insert(0x100, 7, REL, &["1"]));
    let seen = until(&mut j.rx, Duration::from_secs(2), |e| {
        matches!(e, ChannelEvent::Rows(_))
    })
    .await;
    assert!(seen.is_ok(), "{seen:?}");
    let after: Vec<String> = w.checker.check_tokens(None)[before..].to_vec();
    assert!(
        !after.is_empty() && after.iter().all(|t| t == "token-renewed"),
        "the owner's /visible calls carry the renewed token, not the join's (expired at exp): {after:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_window_flushes_the_databases_visibility_cache() {
    let w = World::new();
    w.checker.set_resolve(
        "public",
        "other",
        Ok(Some(resolved(relation(REL2, "other")))),
    );
    w.checker.insert_rows(REL, &["1"]);
    let a = w.instance("a").await;
    let j = watch(&a, &table("items"), role("reader")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let mut j = j.unwrap();
    let _other = watch(&a, &table("other"), role("reader")).await.unwrap();
    assert!(
        until(&mut j.rx, Duration::from_secs(2), |e| *e == live())
            .await
            .is_ok()
    );
    // A change in another table of the database (a policy may read it): R12. Its window
    // closes 100 ms after it's decoded; a visibility answer cached 50 ms in would otherwise live
    // until 150 ms.
    a.repl.session().unwrap().data(
        0x100,
        &msg(0x100, 7, &[change_json(REL2, "other", "i", &["9"])]),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = a.src.visible(&j.handle, &[ks("1")]).await;
    let cached = w.checker.checks_of(Some("reader"));
    assert!(
        eventually(Duration::from_secs(1), || w
            .checker
            .checks_of(Some("reader"))
            > cached)
        .await,
        "the other relation's window is checked"
    );
    let after_window = w.checker.checks_of(Some("reader"));
    let _ = a.src.visible(&j.handle, &[ks("1")]).await;
    assert_eq!(
        w.checker.checks_of(Some("reader")),
        after_window + 1,
        "the database's cache was flushed by a window on another relation"
    );
}

#[tokio::test(start_paused = true)]
async fn reprobe_re_resolves_the_table() {
    let w = World::new();
    let a = w.instance("a").await;
    let j = watch(&a, &table("items"), role("billing")).await;
    assert!(j.is_ok(), "{:?}", j.as_ref().err());
    let j = j.unwrap();
    w.checker.set_resolve(
        "public",
        "items",
        Ok(Some(resolved(relation(REL2, "items")))),
    );
    let info = a.src.reprobe(&j.handle).await;
    assert_eq!(
        info.map(|i| i.rel_oid),
        Ok(REL2),
        "a rename or recreate is picked up"
    );
    w.checker.fail_probe(Some("billing"), true);
    assert_eq!(
        a.src.reprobe(&j.handle).await.err(),
        Some(JoinRefusal::Forbidden)
    );
    w.checker.set_resolve("public", "items", Ok(None));
    w.checker.fail_probe(Some("billing"), false);
    assert_eq!(
        a.src.reprobe(&j.handle).await.err(),
        Some(JoinRefusal::NotFound)
    );
}

#[tokio::test(start_paused = true)]
async fn dropping_the_handle_unwatches() {
    let w = World::new();
    let a = w.instance("a").await;
    let j = streaming(&w, &a).await;
    let Watching { handle, mut rx, .. } = j;
    drop(handle);
    a.repl
        .session()
        .unwrap()
        .data(0x100, &insert(0x100, 7, REL, &["1"]));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|e| matches!(e, ChannelEvent::Rows(_))),
        "no rows after unwatching"
    );
    let _ = w.slots.get("x").await;
}

/// Object storage m17: two storage channels of one database (two buckets of
/// `storage.objects`) each have their own filter: every window reaches both, each releases only
/// its own bucket's objects, and one bucket's last watcher leaving keeps the other streaming.
#[tokio::test(start_paused = true)]
async fn storage_channels_of_two_buckets_on_one_relation_get_their_own_keys() {
    const A: uuid::Uuid = uuid::Uuid::from_u128(0xa);
    const B: uuid::Uuid = uuid::Uuid::from_u128(0xb);
    const O1: &str = "00000000-0000-4000-8000-000000000001";
    const O2: &str = "00000000-0000-4000-8000-000000000002";
    let w = World::new();
    let objects = chelabase_query::realtime::Relation {
        oid: REL,
        schema: "storage".into(),
        name: "objects".into(),
        key: vec![col("id", "uuid")],
    };
    w.checker.set_resolve("storage", "objects", Ok(Some(resolved(objects))));
    w.checker.insert_rows(REL, &[O1, O2]);
    w.checker.place(&[O1], A);
    w.checker.place(&[O2], B);
    let bucket = |b| TableChannel {
        schema: "storage".into(),
        table: "objects".into(),
        bucket: Some(b),
        ..table("objects")
    };
    let a = w.instance("a").await;
    let mut ja = watch(&a, &bucket(A), owner("member-a")).await.unwrap();
    let mut jb = watch(&a, &bucket(B), owner("member-b")).await.unwrap();
    for j in [&mut ja, &mut jb] {
        let seen = until(&mut j.rx, Duration::from_secs(2), |e| *e == live()).await;
        assert!(seen.is_ok(), "the slot streams: {seen:?}");
    }
    let session = a.repl.session().unwrap();
    session.data(0x100, &insert(0x100, 7, REL, &[O1, O2]));
    let rows = |e: &ChannelEvent| matches!(e, ChannelEvent::Rows(_));
    let got_a = until(&mut ja.rx, Duration::from_secs(1), rows).await;
    let got_b = until(&mut jb.rx, Duration::from_secs(1), rows).await;
    assert_eq!(got_a.map(|s| s.last().cloned()), Ok(Some(keyed(ChangeOp::Insert, &[O1]))));
    assert_eq!(got_b.map(|s| s.last().cloned()), Ok(Some(keyed(ChangeOp::Insert, &[O2]))));
    drop(ja);
    session.data(0x200, &insert(0x200, 8, REL, &[O2]));
    let got_b = until(&mut jb.rx, Duration::from_secs(1), rows).await;
    assert_eq!(got_b.map(|s| s.last().cloned()), Ok(Some(keyed(ChangeOp::Insert, &[O2]))));
}
