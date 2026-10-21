//! RT1 T8 coder's edge tests (beyond the RED set): the decoder's and merger's corner cases,
//! partition moves, fail-closed conversion and key shapes in the filter, leases lost to another
//! holder, compute-stopping for another branch, and the connection settings' TLS rules.

mod fakes;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chelabase_nats::rt::{COMPUTE_STOPPING, ComputeStopping};
use fakes::*;
use realtime::bus::{Bus, MemoryBus};
use realtime::channel::TableChannel;
use realtime::check::{
    CONCURRENCY, CheckAnswer, CheckError, CheckRequest, CheckTarget, Checker, ComputeFacts,
    Limiter, Resolved, VisibilityCache,
};
use realtime::decoder::{self, Body, Op, RelChange, ReplicationTarget, Txn, replication_config};
use realtime::filter::{FilterSettings, TableFilter};
use realtime::lease::{KvError, KvStore, Lease, MemoryKv, TTL, lease_key};
use realtime::merge::{MAX_KEYS, Merger, RelWindow, Window};
use realtime::owner::parse_visible;
use realtime::partition::{Parent, fan_out};
use realtime::pool::{ConnInfo, connect_config};
use realtime::protocol::{Change, ChangeOp};
use realtime::replication::{ReplicationSource, SourceDeps, SourceSettings};
use realtime::slots::Replicator;
use realtime::source::{ChangeSource, ChannelEvent, CheckIdentity};
use tokio::sync::mpsc;
use tokio::time::{Instant, advance};
use tokio_postgres::types::PgLsn;

// --- decoder ---

#[test]
fn malformed_shapes_fail_closed() {
    for msg in [
        // An empty key row, a relation OID past u32, a float version, no xid.
        &br#"{"v":1,"lsn":"0/1","xid":1,"changes":[{"rel":5,"s":"p","t":"t","op":"i","k":[[]]}]}"#[..],
        br#"{"v":1,"lsn":"0/1","xid":1,"changes":[{"rel":4294967296,"s":"p","t":"t","op":"i","k":[["1"]]}]}"#,
        br#"{"v":1.0,"lsn":"0/1","xid":1,"changes":[]}"#,
        br#"{"v":1,"lsn":"0/1","changes":[]}"#,
        br#"{"v":1,"lsn":"0/1","xid":-1,"changes":[]}"#,
        br#"{"v":1,"lsn":"0/1","xid":1}"#,
        br#"[1,2]"#,
    ] {
        assert_eq!(
            decoder::parse(msg).body,
            Body::All,
            "{}",
            String::from_utf8_lossy(msg)
        );
    }
    // A readable message keeps its LSN even when it's `all` (it is acknowledged in order).
    let txn = decoder::parse(br#"{"v":2,"lsn":"0/AB","xid":3}"#);
    assert_eq!(txn.lsn, Some("0/AB".parse::<PgLsn>().unwrap()));
}

#[test]
fn a_replication_target_without_an_ip_is_still_verified_by_name() {
    let config = replication_config(&ReplicationTarget {
        host: "compute-1".into(),
        port: 5432,
        server_name: Some("ep-1.compute.chelabase.internal".into()),
        database: "app".into(),
        password: "p".into(),
    });
    assert_eq!(
        config.get_ssl_mode(),
        tokio_postgres::config::SslMode::Require
    );
    assert_eq!(config.get_hostaddrs().len(), 0);
    let plain = replication_config(&ReplicationTarget {
        host: "192.0.2.10".into(),
        port: 5432,
        server_name: None,
        database: "app".into(),
        password: "p".into(),
    });
    assert_eq!(plain.get_options(), None);
    assert_eq!(plain.get_application_name(), Some("chelabase_realtime"));
}

#[test]
#[should_panic(expected = "a slot name made by new_slot_name")]
fn a_foreign_slot_name_is_never_spliced_into_a_command() {
    let _ = decoder::create_slot_command("rt_x; DROP_REPLICATION_SLOT other");
}

// --- merge and partitions ---

fn k(t: &str) -> Vec<String> {
    vec![t.to_string()]
}

fn change(rel: u32, op: Op, keys: &[&str]) -> RelChange {
    RelChange {
        rel,
        schema: "public".into(),
        table: "t".into(),
        op,
        keys: keys.iter().map(|t| k(t)).collect(),
    }
}

#[test]
fn an_insert_then_an_insert_and_delete_in_one_transaction_is_re_read() {
    let mut m = Merger::new(MAX_KEYS);
    let t0 = Instant::now();
    m.push(
        t0,
        &Txn {
            lsn: Some(PgLsn::from(1)),
            xid: Some(1),
            body: Body::Changes(vec![change(1, Op::Insert, &["a"])]),
        },
    );
    m.push(
        t0,
        &Txn {
            lsn: Some(PgLsn::from(2)),
            xid: Some(2),
            body: Body::Changes(vec![
                change(1, Op::Insert, &["a"]),
                change(1, Op::Delete, &["a"]),
            ]),
        },
    );
    let w = m.take().unwrap();
    let r = w.rel(1).unwrap();
    assert_eq!(r.updates, vec![k("a")], "{r:?}");
    assert!(r.inserts.is_empty() && r.deletes.is_empty());
}

#[test]
fn a_row_moving_between_partitions_is_an_update_of_the_root() {
    let parents = HashMap::from([
        (11, vec![Parent { oid: 10, keyed: true }]),
        (12, vec![Parent { oid: 10, keyed: true }]),
    ]);
    let w = Window {
        all: false,
        rels: vec![
            RelWindow {
                rel: 11,
                deletes: vec![k("7")],
                ..RelWindow::default()
            },
            RelWindow {
                rel: 12,
                inserts: vec![k("7")],
                ..RelWindow::default()
            },
        ],
        xids: vec![1],
        end_lsn: Some(PgLsn::from(1)),
    };
    let out = fan_out(&w, &parents, MAX_KEYS);
    let root = out.iter().find(|r| r.rel == 10).unwrap();
    assert_eq!(root.updates, vec![k("7")], "re-read on the root: {root:?}");
    assert!(root.inserts.is_empty() && root.deletes.is_empty());
}

// --- filter: fail-closed conversion and key shapes ---

/// The fake checker, whose conversion can't convert the keys listed.
struct NoConvert {
    inner: Arc<FakeChecker>,
    unconvertible: Vec<String>,
}

#[async_trait]
impl Checker for NoConvert {
    async fn compute(&self, t: &CheckTarget) -> Result<ComputeFacts, CheckError> {
        self.inner.compute(t).await
    }
    async fn resolve(
        &self,
        t: &CheckTarget,
        id: &CheckIdentity,
        s: &str,
        n: &str,
    ) -> Result<Option<Resolved>, CheckError> {
        self.inner.resolve(t, id, s, n).await
    }
    async fn parents(&self, t: &CheckTarget, rel: u32) -> Result<Vec<Parent>, CheckError> {
        self.inner.parents(t, rel).await
    }
    async fn check(
        &self,
        t: &CheckTarget,
        id: &CheckIdentity,
        req: &CheckRequest,
    ) -> Result<CheckAnswer, CheckError> {
        // RT1 final fix round: the conversion comes with the check (one session).
        let mut a = self.inner.check(t, id, req).await?;
        for (texts, c) in req.convert.iter().zip(a.converted.iter_mut()) {
            if self.unconvertible.contains(&texts[0]) {
                *c = None;
            }
        }
        Ok(a)
    }
    async fn convert(
        &self,
        t: &CheckTarget,
        id: &CheckIdentity,
        rel: u32,
        keys: &[Vec<String>],
    ) -> Result<Vec<Option<String>>, CheckError> {
        let out = self.inner.convert(t, id, rel, keys).await?;
        Ok(keys
            .iter()
            .zip(out)
            .map(|(texts, c)| c.filter(|_| !self.unconvertible.contains(&texts[0])))
            .collect())
    }
}

fn filter_with(checker: Arc<dyn Checker>) -> TableFilter {
    TableFilter::new(
        target(),
        relation(REL, "items"),
        checker,
        Limiter::new(CONCURRENCY),
        VisibilityCache::new(Duration::from_millis(100)),
        FilterSettings::default(),
    )
}

fn win(inserts: &[&str], deletes: &[&str]) -> RelWindow {
    RelWindow {
        rel: REL,
        changed: false,
        inserts: inserts.iter().map(|t| k(t)).collect(),
        updates: vec![],
        deletes: deletes.iter().map(|t| k(t)).collect(),
    }
}

fn changed() -> ChannelEvent {
    ChannelEvent::Rows(vec![Change {
        op: ChangeOp::Changed,
        keys: None,
    }])
}

#[tokio::test(start_paused = true)]
async fn an_insert_that_cant_be_converted_is_changed_never_a_guess() {
    let fake = FakeChecker::new();
    fake.insert_rows(REL, &["1", "2"]);
    let mut f = filter_with(Arc::new(NoConvert {
        inner: fake.clone(),
        unconvertible: vec!["2".into()],
    }));
    let (tx, mut rx) = mpsc::channel(16);
    f.add(1, role("billing"), true, tx);
    f.deliver(&win(&["1", "2"], &[]), &[10], Instant::now()).await;
    assert_eq!(drain(&mut rx), vec![changed()]);
}

#[tokio::test(start_paused = true)]
async fn a_delete_that_cant_be_converted_is_keyless_for_bypass_and_nothing_for_rls() {
    let fake = FakeChecker::new();
    fake.hide(Some("reader"), &[]);
    let mut f = filter_with(Arc::new(NoConvert {
        inner: fake.clone(),
        unconvertible: vec!["9".into()],
    }));
    let (tx, mut bypass) = mpsc::channel(16);
    f.add(1, role("billing"), true, tx);
    let (tx, mut rls) = mpsc::channel(16);
    f.add(2, role("reader"), false, tx);
    // 9 is on the RLS viewer's screen, confirmed visible, then deleted.
    fake.insert_rows(REL, &["9"]);
    f.view(2, vec![ks("9")]).await;
    fake.delete_rows(REL, &["9"]);
    f.deliver(&win(&[], &["8", "9"]), &[10], Instant::now()).await;
    assert_eq!(
        drain(&mut bypass),
        vec![ChannelEvent::Rows(vec![
            Change {
                op: ChangeOp::Delete,
                keys: Some(vec![ks("8")]),
            },
            Change {
                op: ChangeOp::Delete,
                keys: None,
            },
        ])]
    );
    assert_eq!(
        drain(&mut rls),
        vec![],
        "an unconverted key is never matched against a view set"
    );
}

#[tokio::test(start_paused = true)]
async fn a_key_of_the_wrong_arity_is_changed_without_a_check() {
    let fake = FakeChecker::new();
    let mut f = filter_with(fake.clone());
    let (tx, mut rx) = mpsc::channel(16);
    f.add(1, role("billing"), true, tx);
    let mut w = win(&["1"], &[]);
    w.inserts[0].push("extra".into());
    f.deliver(&w, &[10], Instant::now()).await;
    assert_eq!(drain(&mut rx), vec![changed()]);
    assert!(fake.checks().is_empty(), "{:?}", fake.checks());
}

#[tokio::test(start_paused = true)]
async fn a_delete_only_window_probes_without_waiting_for_its_xids() {
    let fake = FakeChecker::new();
    fake.never_visible(10);
    let mut f = filter_with(fake.clone());
    let (tx, mut rx) = mpsc::channel(16);
    f.add(1, role("billing"), true, tx);
    let started = Instant::now();
    f.deliver(&win(&[], &["3"]), &[10], Instant::now()).await;
    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(
        fake.checks(),
        vec![Call::Check {
            role: Some("billing".into()),
            rel: REL,
            xids: vec![],
            keys: None
        }],
        "a probe: no rows read, no xid to wait for"
    );
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![Change {
            op: ChangeOp::Delete,
            keys: Some(vec![ks("3")]),
        }])]
    );
}

#[tokio::test(start_paused = true)]
async fn an_rls_viewer_that_saw_a_key_deleted_doesnt_get_its_next_delete() {
    // Delivered, then deleted (keyed), then re-inserted hidden and deleted again: the second
    // delete says nothing (the key left the viewer's delivered set with the first).
    let fake = FakeChecker::new();
    fake.hide(Some("reader"), &[]);
    let mut f = filter_with(fake.clone());
    let (tx, mut rx) = mpsc::channel(16);
    f.add(1, role("reader"), false, tx);
    fake.insert_rows(REL, &["5"]);
    f.deliver(&win(&["5"], &[]), &[10], Instant::now()).await;
    fake.delete_rows(REL, &["5"]);
    f.deliver(&win(&[], &["5"]), &[11], Instant::now()).await;
    assert_eq!(drain(&mut rx).len(), 2);
    fake.insert_rows(REL, &["5"]);
    fake.hide(Some("reader"), &["5"]);
    f.deliver(&win(&["5"], &[]), &[12], Instant::now()).await;
    fake.delete_rows(REL, &["5"]);
    f.deliver(&win(&[], &["5"]), &[13], Instant::now()).await;
    assert_eq!(drain(&mut rx), vec![], "nothing about a row it couldn't see");
}

#[test]
fn a_visible_answer_without_a_probe_carries_no_keys() {
    let a = parse_visible(
        200,
        br#"{"probe":{"ok":false,"bypass":true},"visible":true,"keys":["[\"1\"]"]}"#,
    )
    .unwrap();
    assert!(!a.probe_ok && a.keys.is_empty(), "{a:?}");
    let a = parse_visible(
        200,
        br#"{"probe":{"ok":true,"bypass":true},"visible":false,"keys":["[\"1\"]"]}"#,
    )
    .unwrap();
    assert!(a.keys.is_empty(), "not visible: no keys, whatever the body says");
}

// --- leases ---

#[tokio::test(start_paused = true)]
async fn a_renewal_after_expiry_fails_and_never_revives_the_key() {
    let kv = MemoryKv::new(TTL);
    let mut lease = Lease::acquire(kv.as_ref(), "l", "a").await.unwrap().unwrap();
    advance(TTL + Duration::from_millis(1)).await;
    let other = Lease::acquire(kv.as_ref(), "l", "b").await.unwrap();
    assert!(other.is_some(), "expired: another holder takes it");
    assert_eq!(lease.renew(kv.as_ref()).await, Err(KvError::WrongRevision));
    assert_eq!(
        kv.get("l").await.unwrap().map(|(v, _)| v),
        Some(Bytes::from_static(b"b"))
    );
}

// --- the source ---

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
    async fn source(&self) -> Arc<ReplicationSource> {
        ReplicationSource::start(SourceDeps {
            instance_id: "a".into(),
            bus: self.bus.clone(),
            leases: self.leases.clone(),
            slots: self.slots.clone(),
            checker: self.checker.clone() as Arc<dyn Checker>,
            replicator: self.repl.clone() as Arc<dyn Replicator>,
            settings: SourceSettings::default(),
        })
        .await
        .unwrap()
    }
}

fn items() -> TableChannel {
    TableChannel {
        database_id: DB,
        branch_id: BRANCH,
        pg_database: "app".into(),
        schema: "public".into(),
        table: "items".into(),
    }
}

async fn until_live(rx: &mut mpsc::Receiver<ChannelEvent>) {
    let live = ChannelEvent::Status {
        live: true,
        reason: None,
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(ev) = rx.recv().await {
            if ev == live {
                return;
            }
        }
    })
    .await
    .expect("live");
}

#[tokio::test(start_paused = true)]
async fn compute_stopping_for_another_branch_or_a_forged_payload_changes_nothing() {
    let w = World::new();
    let src = w.source().await;
    let (tx, mut rx) = mpsc::channel(256);
    let _h = src.watch(&items(), &role("billing"), tx).await.unwrap();
    until_live(&mut rx).await;
    let session = w.repl.session().unwrap();
    let other = uuid::Uuid::from_u128(77);
    for (subject_branch, payload_branch) in [(other, other), (other, BRANCH), (BRANCH, other)] {
        w.bus
            .publish(
                &format!("{COMPUTE_STOPPING}.{subject_branch}"),
                None,
                serde_json::to_vec(&ComputeStopping {
                    branch_id: payload_branch,
                })
                .unwrap()
                .into(),
            )
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!session.closed(), "only this branch's own signal stops it");
    assert_eq!(w.repl.starts(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_lease_lost_to_another_holder_stops_decoding_and_leaves_its_key_alone() {
    let w = World::new();
    let src = w.source().await;
    let (tx, mut rx) = mpsc::channel(256);
    let _h = src.watch(&items(), &role("billing"), tx).await.unwrap();
    until_live(&mut rx).await;
    let session = w.repl.session().unwrap();
    // Someone else's revision replaces ours (as after an expiry during a partition).
    let key = lease_key(BRANCH, DB_OID);
    let (_, rev) = w.leases.get(&key).await.unwrap().unwrap();
    w.leases.delete_expect(&key, rev).await.unwrap();
    w.leases
        .create(&key, Bytes::from_static(b"other"))
        .await
        .unwrap();
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !session.closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fenced at the next renewal");
    assert!(started.elapsed() <= Duration::from_secs(4));
    assert_eq!(
        w.leases.get(&key).await.unwrap().map(|(v, _)| v),
        Some(Bytes::from_static(b"other")),
        "never deletes the new holder's key"
    );
}

// --- connection settings (U15) ---

fn info(server_name: Option<&str>) -> ConnInfo {
    ConnInfo {
        host: "10.0.0.5".into(),
        port: 5432,
        server_name: server_name.map(str::to_string),
        replication_host: None,
        user: "authenticator".into(),
        password: "pw".into(),
    }
}

#[test]
fn a_named_compute_needs_the_ca_and_an_unnamed_one_is_plain_only_without_it() {
    assert!(matches!(
        connect_config(&info(Some("ep.compute.chelabase.internal")), "app", None),
        Err(CheckError::Unavailable(_))
    ));
    let plain = connect_config(&info(None), "app", None).unwrap();
    assert_eq!(plain.get_dbname(), Some("app"));
    assert_eq!(plain.get_user(), Some("authenticator"));
    assert!(
        !format!("{:?}", info(None)).contains("pw"),
        "the password is never in a log line"
    );
}
