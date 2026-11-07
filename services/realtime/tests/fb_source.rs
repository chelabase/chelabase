//! RT1 final fix round (batch B, T8 items) on the fakes and tokio's paused clock: a lost slot
//! index ends its slot, the `rt_%` count is read after the index is taken (S5), an unreadable
//! message holds confirmation back like any transaction, a status dropped on a full sink owes the
//! viewer a resync, and `view`/`visible` never hold the table's filter while a check runs.

mod fakes;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chelabase_nats::rt::ROWS;
use fakes::*;
use realtime::bus::{Bus, MemoryBus};
use realtime::channel::TableChannel;
use realtime::check::Checker;
use realtime::lease::{KvError, KvStore, KvWatch, MemoryKv, TTL};
use realtime::protocol::StatusReason;
use realtime::replication::{ReplicationSource, SourceDeps, SourceSettings};
use realtime::slots::{Replicator, index_key, take_index};
use realtime::source::{ChangeSource, ChannelEvent, WatchHandle};
use tokio::sync::mpsc;
use tokio::time::Instant;

fn items() -> TableChannel {
    TableChannel {
        database_id: DB,
        branch_id: BRANCH,
        pg_database: "app".into(),
        schema: "public".into(),
        table: "items".into(),
        bucket: None,
    }
}

fn insert(lsn: u64, xid: u64, keys: &[&str]) -> String {
    let k: Vec<String> = keys.iter().map(|k| format!(r#"["{k}"]"#)).collect();
    format!(
        r#"{{"v":1,"lsn":"{:X}/{:X}","xid":{xid},"changes":[{{"rel":{REL},"s":"public","t":"items","op":"i","k":[{}]}}]}}"#,
        lsn >> 32,
        lsn & 0xffff_ffff,
        k.join(",")
    )
}

fn live() -> ChannelEvent {
    ChannelEvent::Status {
        live: true,
        reason: None,
    }
}

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

struct World {
    bus: Arc<MemoryBus>,
    leases: Arc<MemoryKv>,
    slots: Arc<MemoryKv>,
    checker: Arc<FakeChecker>,
    repl: Arc<FakeReplicator>,
}

impl World {
    fn new() -> Self {
        Self {
            bus: MemoryBus::new(),
            leases: MemoryKv::new(TTL),
            slots: MemoryKv::new(TTL),
            checker: FakeChecker::new(),
            repl: FakeReplicator::new(),
        }
    }
    async fn source_with(&self, slots: Arc<dyn KvStore>) -> Arc<ReplicationSource> {
        ReplicationSource::start(SourceDeps {
            instance_id: "a".into(),
            bus: self.bus.clone(),
            leases: self.leases.clone(),
            slots,
            checker: self.checker.clone() as Arc<dyn Checker>,
            replicator: self.repl.clone() as Arc<dyn Replicator>,
            settings: SourceSettings::default(),
        })
        .await
        .unwrap()
    }
    async fn source(&self) -> Arc<ReplicationSource> {
        self.source_with(self.slots.clone()).await
    }
}

/// Watches `items` as `role` with a sink of `cap`, and waits until the slot streams.
async fn streaming(
    src: &ReplicationSource,
    who: &str,
    cap: usize,
) -> (
    WatchHandle,
    mpsc::Receiver<ChannelEvent>,
    mpsc::Sender<ChannelEvent>,
) {
    let (tx, mut rx) = mpsc::channel(cap);
    let (h, _) = src.watch(&items(), &role(who), tx.clone()).await.unwrap();
    let seen = until(&mut rx, Duration::from_secs(2), |e| *e == live()).await;
    assert!(seen.is_ok(), "the slot streams: {seen:?}");
    (h, rx, tx)
}

// --- a lost slot index ends the slot ---------------------------------------------------------

/// The holder's index expired and was taken by someone else (its renewal finds another
/// revision): the slot no longer counts against the cap, so it must end (its connection
/// closed) rather than stream on as the cap's eleventh; the other holder's index is untouched.
#[tokio::test(start_paused = true)]
async fn a_lost_slot_index_ends_the_slot() {
    let w = World::new();
    let src = w.source().await;
    w.checker.insert_rows(REL, &["1"]);
    let (_h, mut rx, _tx) = streaming(&src, "billing", 256).await;
    let session = w.repl.session().unwrap();
    let key = index_key(BRANCH, 0);
    let (_, rev) = w
        .slots
        .get(&key)
        .await
        .unwrap()
        .expect("the holder took index 0");
    w.slots.delete_expect(&key, rev).await.unwrap();
    w.slots
        .create(&key, Bytes::from_static(b"other"))
        .await
        .unwrap();
    assert!(
        eventually(Duration::from_secs(5), || session.closed()).await,
        "the slot outlived its index (the next renewal is ≤ 3 s away)"
    );
    assert_eq!(
        w.slots.get(&key).await.unwrap().map(|(v, _)| v),
        Some(Bytes::from_static(b"other")),
        "never deletes another holder's index"
    );
    // Watchers learn the slot went; a fresh slot comes with an index of its own.
    let seen = until(&mut rx, Duration::from_secs(40), |e| *e == live()).await;
    assert!(seen.is_ok(), "{seen:?}");
    assert!(
        w.slots.get(&index_key(BRANCH, 1)).await.unwrap().is_some(),
        "the new slot holds index 1"
    );
}

// --- the `rt_%` count after taking the index (S5) ------------------------------------------

/// The slots bucket, and on the first index taken for [`BRANCH`] another instance's slot
/// appears on the compute (the race S5's second check is for): `rt_%` jumps to the cap.
struct RacingSlots {
    inner: Arc<MemoryKv>,
    checker: Arc<FakeChecker>,
}

#[async_trait]
impl KvStore for RacingSlots {
    async fn create(&self, key: &str, value: Bytes) -> Result<u64, KvError> {
        let r = self.inner.create(key, value).await;
        if r.is_ok() && key.starts_with(&BRANCH.hyphenated().to_string()) {
            self.checker.update_compute(|f| f.rt_slots = 10);
        }
        r
    }
    async fn update(&self, key: &str, value: Bytes, revision: u64) -> Result<u64, KvError> {
        self.inner.update(key, value, revision).await
    }
    async fn delete_expect(&self, key: &str, revision: u64) -> Result<(), KvError> {
        self.inner.delete_expect(key, revision).await
    }
    async fn get(&self, key: &str) -> Result<Option<(Bytes, u64)>, KvError> {
        self.inner.get(key).await
    }
    async fn watch(&self, key: &str) -> Result<KvWatch, KvError> {
        self.inner.watch(key).await
    }
}

#[tokio::test(start_paused = true)]
async fn the_rt_count_is_read_after_the_index_is_taken() {
    let w = World::new();
    let slots = Arc::new(RacingSlots {
        inner: w.slots.clone(),
        checker: w.checker.clone(),
    });
    let src = w.source_with(slots).await;
    let (tx, mut rx) = mpsc::channel(256);
    let _h = src.watch(&items(), &role("billing"), tx).await.unwrap();
    let no_slot = ChannelEvent::Status {
        live: false,
        reason: Some(StatusReason::NoSlot),
    };
    let seen = until(&mut rx, Duration::from_secs(2), |e| *e == no_slot).await;
    assert!(
        seen.is_ok(),
        "S5: the count read after taking the index is at the cap: {seen:?}"
    );
    assert_eq!(w.repl.starts(), 0, "no slot past the cap");
    assert_eq!(
        take_index(w.slots.as_ref(), BRANCH, 1, "probe")
            .await
            .map(|i| i.is_some()),
        Ok(true),
        "the index was given back"
    );
}

// --- an unreadable message holds confirmation back --------------------------------------------

/// A message the decoder can't read is `all` (a refetch for everyone) and has no LSN of its
/// own: it must still block confirmation past its position (the walsender's `wal_end` for it)
/// until its window is published, as any transaction does.
#[tokio::test(start_paused = true)]
async fn an_unreadable_message_blocks_confirmation_past_it() {
    let w = World::new();
    let src = w.source().await;
    let (_h, _rx, _tx) = streaming(&src, "billing", 256).await;
    let session = w.repl.session().unwrap();
    w.bus.set_down(true);
    session.data(0x500, "this is not the plugin's JSON");
    session.keepalive(0x600);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        session.confirms().iter().all(|c| u64::from(*c) < 0x500),
        "confirmed past an unpublished (unreadable) message: {:?}",
        session.confirms()
    );
    // Once its window goes out, confirmation moves on.
    w.bus.set_down(false);
    session.keepalive(0x700);
    assert!(
        eventually(Duration::from_secs(5), || session
            .last_confirm()
            .is_some_and(|c| u64::from(c) >= 0x500))
        .await,
        "{:?}",
        session.confirms()
    );
}

// --- a status dropped on a full sink owes a resync ---------------------------------------------

/// Sinks are written with `try_send`. A status that finds the sink full is lost like a rows
/// event, so it sets the viewer's overflow flag the same way: the next delivery is
/// `resync {reason: "overflow"}`, not that window's rows.
#[tokio::test(start_paused = true)]
async fn a_status_dropped_on_a_full_sink_owes_a_resync() {
    let w = World::new();
    let src = w.source().await;
    w.checker.insert_rows(REL, &["1"]);
    let (_h, mut rx, filler) = streaming(&src, "billing", 2).await;
    let session = w.repl.session().unwrap();
    // The viewer stops reading: its sink is full.
    for _ in 0..2 {
        filler
            .try_send(ChannelEvent::Resync {
                reason: "filler".into(),
            })
            .unwrap();
    }
    // Another instance's holder reports the database unavailable: dropped on the full sink.
    let status = br#"{"type":"status","holder":"b","live":false,"reason":"unavailable"}"#;
    w.bus
        .publish(
            &format!("{ROWS}.{}.{DB_OID}.0", BRANCH.hyphenated()),
            None,
            Bytes::from_static(status),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let held = drain(&mut rx);
    assert_eq!(
        held,
        vec![
            ChannelEvent::Resync {
                reason: "filler".into()
            };
            2
        ],
        "the status didn't fit"
    );
    // The viewer reads again; the next window is owed as a resync.
    session.data(0x100, &insert(0x100, 7, &["1"]));
    let seen = until(&mut rx, Duration::from_secs(2), |e| {
        matches!(e, ChannelEvent::Rows(_) | ChannelEvent::Resync { .. })
    })
    .await;
    assert_eq!(
        seen.map(|s| s.last().cloned()),
        Ok(Some(ChannelEvent::Resync {
            reason: "overflow".into()
        })),
        "a lost status is a gap like a lost window"
    );
}

// --- `view`/`visible` never hold the filter during a check -----------------------------------

/// A `view` (and a presence `visible`) of one viewer checks the database; while its check runs,
/// the table's other work (another viewer's renewal here, the next window's delivery) isn't
/// queued behind it.
#[tokio::test(start_paused = true)]
async fn view_and_visible_never_hold_the_filter_during_a_check() {
    let w = World::new();
    let src = w.source().await;
    w.checker.insert_rows(REL, &["1", "2"]);
    let (first, _rx1, _tx1) = streaming(&src, "billing", 256).await;
    let (tx, _rx2) = mpsc::channel(256);
    let (second, _) = src.watch(&items(), &role("reader"), tx).await.unwrap();

    w.checker.hold();
    let viewing = {
        let src = src.clone();
        tokio::spawn(async move {
            src.view(&first, vec![ks("1")]).await;
            first
        })
    };
    let checker = w.checker.clone();
    assert!(
        eventually(Duration::from_secs(1), move || checker.in_flight() == 1).await,
        "the view's check is running"
    );
    let renewed = tokio::time::timeout(
        Duration::from_secs(1),
        src.renew(&second, Arc::from("renewed")),
    )
    .await;
    assert!(
        renewed.is_ok(),
        "a renewal waited behind another viewer's view check"
    );
    w.checker.release();
    let first = viewing.await.unwrap();

    w.checker.hold();
    let presence = {
        let src = src.clone();
        tokio::spawn(async move {
            let _ = src.visible(&first, &[ks("2")]).await;
            first
        })
    };
    let checker = w.checker.clone();
    assert!(
        eventually(Duration::from_secs(1), move || checker.in_flight() == 1).await,
        "the presence check is running"
    );
    let renewed = tokio::time::timeout(
        Duration::from_secs(1),
        src.renew(&second, Arc::from("renewed-again")),
    )
    .await;
    assert!(
        renewed.is_ok(),
        "a renewal waited behind another viewer's presence check"
    );
    w.checker.release();
    let _ = presence.await.unwrap();
}
