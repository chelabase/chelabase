//! F4 audit findings (RED): the sticky wait stretch, the unreadable flag on an unprotected
//! branch, the `_` wildcard in the marker match, the bounded push, and the protection read
//! through one port call.

use std::time::{Duration, Instant};

use chelabase_nats::MemberRolesReply;
use chelabase_neon::ComputeBackend;
use chelabase_neon::reconfigure::ComputeCtlStatus;

use super::{fresh, h, set_protected};
use crate::fakes::{entries, worker};
use crate::ports::WorkerMetadata as _;
use crate::member_roles::{BRANCH_PROTECTED_CHECK_MARK, member_catalog_sql};

// --- 1. the stretch to `ready_cap` is sticky, and only live states stretch it -------------

#[tokio::test]
async fn once_live_a_failed_status_read_doesnt_end_the_stretch() {
    let (mut w, _journal) = worker();
    w.ready_timeout = Duration::from_millis(100);
    w.ready_cap = Duration::from_secs(2);
    w.computes.create("ep-s", "t").await.unwrap();
    // Live for longer than `ready_timeout`, then compute_ctl can't be read at all.
    w.pusher
        .script_statuses("ep-s", vec![ComputeCtlStatus::Other("init".into()); 200]);
    *w.pusher.fail_status_after_queue.lock().unwrap() = Some("connection refused".into());
    let started = Instant::now();
    let err = w.wait_until_ready("ep-s", "app").await.unwrap_err();
    assert!(err.contains("connection refused"), "{err}");
    assert!(
        started.elapsed() >= Duration::from_millis(1800),
        "one failed read after ready_timeout ended a wait that had seen compute_ctl live: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn once_live_a_recovery_after_unreadable_status_is_ready() {
    let (mut w, _journal) = worker();
    w.ready_timeout = Duration::from_millis(100);
    w.ready_cap = Duration::from_secs(5);
    w.computes.create("ep-s", "t").await.unwrap();
    let mut script = vec![ComputeCtlStatus::Other("init".into()); 200];
    script.push(ComputeCtlStatus::Running);
    w.pusher.script_statuses("ep-s", script);
    w.wait_until_ready("ep-s", "app").await.unwrap();
}

#[tokio::test]
async fn terminating_and_empty_are_not_live() {
    // compute_ctl's `ComputeStatus` names (snake_case): these never become `running`.
    for state in [
        "empty",
        "termination_pending_fast",
        "termination_pending_immediate",
        "terminated",
    ] {
        let (mut w, _journal) = worker();
        w.ready_timeout = Duration::from_millis(100);
        w.ready_cap = Duration::from_secs(3);
        w.computes.create("ep-t", "t").await.unwrap();
        w.pusher
            .set_status("ep-t", ComputeCtlStatus::Other(state.into()));
        let started = Instant::now();
        let err = w.wait_until_ready("ep-t", "app").await.unwrap_err();
        assert!(err.contains(state), "{err}");
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{state} stretched the wait: {:?}",
            started.elapsed()
        );
    }
}

#[tokio::test]
async fn init_and_configuration_still_stretch_the_wait() {
    for state in ["init", "configuration_pending", "configuration"] {
        let (mut w, _journal) = worker();
        w.ready_timeout = Duration::from_millis(100);
        w.ready_cap = Duration::from_millis(600);
        w.computes.create("ep-l", "t").await.unwrap();
        w.pusher
            .set_status("ep-l", ComputeCtlStatus::Other(state.into()));
        let started = Instant::now();
        assert!(w.wait_until_ready("ep-l", "app").await.is_err());
        assert!(started.elapsed() >= Duration::from_millis(550), "{state}");
    }
}

fn catalog() -> String {
    format!(
        "r {} f t f t\nd {} {}\n",
        h("postgres"),
        h("app"),
        h("postgres")
    )
}

// --- 3. `_` in a LIKE is a wildcard ---------------------------------------------------------

#[test]
fn the_marker_prefix_is_matched_literally() {
    let sql = member_catalog_sql();
    assert!(sql.contains("starts_with(c, 'neon.born_closed=')"), "{sql}");
    assert!(!sql.contains("LIKE 'neon.born_closed"), "{sql}");
}

// --- 4. the push is bounded, and builds its spec as reset.rs does ---------------------------

#[tokio::test(start_paused = true)]
async fn a_push_to_a_hung_docker_fails_in_time_and_frees_the_lock() {
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes
        .answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "off");
    *w.computes.hang_control_addr.lock().unwrap() = true;
    let reply = tokio::time::timeout(
        Duration::from_secs(60),
        w.reconcile_member_roles(branch, None),
    )
    .await
    .expect("the push must give up on Docker, like apply_pending does");
    assert!(
        matches!(reply, MemberRolesReply::Failed { .. }),
        "{reply:?}"
    );
    // The endpoint's lock is free again.
    tokio::time::timeout(Duration::from_secs(1), w.metadata.lock_endpoint(&ep))
        .await
        .expect("the lock was released")
        .unwrap();
}

#[tokio::test]
async fn the_reconcile_and_reset_push_the_same_spec() {
    // One spec-building helper behind both: the same state gives the same config.
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.apply_pending(&ep).await.unwrap();
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes
        .answer_once_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "off");
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
    assert!(matches!(
        w.reconcile_member_roles(branch, None).await,
        MemberRolesReply::Done { .. }
    ));
    let pushed = w.pusher.pushed.lock().unwrap();
    assert_eq!(pushed.len(), 2);
    assert_eq!(pushed[0].1, pushed[1].1);
}

// --- 5. the protection is read through one port call ---------------------------------------

fn calls(w: &crate::fakes::FakeWorker, journal: &crate::fakes::Journal) -> usize {
    let _ = w;
    entries(journal)
        .iter()
        .filter(|e| e.starts_with("metadata.endpoint_branch"))
        .count()
}

#[tokio::test]
async fn a_push_reads_the_branch_with_one_endpoint_branch_call_not_a_listing() {
    let (w, journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    // Listing every endpoint is no longer how the branch is found.
    w.metadata.state().fail = Some("list_endpoints_for_reconcile");
    w.apply_pending(&ep).await.unwrap();
    assert_eq!(calls(&w, &journal), 1);
    let pushed = w.pusher.pushed.lock().unwrap();
    assert_eq!(pushed.len(), 1);
    assert_eq!(
        pushed[0].1["spec"]["branch_protected"],
        serde_json::json!(true)
    );
}

#[tokio::test]
async fn a_deleting_branch_counts_as_not_found() {
    // Same as an endpoint with no branch (a static compute): not protected, no error. The
    // orchestrator-side `endpoint_branch` excludes deleting and deleted branches.
    let (w, journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.metadata.state().branches.get_mut(&branch).unwrap().status =
        chelabase_metadata::BranchStatus::Deleting;
    w.apply_pending(&ep).await.unwrap();
    assert_eq!(calls(&w, &journal), 1);
    assert_eq!(
        w.pusher.pushed.lock().unwrap()[0].1["spec"]["branch_protected"],
        serde_json::json!(false)
    );
}
