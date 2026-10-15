//! U11 and T9 m6: how the worker waits for a new compute.
//!
//! - Only compute_ctl's fatal-migration error ends the wait at once (`migration N failed: …`,
//!   `migrations could not run: …`, as `MigrationFailure` displays them); any other `failed`
//!   keeps polling to the deadline (the container's `unless-stopped` restart recovers a
//!   transient start error), and the last error is what the wait reports.
//! - While compute_ctl reports a live, non-failed state (`init`, `configuration_pending`, …) the
//!   wait goes on past `ready_timeout`, up to `ready_cap` (5 minutes in production: more than
//!   RT3-14's 2-minute migration timeout); no other state extends it.

use std::time::{Duration, Instant};

use chelabase_neon::ComputeBackend;
use chelabase_neon::reconfigure::ComputeCtlStatus;

use crate::fakes::worker;
use crate::provision::{READY_CAP, READY_TIMEOUT, is_fatal_migration_error};

#[test]
fn only_the_fatal_migration_errors_are_fatal() {
    for fatal in [
        "migration 2 failed: boom",
        "migration 12 failed: relation \"x\" does not exist",
        "migrations could not run: connection refused",
        // compute_ctl may wrap the error it reports.
        "compute start failed: migration 3 failed: deadlock detected",
        "Failed to run migrations: migrations could not run: timed out",
    ] {
        assert!(is_fatal_migration_error(fatal), "{fatal:?}");
    }
    for transient in [
        "",
        "postgres exited with code 1",
        "could not connect to the pageserver",
        "compute_ctl reported failed without an error",
        "migrationX failed",
        "migration failed",
        "the 2nd migration is fine",
        "walproposer: timeout waiting for safekeepers",
    ] {
        assert!(!is_fatal_migration_error(transient), "{transient:?}");
    }
}

#[test]
fn the_wait_is_capped_at_five_minutes() {
    assert_eq!(READY_CAP, Duration::from_secs(300));
    assert!(
        READY_CAP > Duration::from_secs(120),
        "longer than RT3-14's migration timeout"
    );
    assert!(READY_TIMEOUT <= READY_CAP);
}

#[tokio::test]
async fn a_fatal_migration_error_ends_the_wait_at_once() {
    for message in [
        "migration 4 failed: boom",
        "migrations could not run: no connection",
    ] {
        let (mut w, _journal) = worker();
        w.ready_timeout = Duration::from_secs(5);
        w.ready_cap = Duration::from_secs(10);
        w.computes.create("ep-f", "t").await.unwrap();
        w.pusher
            .set_status("ep-f", ComputeCtlStatus::Failed(message.into()));
        let started = Instant::now();
        let err = w.wait_until_ready("ep-f", "app").await.unwrap_err();
        assert!(err.contains(message), "{err}");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{message}: {:?}",
            started.elapsed()
        );
    }
}

#[tokio::test]
async fn any_other_failure_keeps_polling_to_the_deadline_and_is_reported() {
    let (mut w, _journal) = worker();
    w.ready_timeout = Duration::from_millis(200);
    w.computes.create("ep-t", "t").await.unwrap();
    w.pusher.set_status(
        "ep-t",
        ComputeCtlStatus::Failed("postgres exited with code 1".into()),
    );
    let started = Instant::now();
    let err = w.wait_until_ready("ep-t", "app").await.unwrap_err();
    assert!(
        started.elapsed() >= Duration::from_millis(190),
        "gave up early: {:?}",
        started.elapsed()
    );
    assert!(
        err.contains("postgres exited with code 1"),
        "the last error is reported: {err}"
    );
}

#[tokio::test]
async fn a_transient_failure_that_recovers_within_the_deadline_is_ready() {
    let (mut w, _journal) = worker();
    w.ready_timeout = Duration::from_secs(5);
    w.computes.create("ep-r", "t").await.unwrap();
    w.pusher.script_statuses(
        "ep-r",
        vec![
            ComputeCtlStatus::Failed("could not connect to the pageserver".into()),
            ComputeCtlStatus::Failed("could not connect to the pageserver".into()),
            ComputeCtlStatus::Other("init".into()),
        ],
    );
    // Then `Running` (the fake's default).
    w.wait_until_ready("ep-r", "app").await.unwrap();
}

#[tokio::test]
async fn the_error_reported_is_the_last_one_seen() {
    let (mut w, _journal) = worker();
    w.ready_timeout = Duration::from_millis(150);
    w.computes.create("ep-l", "t").await.unwrap();
    w.pusher.script_statuses(
        "ep-l",
        vec![ComputeCtlStatus::Failed("first problem".into())],
    );
    w.pusher.set_status(
        "ep-l",
        ComputeCtlStatus::Other("configuration_pending".into()),
    );
    let err = w.wait_until_ready("ep-l", "app").await.unwrap_err();
    assert!(err.contains("configuration_pending"), "{err}");
    assert!(!err.contains("first problem"), "{err}");
}

#[tokio::test]
async fn an_unreachable_compute_ctl_is_polled_until_the_deadline() {
    let (mut w, _journal) = worker();
    w.ready_timeout = Duration::from_millis(150);
    w.ready_cap = Duration::from_secs(30);
    w.computes.create("ep-u", "t").await.unwrap();
    *w.pusher.fail_status.lock().unwrap() = Some("connection refused".into());
    let started = Instant::now();
    let err = w.wait_until_ready("ep-u", "app").await.unwrap_err();
    assert!(err.contains("connection refused"), "{err}");
    assert!(
        started.elapsed() >= Duration::from_millis(140),
        "{:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "an unreachable compute_ctl doesn't extend the wait: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_live_state_extends_the_wait_up_to_the_cap() {
    // Migrations take longer than `ready_timeout`: compute_ctl keeps reporting a live state, and
    // the wait follows it to `running`.
    let (mut w, _journal) = worker();
    w.ready_timeout = Duration::from_millis(100);
    w.ready_cap = Duration::from_secs(10);
    w.computes.create("ep-m", "t").await.unwrap();
    w.pusher
        .script_statuses("ep-m", vec![ComputeCtlStatus::Other("init".into()); 300]);
    let started = Instant::now();
    w.wait_until_ready("ep-m", "app").await.unwrap();
    assert!(
        started.elapsed() > Duration::from_millis(150),
        "the script outlasts ready_timeout, so the wait went past it: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_state_that_never_leaves_live_gives_up_at_the_cap() {
    let (mut w, _journal) = worker();
    w.ready_timeout = Duration::from_millis(100);
    w.ready_cap = Duration::from_millis(600);
    w.computes.create("ep-c", "t").await.unwrap();
    w.pusher.set_status(
        "ep-c",
        ComputeCtlStatus::Other("configuration_pending".into()),
    );
    let started = Instant::now();
    let err = w.wait_until_ready("ep-c", "app").await.unwrap_err();
    assert!(err.contains("configuration_pending"), "{err}");
    assert!(
        started.elapsed() >= Duration::from_millis(550),
        "{:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_failed_state_isnt_live_and_doesnt_extend_the_wait() {
    let (mut w, _journal) = worker();
    w.ready_timeout = Duration::from_millis(100);
    w.ready_cap = Duration::from_secs(30);
    w.computes.create("ep-x", "t").await.unwrap();
    w.pusher.set_status(
        "ep-x",
        ComputeCtlStatus::Failed("postgres exited with code 1".into()),
    );
    let started = Instant::now();
    assert!(w.wait_until_ready("ep-x", "app").await.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}
