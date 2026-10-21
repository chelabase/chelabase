//! RT1 T7: limits (spec §8.1, R15, P16, S6, W33, W35): the defaults, joins per socket per
//! second, sockets per member as the Valkey sorted set `rt:cnt:sock:<sub>` across instances,
//! and tickets per member per second.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use realtime::config::{Limits, Timings};
use realtime::limits::{
    RateLimiter, SOCKET_ENTRY_TTL, SOCKET_REFRESH, SocketCounter, TicketRate, socket_key,
    ticket_rate_key,
};
use realtime::store::{CounterStore, MemoryStore};

#[test]
fn the_default_limits_are_the_specs() {
    let l = Limits::default();
    assert_eq!(l.sockets_per_member, 20);
    assert_eq!(l.channels_per_socket, 50);
    assert_eq!(l.joins_per_sec, 5);
    assert_eq!(l.tickets_per_sec, 10);
    assert_eq!(l.presence_per_sec, 20);
    assert_eq!(l.members_per_channel, 1000);
}

#[test]
fn the_default_timings_are_the_specs() {
    let t = Timings::default();
    assert_eq!(t.auth_timeout, Duration::from_secs(5));
    assert_eq!(t.ping_interval, Duration::from_secs(25));
    assert_eq!(t.pong_timeout, Duration::from_secs(10));
    assert_eq!(t.reauth_interval, Duration::from_secs(60));
    assert_eq!(t.access_coalesce, Duration::from_secs(1));
    assert_eq!(t.project_dedup, Duration::from_secs(1));
    assert_eq!(t.presence_heartbeat, Duration::from_secs(5));
    assert_eq!(t.presence_missed, 3);
    assert_eq!(t.socket_refresh, SOCKET_REFRESH);
    assert_eq!(SOCKET_REFRESH, Duration::from_secs(30));
    assert_eq!(SOCKET_ENTRY_TTL, Duration::from_secs(90));
}

#[test]
fn keys() {
    assert_eq!(socket_key("user-1"), "rt:cnt:sock:user-1");
    assert_eq!(
        ticket_rate_key("user-1", 1_700_000_000),
        "rt:cnt:tix:user-1:1700000000"
    );
}

#[test]
fn joins_are_limited_to_five_in_any_second() {
    let t0 = Instant::now();
    let mut r = RateLimiter::new(5);
    for i in 0..5 {
        assert!(r.allow(t0), "join {i}");
    }
    assert!(!r.allow(t0), "the sixth in the same second");
    assert!(!r.allow(t0 + Duration::from_millis(999)));
    assert!(r.allow(t0 + Duration::from_secs(1)), "a second later");
}

#[test]
fn the_window_slides() {
    let t0 = Instant::now();
    let mut r = RateLimiter::new(2);
    assert!(r.allow(t0));
    assert!(r.allow(t0 + Duration::from_millis(600)));
    assert!(!r.allow(t0 + Duration::from_millis(900)));
    assert!(
        r.allow(t0 + Duration::from_millis(1001)),
        "the first one left the window"
    );
    assert!(!r.allow(t0 + Duration::from_millis(1100)));
}

const T0: u64 = 1_700_000_000_000;

#[tokio::test]
async fn a_member_gets_at_most_the_limit_of_sockets() {
    let store = MemoryStore::new();
    let c = SocketCounter::new(store.clone(), 3);
    for i in 0..3 {
        assert!(
            c.admit("ana", &format!("s{i}"), T0).await.unwrap(),
            "socket {i}"
        );
    }
    assert!(
        !c.admit("ana", "s3", T0).await.unwrap(),
        "the fourth is refused"
    );
    assert_eq!(
        c.count("ana", T0).await.unwrap(),
        3,
        "a refused socket isn't counted"
    );
    assert!(c.admit("bob", "b0", T0).await.unwrap(), "per member");
    c.release("ana", "s1").await.unwrap();
    assert!(
        c.admit("ana", "s3", T0).await.unwrap(),
        "a released slot is free"
    );
}

#[tokio::test]
async fn the_count_is_a_sorted_set_of_socket_ids_scored_by_expiry() {
    let store = MemoryStore::new();
    let c = SocketCounter::new(store.clone(), 20);
    assert!(c.admit("ana", "s0", T0).await.unwrap());
    assert!(c.admit("ana", "s1", T0).await.unwrap());
    // S6: the key is the member's sorted set; its members are the socket ids.
    assert_eq!(store.zcard(&socket_key("ana")).await.unwrap(), 2);
    store.zrem(&socket_key("ana"), "s0").await.unwrap();
    assert_eq!(c.count("ana", T0).await.unwrap(), 1);
}

#[tokio::test]
async fn a_crashed_instances_sockets_heal_after_90_seconds() {
    let store = MemoryStore::new();
    let c = SocketCounter::new(store.clone(), 2);
    assert!(c.admit("ana", "crashed-1", T0).await.unwrap());
    assert!(c.admit("ana", "crashed-2", T0).await.unwrap());
    let ttl = SOCKET_ENTRY_TTL.as_millis() as u64;
    assert!(
        !c.admit("ana", "new", T0 + ttl - 1_000).await.unwrap(),
        "89 s: still counted"
    );
    assert!(
        c.admit("ana", "new", T0 + ttl + 1).await.unwrap(),
        "past 90 s: trimmed"
    );
    assert_eq!(c.count("ana", T0 + ttl + 1).await.unwrap(), 1);
}

#[tokio::test]
async fn a_refreshed_socket_stays_counted() {
    let store = MemoryStore::new();
    let c = SocketCounter::new(store.clone(), 2);
    assert!(c.admit("ana", "live", T0).await.unwrap());
    assert!(c.admit("ana", "dead", T0).await.unwrap());
    c.refresh("ana", "live", T0 + 60_000).await.unwrap();
    let later = T0 + 120_000;
    assert_eq!(
        c.count("ana", later).await.unwrap(),
        1,
        "only the refreshed one is left"
    );
}

#[tokio::test]
async fn concurrent_admissions_never_exceed_the_limit() {
    // Two instances (or one with many tabs) admitting at once.
    let store = MemoryStore::new();
    let a = SocketCounter::new(store.clone(), 5);
    let b = SocketCounter::new(store.clone(), 5);
    let admits = (0..40).map(|i| {
        let c = if i % 2 == 0 { a.clone() } else { b.clone() };
        async move { c.admit("ana", &format!("s{i}"), T0).await.unwrap() }
    });
    let admitted = join_all(admits).await.into_iter().filter(|ok| *ok).count();
    assert!(admitted <= 5, "{admitted} admitted");
    assert!(admitted >= 1);
    assert_eq!(
        a.count("ana", T0).await.unwrap() as usize,
        admitted,
        "refused sockets left nothing behind"
    );
}

#[tokio::test]
async fn valkey_down_fails_the_count() {
    let store = MemoryStore::new();
    let c = SocketCounter::new(store.clone(), 2);
    store.fail(true);
    assert!(c.admit("ana", "s0", T0).await.is_err());
    let t = TicketRate::new(store.clone(), 10);
    assert!(t.allow("ana", T0).await.is_err());
}

#[tokio::test]
async fn tickets_are_limited_per_member_per_second() {
    let store = MemoryStore::new();
    let a = TicketRate::new(store.clone(), 10);
    let b = TicketRate::new(store.clone(), 10);
    for i in 0..10 {
        let rate = if i % 2 == 0 { &a } else { &b };
        assert!(rate.allow("ana", T0 + i).await.unwrap(), "ticket {i}");
    }
    assert!(
        !a.allow("ana", T0 + 500).await.unwrap(),
        "the 11th in the second, on any instance"
    );
    assert!(b.allow("bob", T0 + 500).await.unwrap(), "per member");
    assert!(a.allow("ana", T0 + 1_000).await.unwrap(), "the next second");
    let ttl = store
        .ttl_of(&ticket_rate_key("ana", T0 / 1000))
        .expect("the counter expires");
    assert!(ttl <= Duration::from_secs(5), "{ttl:?}");
}

#[tokio::test]
async fn store_counters_increment_and_expire() {
    // The fake's INCR/EXPIRE, which the rate tests rely on.
    let store: Arc<MemoryStore> = MemoryStore::new();
    assert_eq!(
        store
            .incr("rt:cnt:x", Duration::from_secs(2))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .incr("rt:cnt:x", Duration::from_secs(2))
            .await
            .unwrap(),
        2
    );
    assert!(store.ttl_of("rt:cnt:x").is_some());
}
