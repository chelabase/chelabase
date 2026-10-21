//! The storage-token refresh (F2, part 2): `now` is read for each endpoint once its lock is
//! held, and the loop goes on after a pass that panics.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chelabase_neon::jwt::STORAGE_TOKEN_TTL;

use crate::c2_tests::{claims, ready_named};
use crate::fakes::{FakeWorker, worker};
use crate::ports::WorkerMetadata;
use crate::token_refresh::{RefreshPass, storage_token_refresh_loop};

const NOW: u64 = 1_800_000_000;

fn tokens(w: &FakeWorker) -> Vec<(String, String)> {
    w.pusher.storage_tokens.lock().unwrap().clone()
}

// A pass that waited for an endpoint's lock mints with the time it got the lock, not the time
// the pass started: the token's `iat`/`exp` are the clock's after the wait.
#[tokio::test]
async fn now_is_read_for_each_endpoint_once_its_lock_is_held() {
    let (w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    let held = w.metadata.lock_endpoint(&ep).await.unwrap();
    let clock = Arc::new(AtomicU64::new(NOW));

    {
        let c = clock.clone();
        let pass = w.refresh_storage_tokens_with(move || c.load(Ordering::SeqCst));
        tokio::pin!(pass);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut pass)
                .await
                .is_err(),
            "the pass waits for the lock"
        );
        clock.store(NOW + 4_000, Ordering::SeqCst);
        drop(held);
        let done = tokio::time::timeout(Duration::from_secs(5), &mut pass)
            .await
            .expect("the pass finishes once the lock is free");
        assert_eq!(
            done,
            RefreshPass {
                refreshed: 1,
                failed: 0,
                skipped: 0
            }
        );
    }

    let sent = tokens(&w);
    let c = claims(&sent[0].1);
    assert_eq!(
        c["iat"],
        NOW + 4_000,
        "read after the lock, not at the pass's start: {c}"
    );
    assert_eq!(c["exp"], NOW + 4_000 + STORAGE_TOKEN_TTL.as_secs(), "{c}");
}

// Each endpoint reads the clock itself: two computes refreshed in one pass get the times the
// clock gave for each.
#[tokio::test]
async fn each_endpoint_reads_the_clock_itself() {
    let (w, _journal) = worker();
    let a = ready_named(&w, "a").await;
    let b = ready_named(&w, "b").await;
    for ep in [&a, &b] {
        w.pusher.set_capability(ep, true);
    }
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    let pass = w
        .refresh_storage_tokens_with(move || NOW + 1_000 * t.fetch_add(1, Ordering::SeqCst))
        .await;

    assert_eq!(pass.refreshed, 2);
    let read = ticks.load(Ordering::SeqCst);
    assert!(
        read >= 2,
        "the clock was read for each endpoint ({read} reads)"
    );
    let iats: Vec<u64> = tokens(&w)
        .iter()
        .map(|(_, t)| claims(t)["iat"].as_u64().unwrap())
        .collect();
    assert_ne!(iats[0], iats[1], "one clock read per endpoint: {iats:?}");
    for iat in &iats {
        assert!(
            (NOW..NOW + 1_000 * read).contains(iat) && (iat - NOW).is_multiple_of(1_000),
            "{iats:?}"
        );
    }
}

// `refresh_storage_tokens_once(now)` is the pass with a clock that always answers `now`.
#[tokio::test]
async fn the_once_pass_still_mints_with_the_given_now() {
    let (w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    w.refresh_storage_tokens_once(NOW).await;
    assert_eq!(claims(&tokens(&w)[0].1)["iat"], NOW);
}

fn set_panic(w: &FakeWorker, on: bool) {
    *w.pusher
        .panic_report
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = on;
}

// A pass that panics (a bug anywhere in it) doesn't end the loop: the next interval runs a
// pass again.
#[tokio::test(start_paused = true)]
async fn the_loop_survives_a_pass_that_panics() {
    let (mut w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    w.storage_token_refresh_every = Duration::from_secs(600);
    set_panic(&w, true);
    let w = Arc::new(w);
    let handle = tokio::spawn(storage_token_refresh_loop(w.clone()));

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !handle.is_finished(),
        "a panicking pass ended the refresh loop"
    );
    assert!(tokens(&w).is_empty());

    set_panic(&w, false);
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(!handle.is_finished());
    assert_eq!(tokens(&w).len(), 1, "the next pass ran and refreshed");
    // The endpoint's lock was released by the panicking pass.
    tokio::time::timeout(Duration::from_secs(1), w.metadata.lock_endpoint(&ep))
        .await
        .expect("the lock is free")
        .unwrap();
    handle.abort();
}
