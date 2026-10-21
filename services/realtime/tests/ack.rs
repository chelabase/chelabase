//! RT1 T8: acknowledgements (spec §3.3 step 3, W8, P14): in order, only after publish, the
//! keepalive's `wal_end` when idle, never backwards; the lag watchdog (64 MB or 60 s).

use std::time::Duration;

use realtime::ack::{Acks, LAG_MAX_AGE, LAG_MAX_BYTES};
use tokio::time::Instant;
use tokio_postgres::types::PgLsn;

fn l(n: u64) -> PgLsn {
    PgLsn::from(n)
}

#[test]
fn an_idle_slot_confirms_the_keepalives_wal_end() {
    let mut a = Acks::new(l(100));
    a.keepalive(l(5_000));
    assert_eq!(
        a.confirmed(),
        l(5_000),
        "a quiet watched database advances confirmed_flush_lsn"
    );
    a.keepalive(l(9_000));
    assert_eq!(a.confirmed(), l(9_000));
}

#[test]
fn a_transaction_is_confirmed_only_once_published() {
    let mut a = Acks::new(l(100));
    let t0 = Instant::now();
    a.received(l(200), t0);
    assert_eq!(a.in_flight(), 1);
    a.keepalive(l(300));
    assert_eq!(
        a.confirmed(),
        l(100),
        "nothing past an unpublished transaction, keepalive or not"
    );
    a.published(l(200));
    assert_eq!(a.confirmed(), l(200));
    assert_eq!(a.in_flight(), 0);
    a.keepalive(l(300));
    assert_eq!(a.confirmed(), l(300), "idle again: the keepalive counts");
}

#[test]
fn confirmations_stay_in_order() {
    let mut a = Acks::new(l(100));
    let t0 = Instant::now();
    a.received(l(200), t0);
    a.received(l(300), t0);
    a.received(l(400), t0);
    a.published(l(300));
    assert_eq!(
        a.confirmed(),
        l(300),
        "publishing up to 300 publishes 200 too"
    );
    assert_eq!(a.in_flight(), 1);
    a.keepalive(l(1_000));
    assert_eq!(a.confirmed(), l(300), "400 is still in flight");
    a.published(l(400));
    assert_eq!(a.confirmed(), l(400));
}

#[test]
fn the_confirmed_lsn_never_goes_back() {
    let mut a = Acks::new(l(100));
    a.keepalive(l(500));
    assert_eq!(a.confirmed(), l(500));
    a.keepalive(l(400));
    a.published(l(300));
    assert_eq!(a.confirmed(), l(500));
}

#[test]
fn the_watchdog_trips_on_bytes_behind_while_something_is_in_flight() {
    let mut a = Acks::new(l(0));
    let t0 = Instant::now();
    a.received(l(10), t0);
    assert!(
        a.lagging(l(10 + LAG_MAX_BYTES + 1), t0),
        "more than 64 MB past the confirmed LSN"
    );
    assert!(!a.lagging(l(10 + LAG_MAX_BYTES - 1), t0));
    assert_eq!(LAG_MAX_BYTES, 64 * 1024 * 1024);
}

#[test]
fn the_watchdog_trips_on_an_old_unpublished_transaction() {
    let mut a = Acks::new(l(0));
    let t0 = Instant::now();
    a.received(l(10), t0);
    assert!(a.lagging(l(20), t0 + LAG_MAX_AGE + Duration::from_millis(1)));
    assert!(!a.lagging(l(20), t0 + LAG_MAX_AGE - Duration::from_secs(1)));
    assert_eq!(LAG_MAX_AGE, Duration::from_secs(60));
}

#[test]
fn an_idle_slot_never_lags() {
    let mut a = Acks::new(l(0));
    let t0 = Instant::now();
    a.received(l(10), t0);
    a.published(l(10));
    a.keepalive(l(10 + 10 * LAG_MAX_BYTES));
    assert_eq!(a.confirmed(), l(10 + 10 * LAG_MAX_BYTES));
    assert!(!a.lagging(l(10 + 10 * LAG_MAX_BYTES), t0 + Duration::from_secs(3_600)));
}
