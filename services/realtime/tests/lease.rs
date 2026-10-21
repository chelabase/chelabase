//! RT1 T8: leases and slot indexes on the in-memory KV (S4's semantics on tokio's paused clock):
//! create-if-absent, CAS renewal, the local fencing deadline, CAS release, takeover after a crash
//! by expiry (delete markers), racers; S5's slot index semaphore; slot errors and retries (W7,
//! W11, W21, P14). The same against NATS is `leases_live.rs`.

use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use realtime::lease::{
    FENCE_MARGIN, KvError, KvEvent, KvStore, Lease, MemoryKv, TTL, bucket_config, lease_key,
};
use realtime::protocol::StatusReason;
use realtime::slots::{
    RETRY, RETRY_MAX, SlotError, cap, classify, index_key, over_cap, retry_delay, take_index,
};
use tokio::time::{Instant, advance};
use uuid::Uuid;

const BRANCH: Uuid = Uuid::from_u128(0x2222_2222_2222_4222_8222_2222_2222_2222);
const OTHER_BRANCH: Uuid = Uuid::from_u128(0x3333_3333_3333_4333_8333_3333_3333_3333);

fn kv() -> std::sync::Arc<MemoryKv> {
    MemoryKv::new(TTL)
}

#[test]
fn keys_and_bucket_settings() {
    assert_eq!(
        lease_key(BRANCH, 16_384),
        "22222222-2222-4222-8222-222222222222.16384"
    );
    assert_eq!(
        index_key(BRANCH, 3),
        "22222222-2222-4222-8222-222222222222.3"
    );
    let c = bucket_config("CHELABASE_RT_LEASES");
    assert_eq!(c.bucket, "CHELABASE_RT_LEASES");
    assert_eq!(c.history, 1);
    assert_eq!(c.max_age, Duration::from_secs(10));
    assert_eq!(
        c.storage,
        async_nats::jetstream::stream::StorageType::Memory
    );
    assert_eq!(
        c.limit_markers,
        Some(Duration::from_secs(5)),
        "expiry leaves a delete marker watchers see (W12)"
    );
}

#[tokio::test(start_paused = true)]
async fn create_is_if_absent_and_update_and_delete_are_cas() {
    let kv = kv();
    let rev = kv.create("a", Bytes::from_static(b"1")).await;
    assert!(rev.is_ok(), "{rev:?}");
    let rev = rev.unwrap();
    assert_eq!(
        kv.create("a", Bytes::from_static(b"2")).await,
        Err(KvError::Exists)
    );
    let rev2 = kv.update("a", Bytes::from_static(b"3"), rev).await.unwrap();
    assert!(rev2 > rev);
    assert_eq!(
        kv.update("a", Bytes::from_static(b"4"), rev).await,
        Err(KvError::WrongRevision),
        "a stale revision"
    );
    assert_eq!(
        kv.delete_expect("a", rev).await,
        Err(KvError::WrongRevision),
        "never a blind delete"
    );
    assert_eq!(
        kv.get("a").await.unwrap(),
        Some((Bytes::from_static(b"3"), rev2))
    );
    kv.delete_expect("a", rev2).await.unwrap();
    assert_eq!(kv.get("a").await.unwrap(), None);
    assert!(
        kv.create("a", Bytes::from_static(b"5")).await.is_ok(),
        "free again"
    );
}

#[tokio::test(start_paused = true)]
async fn every_revision_expires_after_max_age_and_watchers_see_it() {
    let kv = kv();
    let watch = kv.watch("lease.>").await;
    assert!(watch.is_ok(), "a watch");
    let mut watch = watch.unwrap();
    let rev = kv.create("lease.x", Bytes::new()).await.unwrap();
    assert_eq!(
        watch.next().await,
        Some(KvEvent::Put {
            key: "lease.x".into(),
            revision: rev
        })
    );
    advance(Duration::from_secs(6)).await;
    let rev = kv.update("lease.x", Bytes::new(), rev).await.unwrap();
    let _ = watch.next().await;
    advance(Duration::from_secs(9)).await;
    assert!(
        kv.get("lease.x").await.unwrap().is_some(),
        "renewed at 6 s: alive at 15 s"
    );
    advance(Duration::from_millis(1_100)).await;
    assert_eq!(kv.get("lease.x").await.unwrap(), None, "gone at 16.1 s");
    let marker = tokio::time::timeout(Duration::from_secs(1), watch.next()).await;
    assert_eq!(
        marker,
        Ok(Some(KvEvent::Deleted {
            key: "lease.x".into()
        })),
        "the expiry is a delete marker"
    );
    let _ = rev;
}

#[tokio::test(start_paused = true)]
async fn a_lease_fences_itself_at_its_local_deadline() {
    let kv = kv();
    let t0 = Instant::now();
    let lease = Lease::acquire(kv.as_ref(), "l", "instance-a").await;
    assert!(matches!(lease, Ok(Some(_))), "{lease:?}");
    let mut lease = lease.unwrap().unwrap();
    assert_eq!(lease.deadline(), t0 + TTL - FENCE_MARGIN);
    assert!(!lease.fenced(t0 + Duration::from_secs(6)));
    assert!(lease.fenced(t0 + Duration::from_secs(7)));

    advance(Duration::from_secs(3)).await;
    lease.renew(kv.as_ref()).await.unwrap();
    assert_eq!(
        lease.deadline(),
        t0 + Duration::from_secs(3) + TTL - FENCE_MARGIN
    );

    advance(Duration::from_secs(3)).await;
    kv.set_down(true);
    assert!(lease.renew(kv.as_ref()).await.is_err());
    assert_eq!(
        lease.deadline(),
        t0 + Duration::from_secs(10),
        "a failed renewal keeps the old deadline"
    );
    assert!(lease.fenced(t0 + Duration::from_secs(10)));
}

#[tokio::test(start_paused = true)]
async fn another_holder_waits_and_takes_over_after_a_crash() {
    let kv = kv();
    let a = Lease::acquire(kv.as_ref(), "l", "instance-a").await;
    assert!(matches!(a, Ok(Some(_))), "{a:?}");
    let mut watch = kv.watch("l").await.unwrap();
    assert!(
        Lease::acquire(kv.as_ref(), "l", "instance-b")
            .await
            .unwrap()
            .is_none(),
        "held"
    );
    // A crashes: no renewals, no release.
    drop(a);
    let started = Instant::now();
    let deleted = tokio::time::timeout(Duration::from_secs(12), async {
        while let Some(ev) = watch.next().await {
            if matches!(ev, KvEvent::Deleted { .. }) {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(deleted, Ok(true), "the marker wakes the watcher");
    assert!(started.elapsed() <= Duration::from_secs(10) + Duration::from_millis(100));
    let b = Lease::acquire(kv.as_ref(), "l", "instance-b")
        .await
        .unwrap();
    assert!(b.is_some(), "B takes over within ~10 s");
}

#[tokio::test(start_paused = true)]
async fn release_is_at_the_holders_own_revision() {
    let kv = kv();
    let a = Lease::acquire(kv.as_ref(), "l", "a").await;
    assert!(matches!(a, Ok(Some(_))), "{a:?}");
    a.unwrap().unwrap().release(kv.as_ref()).await.unwrap();
    let b = Lease::acquire(kv.as_ref(), "l", "b")
        .await
        .unwrap()
        .unwrap();

    // A stale holder (its key expired and B took it) can't release B's lease.
    let kv2 = MemoryKv::new(TTL);
    let stale = Lease::acquire(kv2.as_ref(), "l", "a")
        .await
        .unwrap()
        .unwrap();
    advance(TTL + Duration::from_millis(10)).await;
    let fresh = Lease::acquire(kv2.as_ref(), "l", "b")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stale.release(kv2.as_ref()).await,
        Err(KvError::WrongRevision)
    );
    assert!(kv2.get("l").await.unwrap().is_some(), "B's lease survives");
    let _ = (b, fresh);
}

#[tokio::test(start_paused = true)]
async fn two_racers_one_winner() {
    let kv = kv();
    let (a, b) = tokio::join!(
        Lease::acquire(kv.as_ref(), "race", "a"),
        Lease::acquire(kv.as_ref(), "race", "b"),
    );
    let winners = [a, b]
        .into_iter()
        .filter(|r| matches!(r, Ok(Some(_))))
        .count();
    assert_eq!(winners, 1);
}

// --- S5: the slot index semaphore ---

#[test]
fn the_cap_is_half_the_computes_slots_at_most() {
    assert_eq!(cap(10, 20), 10);
    assert_eq!(cap(10, 10), 5, "computes still on 10 slots give realtime 5");
    assert_eq!(cap(10, 40), 10);
    assert_eq!(cap(3, 20), 3);
    assert_eq!(cap(10, 1), 0);
}

#[test]
fn the_rt_count_is_a_second_check() {
    // Counted before ours is created: one more must stay within the cap.
    assert!(over_cap(10, 10));
    assert!(!over_cap(9, 10));
    assert!(over_cap(11, 10));
    assert!(over_cap(0, 0));
}

#[tokio::test(start_paused = true)]
async fn the_lowest_free_index_is_taken() {
    let kv = kv();
    let first = take_index(kv.as_ref(), BRANCH, 2, "a").await;
    assert!(matches!(first, Ok(Some(_))), "{first:?}");
    let first = first.unwrap().unwrap();
    assert_eq!(first.n(), 0);
    let second = take_index(kv.as_ref(), BRANCH, 2, "b")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.n(), 1);
    assert!(
        take_index(kv.as_ref(), BRANCH, 2, "c")
            .await
            .unwrap()
            .is_none(),
        "no free index: no_slot"
    );
    assert!(
        take_index(kv.as_ref(), OTHER_BRANCH, 2, "c")
            .await
            .unwrap()
            .is_some(),
        "another compute has its own cap"
    );
    first.release(kv.as_ref()).await.unwrap();
    let again = take_index(kv.as_ref(), BRANCH, 2, "c")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.n(), 0);
    let _ = second;
}

#[tokio::test(start_paused = true)]
async fn a_crashed_holders_index_frees_itself_and_renewal_keeps_it() {
    let kv = kv();
    let held = take_index(kv.as_ref(), BRANCH, 1, "a").await;
    assert!(matches!(held, Ok(Some(_))), "an index: {held:?}");
    let mut held = held.unwrap();
    let crashed = take_index(kv.as_ref(), OTHER_BRANCH, 1, "x").await.unwrap();
    for _ in 0..4 {
        advance(Duration::from_secs(3)).await;
        held.as_mut().unwrap().renew(kv.as_ref()).await.unwrap();
    }
    assert!(
        take_index(kv.as_ref(), BRANCH, 1, "b")
            .await
            .unwrap()
            .is_none(),
        "renewed with the lease"
    );
    assert!(
        take_index(kv.as_ref(), OTHER_BRANCH, 1, "y")
            .await
            .unwrap()
            .is_some(),
        "the crashed holder's index expired"
    );
    let _ = crashed;
}

#[tokio::test(start_paused = true)]
async fn two_holders_racing_for_the_last_index() {
    let kv = kv();
    let (a, b) = tokio::join!(
        take_index(kv.as_ref(), BRANCH, 1, "a"),
        take_index(kv.as_ref(), BRANCH, 1, "b"),
    );
    let got = [a, b]
        .into_iter()
        .filter(|r| matches!(r, Ok(Some(_))))
        .count();
    assert_eq!(
        got, 1,
        "the cap holds across databases of one compute (W11)"
    );
}

// --- slot errors and retries ---

#[test]
fn postgres_errors_map_to_status_reasons() {
    let cases: &[(Option<&str>, &str, SlotError)] = &[
        (
            Some("53400"),
            "all replication slots are in use",
            SlotError::NoSlot,
        ),
        (
            Some("53300"),
            "number of requested standby connections exceeds max_wal_senders (currently 20)",
            SlotError::NoSlot,
        ),
        (
            Some("42501"),
            "permission denied for database \"app\"",
            SlotError::NoConnect,
        ),
        (
            Some("55000"),
            "database \"app\" is not currently accepting connections",
            SlotError::NoConnect,
        ),
        (
            Some("0A000"),
            "chelabase_keys: database encoding \"SQL_ASCII\" can't be converted to UTF-8",
            SlotError::Unsupported,
        ),
    ];
    for (code, message, expected) in cases {
        assert_eq!(&classify(*code, message), expected, "{code:?} {message}");
    }
    assert!(matches!(
        classify(Some("28P01"), "password authentication failed"),
        SlotError::Unavailable(_)
    ));
    assert!(matches!(
        classify(None, "connection reset"),
        SlotError::Unavailable(_)
    ));
}

#[test]
fn slot_errors_are_status_reasons() {
    assert_eq!(SlotError::NoSlot.reason(), StatusReason::NoSlot);
    assert_eq!(SlotError::NoConnect.reason(), StatusReason::NoConnect);
    assert_eq!(SlotError::ReadOnly.reason(), StatusReason::ReadOnly);
    assert_eq!(SlotError::Unsupported.reason(), StatusReason::Unsupported);
    assert_eq!(SlotError::Waiting.reason(), StatusReason::Waiting);
    assert_eq!(
        SlotError::Unavailable("x".into()).reason(),
        StatusReason::Unavailable
    );
}

#[test]
fn retries_start_at_30_s_and_back_off() {
    assert_eq!(retry_delay(1), Duration::from_secs(30));
    assert_eq!(RETRY, Duration::from_secs(30));
    let mut last = Duration::ZERO;
    for attempt in 1..30 {
        let d = retry_delay(attempt);
        assert!(d >= last, "never shorter: {attempt}");
        assert!(d <= RETRY_MAX, "at most 5 min: {attempt}");
        last = d;
    }
    assert_eq!(retry_delay(2), Duration::from_secs(60));
    assert_eq!(retry_delay(29), RETRY_MAX);
}
