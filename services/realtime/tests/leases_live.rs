//! RT1 T8: leases and slot indexes against the stack's NATS (S4, S5, R17, W12, S29), with the
//! runner's credentials (its NATS user has realtime's JetStream rights, S2). Every key is unique
//! to the test run, so runs don't collide.
//!
//! ```text
//! just workbench test -p realtime --test leases_live -- --run-ignored only
//! ```

use std::time::{Duration, Instant};

use bytes::Bytes;
use chelabase_nats::rt::{LEASES_BUCKET, SLOTS_BUCKET};
use futures_util::StreamExt;
use realtime::lease::{KvError, KvEvent, KvStore, Lease, NatsKv, ensure_buckets, lease_key};
use realtime::slots::take_index;
use uuid::Uuid;

async fn js() -> async_nats::jetstream::Context {
    let config = chelabase_nats::NatsConfig::from_env().expect("the runner's NATS settings");
    let client = chelabase_nats::connect(&config)
        .await
        .expect("NATS with the runner's credentials");
    async_nats::jetstream::new(client)
}

async fn buckets() -> (NatsKv, NatsKv) {
    let b = ensure_buckets(&js().await).await;
    assert!(b.is_ok(), "the buckets: {:?}", b.as_ref().err());
    let b = b.unwrap();
    (b.leases, b.slots)
}

fn unique_key() -> String {
    lease_key(Uuid::new_v4(), 16_384)
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test leases_live -- --run-ignored only)"]
async fn buckets_are_created_per_s4() {
    let js = js().await;
    let first = ensure_buckets(&js).await;
    assert!(first.is_ok(), "{:?}", first.as_ref().err());
    assert!(
        ensure_buckets(&js).await.is_ok(),
        "idempotent at every start"
    );
    for bucket in [LEASES_BUCKET, SLOTS_BUCKET] {
        let kv = js.get_key_value(bucket).await.expect(bucket);
        let status = kv.status().await.expect("status");
        assert_eq!(status.history(), 1, "{bucket}");
        assert_eq!(status.max_age(), Duration::from_secs(10), "{bucket}");
        let config = &status.info.config;
        assert_eq!(
            config.storage,
            async_nats::jetstream::stream::StorageType::Memory,
            "{bucket}"
        );
        assert_eq!(
            config.subject_delete_marker_ttl,
            Some(Duration::from_secs(5)),
            "{bucket}: expiry leaves delete markers (W12)"
        );
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test leases_live -- --run-ignored only)"]
async fn create_cas_renew_fence_and_cas_release() {
    let (leases, _) = buckets().await;
    let key = unique_key();
    let started = tokio::time::Instant::now();
    let a = Lease::acquire(&leases, &key, "instance-a").await;
    assert!(matches!(a, Ok(Some(_))), "{a:?}");
    let mut a = a.unwrap().unwrap();
    assert!(a.deadline() <= started + Duration::from_secs(7) + Duration::from_millis(500));
    assert!(
        Lease::acquire(&leases, &key, "instance-b")
            .await
            .unwrap()
            .is_none(),
        "create-if-absent"
    );
    let rev = a.revision();
    tokio::time::sleep(Duration::from_secs(3)).await;
    a.renew(&leases).await.expect("CAS renewal");
    assert!(a.revision() > rev);
    assert!(!a.fenced(tokio::time::Instant::now()));
    // A renewal at a stale revision fails (someone else wrote the key).
    let (_, current) = leases.get(&key).await.unwrap().unwrap();
    assert_eq!(current, a.revision());
    assert_eq!(
        leases.update(&key, Bytes::from_static(b"x"), rev).await,
        Err(KvError::WrongRevision)
    );
    a.release(&leases).await.expect("CAS release");
    assert_eq!(leases.get(&key).await.unwrap(), None);
    assert!(
        Lease::acquire(&leases, &key, "instance-b")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test leases_live -- --run-ignored only)"]
async fn a_crashed_holder_is_taken_over_within_10_s_by_its_marker() {
    let (leases, _) = buckets().await;
    let key = unique_key();
    let mut watch = leases.watch(&key).await.expect("a watch");
    let a = Lease::acquire(&leases, &key, "instance-a")
        .await
        .unwrap()
        .unwrap();
    let crashed = Instant::now();
    drop(a); // no renewals, no release
    let deleted = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(ev) = watch.next().await {
            if matches!(ev, KvEvent::Deleted { .. }) {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(deleted, Ok(true), "the delete marker reaches the watcher");
    let took = crashed.elapsed();
    assert!(took <= Duration::from_secs(12), "{took:?}");
    assert!(
        Lease::acquire(&leases, &key, "instance-b")
            .await
            .unwrap()
            .is_some(),
        "B takes over"
    );
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test leases_live -- --run-ignored only)"]
async fn two_racers_one_winner_and_no_blind_delete() {
    let (leases, _) = buckets().await;
    let key = unique_key();
    let (a, b) = tokio::join!(
        Lease::acquire(&leases, &key, "a"),
        Lease::acquire(&leases, &key, "b")
    );
    let mut won: Vec<Lease> = [a, b].into_iter().filter_map(|r| r.unwrap()).collect();
    assert_eq!(won.len(), 1);
    let stale = won.pop().unwrap();
    tokio::time::sleep(Duration::from_secs(11)).await;
    let fresh = Lease::acquire(&leases, &key, "c").await.unwrap();
    assert!(fresh.is_some(), "expired, taken");
    assert_eq!(stale.release(&leases).await, Err(KvError::WrongRevision));
    assert!(
        leases.get(&key).await.unwrap().is_some(),
        "C's lease survives"
    );
}

#[tokio::test]
#[ignore = "needs the workbench stack (just workbench test -p realtime --test leases_live -- --run-ignored only)"]
async fn the_slot_index_semaphore() {
    let (_, slots) = buckets().await;
    let branch = Uuid::new_v4();
    let (a, b) = tokio::join!(
        take_index(&slots, branch, 2, "a"),
        take_index(&slots, branch, 2, "b")
    );
    let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());
    let mut ns = [a.n(), b.n()];
    ns.sort();
    assert_eq!(ns, [0, 1], "racers take distinct indexes");
    assert!(
        take_index(&slots, branch, 2, "c").await.unwrap().is_none(),
        "no free index: no_slot"
    );
    let freed = a.n();
    a.release(&slots).await.unwrap();
    assert_eq!(
        take_index(&slots, branch, 2, "c")
            .await
            .unwrap()
            .map(|i| i.n()),
        Some(freed)
    );
    let _ = b;
}
