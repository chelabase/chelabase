//! RT1 T8 audit: a fenced holder publishes nothing once its local deadline has passed, even
//! when a publish was still in flight then (W12, S29), and an owner group is never checked
//! with the token of a member who left (T8 token renewal).

mod fakes;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chelabase_nats::rt::ROWS;
use fakes::*;
use realtime::bus::{Bus, BusError, BusStream, MemoryBus};
use realtime::channel::TableChannel;
use realtime::check::{CONCURRENCY, Checker, Limiter, VisibilityCache};
use realtime::filter::{FilterSettings, TableFilter};
use realtime::lease::{MemoryKv, TTL};
use realtime::merge::RelWindow;
use realtime::replication::{ReplicationSource, SourceDeps, SourceSettings};
use realtime::slots::Replicator;
use realtime::source::{ChangeSource, ChannelEvent};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;

/// The in-memory bus, except that a relation window's publish (`rows.<b>.<db>.<rel>`, rel ≠ 0)
/// waits while the gate is shut; `forwarded` counts the ones that got through.
struct GatedBus {
    inner: Arc<MemoryBus>,
    gate: Mutex<Option<Arc<Semaphore>>>,
    forwarded: AtomicUsize,
}

impl GatedBus {
    fn shut(&self) {
        *self.gate.lock().unwrap() = Some(Arc::new(Semaphore::new(0)));
    }
    fn open(&self) {
        if let Some(g) = self.gate.lock().unwrap().take() {
            g.add_permits(Semaphore::MAX_PERMITS / 2);
        }
    }
}

#[async_trait]
impl Bus for GatedBus {
    async fn publish(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<(), BusError> {
        let window = subject.starts_with(ROWS) && !subject.ends_with(".0");
        if window {
            let gate = self.gate.lock().unwrap().clone();
            if let Some(g) = gate {
                let _permit = g.acquire().await;
            }
        }
        self.inner.publish(subject, reply, payload).await?;
        if window {
            self.forwarded.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    async fn subscribe(&self, subject: &str) -> Result<BusStream, BusError> {
        self.inner.subscribe(subject).await
    }
    fn new_inbox(&self) -> String {
        self.inner.new_inbox()
    }
}

fn insert(lsn: u64, xid: u64, key: &str) -> String {
    format!(
        r#"{{"v":1,"lsn":"0/{lsn:X}","xid":{xid},"changes":[{{"rel":{REL},"s":"public","t":"items","op":"i","k":[["{key}"]]}}]}}"#
    )
}

#[tokio::test(start_paused = true)]
async fn a_publish_in_flight_at_the_fencing_deadline_never_goes_out() {
    let bus = Arc::new(GatedBus {
        inner: MemoryBus::new(),
        gate: Mutex::new(None),
        forwarded: AtomicUsize::new(0),
    });
    let leases = MemoryKv::new(TTL);
    let checker = FakeChecker::new();
    let repl = FakeReplicator::new();
    let src = ReplicationSource::start(SourceDeps {
        instance_id: "a".into(),
        bus: bus.clone(),
        leases: leases.clone(),
        slots: MemoryKv::new(TTL),
        checker: checker.clone() as Arc<dyn Checker>,
        replicator: repl.clone() as Arc<dyn Replicator>,
        settings: SourceSettings::default(),
    })
    .await
    .expect("the source starts");
    checker.insert_rows(REL, &["1"]);
    let ch = TableChannel {
        database_id: DB,
        branch_id: BRANCH,
        pg_database: "app".into(),
        schema: "public".into(),
        table: "items".into(),
        bucket: None,
    };
    let (tx, mut rx) = mpsc::channel(256);
    let (_handle, _info) = src.watch(&ch, &role("billing"), tx).await.expect("joined");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ChannelEvent::Status { live: true, .. })) => break,
            Ok(Some(_)) => {}
            other => panic!("the slot streams: {other:?}"),
        }
    }
    let session = repl.session().unwrap();

    // The window's publish hangs (a NATS client whose buffer is full), and renewals fail.
    bus.shut();
    leases.set_down(true);
    session.data(0x100, &insert(0x100, 7, "1"));
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert!(
        session.closed(),
        "fenced by the local deadline (last renewal + 7 s)"
    );
    bus.open();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        bus.forwarded.load(Ordering::SeqCst),
        0,
        "W12: a fenced holder publishes nothing after its deadline"
    );
}

#[tokio::test(start_paused = true)]
async fn an_owner_group_is_checked_with_a_remaining_viewers_token() {
    let fake = FakeChecker::new();
    let mut f = TableFilter::new(
        target(),
        relation(REL, "items"),
        fake.clone() as Arc<dyn Checker>,
        Limiter::new(CONCURRENCY),
        VisibilityCache::new(Duration::from_millis(100)),
        FilterSettings::default(),
    );
    let (tx_b, mut rx_b) = mpsc::channel(16);
    f.add(1, owner("token-b"), true, tx_b);
    let (tx_a, _rx_a) = mpsc::channel(16);
    f.add(2, owner("token-a"), true, tx_a);
    // A leaves (its socket closed, or re-authorization removed it): B's checks never carry
    // A's token again.
    f.remove(2);
    fake.insert_rows(REL, &["5"]);
    let window = RelWindow {
        rel: REL,
        changed: false,
        inserts: vec![texts("5")],
        updates: vec![],
        deletes: vec![],
    };
    f.deliver(&window, &[10], Instant::now()).await;
    let tokens = fake.check_tokens(None);
    assert!(!tokens.is_empty(), "the owner group was checked");
    assert!(
        tokens.iter().all(|t| t == "token-b"),
        "only the remaining viewer's token: {tokens:?}"
    );
    assert!(!fake_drain_is_empty(&mut rx_b), "B still gets its rows");

    // A renewal of B's token reaches the group, and survives another viewer leaving.
    let (tx_c, _rx_c) = mpsc::channel(16);
    f.add(3, owner("token-c"), true, tx_c);
    f.renew(1, Arc::from("token-b2"));
    f.remove(3);
    fake.clear_calls();
    f.deliver(&window, &[11], Instant::now()).await;
    let tokens = fake.check_tokens(None);
    assert_eq!(
        tokens.last().map(String::as_str),
        Some("token-b2"),
        "{tokens:?}"
    );
}

fn fake_drain_is_empty(rx: &mut mpsc::Receiver<ChannelEvent>) -> bool {
    drain(rx).is_empty()
}
