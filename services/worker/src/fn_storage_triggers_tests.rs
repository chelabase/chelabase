//! Edge functions m14: storage triggers with a fake invoker, a fake recorder and a fake bus.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chelabase_metadata::{StorageTriggerTarget, TriggerDelivery};
use chelabase_nats::storage_objects::{
    self, ObjectActor, ObjectEvent, ObjectEventType, ObjectFrom,
};
use chelabase_types::{BranchId, DatabaseId};
use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use crate::fn_schedules::Invoker;
use crate::fn_storage_triggers::{
    Bus, Delivery, Gaps, MAX_AGE, Next, Recorder, Running, event_body, matches, reconcile,
};
use crate::storage_webhooks::backoff;

fn at(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).unwrap()
}

fn target(events: &[&str], bucket: Option<&str>, prefix: Option<&str>) -> StorageTriggerTarget {
    StorageTriggerTarget {
        id: Uuid::new_v4(),
        branch_id: BranchId(Uuid::from_u128(1)),
        database_id: DatabaseId(Uuid::from_u128(2)),
        function_name: "resize".into(),
        pg_database: "app".into(),
        bucket: bucket.map(Into::into),
        prefix: prefix.map(Into::into),
        events: events.iter().map(|s| s.to_string()).collect(),
        enabled_since: at(1_700_000_000),
    }
}

fn event(kind: ObjectEventType, bucket: &str, name: &str, time: DateTime<Utc>) -> ObjectEvent {
    ObjectEvent {
        id: Uuid::new_v4(),
        kind,
        time,
        database_id: Uuid::from_u128(2),
        branch_id: Uuid::from_u128(1),
        pg_database: "app".into(),
        bucket: bucket.into(),
        name: name.into(),
        version_id: Some(Uuid::new_v4()),
        size: Some(3),
        content_type: Some("image/png".into()),
        actor: ObjectActor {
            role: "authenticated".into(),
            sub: None,
        },
        from: None,
    }
}

// --- filters ---

/// AC 2 and 8: another branch's event, another Postgres database's (the same bucket name) and
/// another Chelabase database's are never the trigger's.
#[test]
fn fn_storage_triggers_branch_and_database_filter() {
    let t = target(&["object.created"], Some("avatars"), None);
    let e = event(ObjectEventType::Created, "avatars", "a.png", at(0));
    assert!(matches(&t, &e));
    let other_branch = ObjectEvent {
        branch_id: Uuid::from_u128(9),
        ..e.clone()
    };
    assert!(!matches(&t, &other_branch), "another branch");
    let other_pg = ObjectEvent {
        pg_database: "other".into(),
        ..e.clone()
    };
    assert!(!matches(&t, &other_pg), "another Postgres database");
    let other_db = ObjectEvent {
        database_id: Uuid::from_u128(9),
        ..e.clone()
    };
    assert!(!matches(&t, &other_db), "another database");
}

/// AC 3: bucket, prefix and event filters, a move matching where it went or where it was, and
/// a test event never.
#[test]
fn fn_storage_triggers_bucket_prefix_and_event_filters() {
    let t = target(&["object.created"], None, Some("in/"));
    assert!(!matches(
        &t,
        &event(ObjectEventType::Created, "avatars", "out/a.png", at(0))
    ));
    assert!(matches(
        &t,
        &event(ObjectEventType::Created, "avatars", "in/a.png", at(0))
    ));
    assert!(!matches(
        &t,
        &event(ObjectEventType::Deleted, "avatars", "in/a.png", at(0))
    ));
    let t = target(&["object.created", "object.moved"], Some("avatars"), None);
    assert!(!matches(
        &t,
        &event(ObjectEventType::Created, "docs", "a.png", at(0))
    ));
    assert!(
        !matches(
            &t,
            &event(ObjectEventType::Created, "Avatars", "a.png", at(0))
        ),
        "exact bucket"
    );
    let mut moved = event(ObjectEventType::Moved, "archive", "a.png", at(0));
    moved.from = Some(ObjectFrom {
        bucket: "avatars".into(),
        name: "a.png".into(),
    });
    assert!(matches(&t, &moved), "from where it was");
    let test = ObjectEvent {
        kind: ObjectEventType::Test,
        ..event(ObjectEventType::Created, "avatars", "a.png", at(0))
    };
    assert!(!matches(&target(&["object.created"], None, None), &test));
}

#[test]
fn fn_storage_triggers_event_body_is_the_runtime_contract() {
    let e = event(ObjectEventType::Created, "avatars", "a.png", at(0));
    let body = event_body(&e);
    assert_eq!(body["type"], "storage");
    assert_eq!(body["event"], serde_json::to_value(&e).unwrap());
}

// --- delivery ---

struct FakeInvoker {
    answers: Mutex<Vec<Result<u16, String>>>,
    seen: Mutex<Vec<(BranchId, String, serde_json::Value)>>,
}

impl FakeInvoker {
    fn new(answers: Vec<Result<u16, String>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers),
            seen: Mutex::default(),
        })
    }
}

#[async_trait::async_trait]
impl Invoker for FakeInvoker {
    async fn invoke(
        &self,
        branch: BranchId,
        name: &str,
        body: serde_json::Value,
    ) -> Result<u16, String> {
        self.seen
            .lock()
            .unwrap()
            .push((branch, name.to_string(), body));
        let mut a = self.answers.lock().unwrap();
        if a.len() > 1 {
            a.remove(0)
        } else {
            a[0].clone()
        }
    }
}

#[derive(Default)]
struct FakeRecorder {
    records: Mutex<Vec<(BranchId, TriggerDelivery)>>,
}

#[async_trait::async_trait]
impl Recorder for Arc<FakeRecorder> {
    async fn record(&self, branch: BranchId, delivery: TriggerDelivery) -> Result<(), String> {
        self.records.lock().unwrap().push((branch, delivery));
        Ok(())
    }
}

fn delivery(invoker: Arc<FakeInvoker>) -> (Delivery<Arc<FakeRecorder>>, Arc<FakeRecorder>) {
    let recorder = Arc::new(FakeRecorder::default());
    (Delivery::new(invoker, recorder.clone()), recorder)
}

/// AC 1 and 4: the function answers 500 twice, then 200: three invocations of the trigger's
/// function on its branch with the same event, NAKed with growing backoff, then ACKed; each
/// outcome is recorded.
#[tokio::test]
async fn fn_storage_triggers_nak_with_backoff_until_a_2xx() {
    let invoker = FakeInvoker::new(vec![Ok(500), Err("connection refused".into()), Ok(200)]);
    let (d, recorder) = delivery(invoker.clone());
    let t = target(&["object.created"], Some("avatars"), None);
    let t0 = Utc::now();
    let e = event(ObjectEventType::Created, "avatars", "a.png", t0);
    let payload = serde_json::to_vec(&e).unwrap();
    let mut nexts = Vec::new();
    for attempt in 1..=3u64 {
        nexts.push(d.process(&t, &payload, attempt, t0, 0.0).await);
    }
    assert_eq!(
        nexts,
        [
            Next::Retry(backoff(1, 0.0)),
            Next::Retry(backoff(2, 0.0)),
            Next::Ack
        ]
    );
    let seen = invoker.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    for (branch, name, body) in seen.iter() {
        assert_eq!((*branch, name.as_str()), (t.branch_id, "resize"));
        assert_eq!(*body, event_body(&e));
    }
    let records = recorder.records.lock().unwrap();
    let got: Vec<_> = records
        .iter()
        .map(|(b, r)| (*b, r.id, r.delivered, r.dropped, r.error.clone()))
        .collect();
    assert_eq!(
        got,
        [
            (
                t.branch_id,
                t.id,
                false,
                0,
                Some("the function answered 500".to_string())
            ),
            (
                t.branch_id,
                t.id,
                false,
                0,
                Some("the functions service didn't answer: connection refused".to_string())
            ),
            (t.branch_id, t.id, true, 0, None),
        ]
    );
}

/// An event 24 h old is dropped unsent, and counted; a failing one is retried until its
/// redelivery finds it 24 h old, then dropped. Nothing is sent for an unmatched or unreadable
/// event.
#[tokio::test]
async fn fn_storage_triggers_events_older_than_24h_are_dropped_and_counted() {
    let invoker = FakeInvoker::new(vec![Ok(503)]);
    let (d, recorder) = delivery(invoker.clone());
    let t = target(&["object.created"], None, None);
    let now = Utc::now();
    let stale =
        serde_json::to_vec(&event(ObjectEventType::Created, "a", "b", now - MAX_AGE)).unwrap();
    assert_eq!(d.process(&t, &stale, 1, now, 0.0).await, Next::Dropped);
    assert!(invoker.seen.lock().unwrap().is_empty(), "never sent");
    let almost = event(
        ObjectEventType::Created,
        "a",
        "b",
        now - MAX_AGE + TimeDelta::seconds(1),
    );
    let almost = serde_json::to_vec(&almost).unwrap();
    assert!(matches!(
        d.process(&t, &almost, 9, now, 0.0).await,
        Next::Retry(_)
    ));
    assert_eq!(
        d.process(&t, &almost, 10, now + TimeDelta::seconds(1), 0.0)
            .await,
        Next::Dropped
    );
    assert_eq!(invoker.seen.lock().unwrap().len(), 1);
    let unmatched = serde_json::to_vec(&event(ObjectEventType::Deleted, "a", "b", now)).unwrap();
    assert_eq!(d.process(&t, &unmatched, 1, now, 0.0).await, Next::Ack);
    assert_eq!(d.process(&t, b"not json", 1, now, 0.0).await, Next::Ack);
    assert_eq!(invoker.seen.lock().unwrap().len(), 1);
    assert_eq!(d.dropped.load(Ordering::SeqCst), 2);
    let dropped: i64 = recorder
        .records
        .lock()
        .unwrap()
        .iter()
        .map(|(_, r)| r.dropped)
        .sum();
    assert_eq!(dropped, 2, "each drop reaches the trigger's dropped count");
}

// --- gaps: the per-database cap ---

/// AC 10: the trigger's consumer is stuck while a burst on another branch pushes the
/// database's subject past its cap: every event evicted before the consumer reached it is
/// counted once, and nothing it did reach is.
#[test]
fn fn_storage_triggers_cap_evictions_are_counted_as_dropped() {
    let cap = 5;
    let mut g = Gaps::new(cap, 0);
    let mut dropped = 0;
    // Stream sequences 10, 20, ... (other databases in between): the consumer gets the first.
    for seq in (1..=3).map(|i| i * 10) {
        dropped += g.arrived(seq);
    }
    let (d, ordinal) = g.first_delivered(10);
    assert_eq!((d, ordinal), (0, Some(1)));
    // 17 more arrive while it is stuck: 20 in all, so the subject keeps the newest 5
    // (160..=200) and 20..=150 (14 events) are gone.
    for seq in (4..=20).map(|i| i * 10) {
        dropped += g.arrived(seq);
    }
    assert_eq!(dropped, 14);
    // The consumer resumes at the oldest kept one: nothing more is counted.
    assert_eq!(g.first_delivered(160).0, 0);
    for seq in [170, 180, 190, 200] {
        assert_eq!(g.first_delivered(seq).0, 0);
    }
    assert_eq!(dropped + g.arrived(210), 14);
}

/// An event the consumer skips (gone before it got there, the cap or the stream's own
/// limits) is counted when the consumer passes it; one the watcher hadn't seen yet isn't.
#[test]
fn fn_storage_triggers_a_skipped_sequence_is_a_gap() {
    let mut g = Gaps::new(100, 0);
    for seq in [1, 2, 3, 4] {
        assert_eq!(g.arrived(seq), 0);
    }
    assert_eq!(g.first_delivered(1).0, 0);
    assert_eq!(g.first_delivered(4).0, 2, "2 and 3 were skipped");
    // The consumer ahead of the watcher: what the watcher sees later is already delivered.
    assert_eq!(g.first_delivered(6), (0, None));
    assert_eq!(g.arrived(5), 0);
    assert_eq!(g.arrived(6), 0);
    assert_eq!(g.first_delivered(7).0, 0);
    // The watcher started after the consumer's position: older sequences are never its gaps.
    let mut g = Gaps::new(100, 50);
    assert_eq!(g.first_delivered(40).0, 0);
    assert_eq!(g.arrived(51), 0);
    assert_eq!(g.first_delivered(51).0, 0);
}

/// A NAKed event the cap evicts before its redelivery is counted once, when it is gone; one
/// settled (delivered or dropped) never is.
#[test]
fn fn_storage_triggers_an_evicted_retry_is_counted() {
    let cap = 3;
    let mut g = Gaps::new(cap, 0);
    for seq in [1, 2] {
        g.arrived(seq);
    }
    let (_, o1) = g.first_delivered(1);
    let (_, o2) = g.first_delivered(2);
    g.unsettled(1, o1);
    g.unsettled(2, o2);
    // Redelivered: in flight again, then settled.
    let o2 = g.redelivered(2);
    assert_eq!(o2, Some(2));
    assert_eq!(g.arrived(3) + g.arrived(4), 1, "1 is gone (2, 3, 4 kept)");
    assert_eq!(g.first_delivered(3).0, 0);
    assert_eq!(g.first_delivered(4).0, 0);
    assert_eq!(g.arrived(5), 0, "2 was in flight, not waiting");
    assert_eq!(g.first_delivered(5).0, 0);
    // An unknown ordinal (the watcher hadn't seen it) counts from the newest arrival.
    g.unsettled(9, None);
    assert_eq!(g.arrived(6) + g.arrived(7), 0);
    assert_eq!(g.arrived(8), 1);
    assert_eq!(g.redelivered(9), None, "counted once and forgotten");
}

// --- the consumers' lifecycle ---

#[derive(Default)]
struct FakeBus {
    deleted: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Bus for FakeBus {
    async fn delete_consumer(&self, name: &str) -> Result<(), String> {
        self.deleted.lock().unwrap().push(name.to_string());
        Ok(())
    }
}

fn idle(_: tokio::sync::watch::Receiver<Arc<StorageTriggerTarget>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(std::future::pending())
}

fn name(t: &StorageTriggerTarget) -> String {
    storage_objects::fn_trigger_consumer_name(t.id, t.enabled_since)
}

/// A new trigger starts at its `enabled_since`; a deleted or tombstoned one (gone from the
/// targets) stops and its consumer is deleted within the poll; a disabled-then-re-enabled one
/// has its old consumer deleted and a new one from the new time; an unchanged one keeps its
/// task.
#[tokio::test]
async fn fn_storage_triggers_consumers_follow_the_triggers() {
    let bus = FakeBus::default();
    let mut running: HashMap<String, Running> = HashMap::new();
    let (a, b) = (
        target(&["object.created"], None, None),
        target(&["object.created"], None, None),
    );
    reconcile(&bus, &mut running, vec![a.clone(), b.clone()], &idle).await;
    let mut keys: Vec<_> = running.keys().cloned().collect();
    keys.sort();
    let mut want = vec![name(&a), name(&b)];
    want.sort();
    assert_eq!(keys, want);
    assert!(bus.deleted.lock().unwrap().is_empty());
    let a_task = running[&name(&a)].task.id();

    // b deleted (or its branch tombstoned): gone from the targets.
    reconcile(&bus, &mut running, vec![a.clone()], &idle).await;
    assert_eq!(*bus.deleted.lock().unwrap(), [name(&b)]);
    assert_eq!(running.keys().cloned().collect::<Vec<_>>(), [name(&a)]);
    assert_eq!(
        running[&name(&a)].task.id(),
        a_task,
        "unchanged keeps its task"
    );

    // a disabled (gone), then re-enabled an hour later (a new enabled_since).
    let again = StorageTriggerTarget {
        enabled_since: a.enabled_since + TimeDelta::hours(1),
        ..a.clone()
    };
    reconcile(&bus, &mut running, vec![again.clone()], &idle).await;
    assert_eq!(*bus.deleted.lock().unwrap(), [name(&b), name(&a)]);
    assert_eq!(running.keys().cloned().collect::<Vec<_>>(), [name(&again)]);
    assert_ne!(name(&again), name(&a));
    let cfg = storage_objects::fn_trigger_consumer(
        again.id,
        again.database_id.as_uuid(),
        again.enabled_since,
        Duration::from_secs(90),
    );
    let async_nats::jetstream::consumer::DeliverPolicy::ByStartTime { start_time } =
        cfg.deliver_policy
    else {
        panic!("{:?}", cfg.deliver_policy)
    };
    assert_eq!(
        start_time.unix_timestamp(),
        again.enabled_since.timestamp(),
        "nothing from the disabled hour"
    );

    reconcile(&bus, &mut running, vec![], &idle).await;
    assert!(running.is_empty());
    assert_eq!(bus.deleted.lock().unwrap().len(), 3);
}
