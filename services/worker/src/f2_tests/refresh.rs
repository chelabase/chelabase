//! The storage-token refresh pass (F2, part 2): failure counts of computes gone from the list
//! are dropped, a compute compute_ctl doesn't call `running` or `configuration_pending` is
//! skipped, and an endpoint whose lock stays taken is skipped, not failed.

use std::time::Duration;

use chelabase_neon::reconfigure::ComputeCtlStatus;

use crate::c2_tests::ready_named;
use crate::fakes::{FakeWorker, entries, position, worker};
use crate::ports::WorkerMetadata;
use crate::token_refresh::RefreshPass;

const NOW: u64 = 1_800_000_000;

fn tokens(w: &FakeWorker) -> Vec<(String, String)> {
    w.pusher.storage_tokens.lock().unwrap().clone()
}

fn failure_entries(w: &FakeWorker) -> Vec<String> {
    let mut eps: Vec<String> = w
        .storage_token_failures
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    eps.sort();
    eps
}

// A compute removed since its last failure leaves no count behind: the map holds only
// endpoints the last listing had.
#[tokio::test]
async fn failure_counts_of_computes_gone_from_the_list_are_dropped() {
    let (w, _journal) = worker();
    let a = ready_named(&w, "a").await;
    let b = ready_named(&w, "b").await;
    for ep in [&a, &b] {
        w.pusher.set_capability(ep, true);
        w.pusher
            .fail_storage_token_for(ep, Some("compute_ctl said 500".into()));
    }
    w.refresh_storage_tokens_once(NOW).await;
    assert_eq!(w.storage_token_failure_count(&a), 1);
    assert_eq!(w.storage_token_failure_count(&b), 1);

    w.computes.remove(&a);
    let pass = w.refresh_storage_tokens_once(NOW).await;

    assert_eq!(
        w.storage_token_failure_count(&a),
        0,
        "a is gone: its count with it"
    );
    assert_eq!(
        failure_entries(&w),
        std::slice::from_ref(&b),
        "no entry for a gone compute"
    );
    assert_eq!(
        w.storage_token_failure_count(&b),
        2,
        "b still fails: {pass:?}"
    );

    // A compute that is listed but skipped (stopped) keeps its count: it may come back.
    w.computes.set_running(&b, false);
    w.refresh_storage_tokens_once(NOW).await;
    assert_eq!(w.storage_token_failure_count(&b), 2);
}

// compute_ctl takes `POST /storage_token` only while `running` or `configuration_pending`
// (SP7): any other state is skipped (no push, no failure), not a counted 412.
#[tokio::test]
async fn a_capable_compute_in_another_state_is_skipped() {
    for (status, refreshed) in [
        (ComputeCtlStatus::Running, true),
        (
            ComputeCtlStatus::Other("configuration_pending".into()),
            true,
        ),
        (ComputeCtlStatus::Other("init".into()), false),
        (ComputeCtlStatus::Other("empty".into()), false),
        (ComputeCtlStatus::Other("termination_pending".into()), false),
        (ComputeCtlStatus::Other("terminated".into()), false),
        (ComputeCtlStatus::Other("Running".into()), false),
        (ComputeCtlStatus::Failed("migration 2 failed".into()), false),
    ] {
        let (w, journal) = worker();
        let ep = ready_named(&w, "app").await;
        w.pusher.set_capability(&ep, true);
        w.pusher
            .statuses
            .lock()
            .unwrap()
            .insert(ep.clone(), status.clone());
        journal.lock().unwrap().clear();

        let pass = w.refresh_storage_tokens_once(NOW).await;

        if refreshed {
            assert_eq!(
                pass,
                RefreshPass {
                    refreshed: 1,
                    failed: 0,
                    skipped: 0
                },
                "{status:?}"
            );
            assert_eq!(tokens(&w).len(), 1, "{status:?}");
        } else {
            assert_eq!(
                pass,
                RefreshPass {
                    refreshed: 0,
                    failed: 0,
                    skipped: 1
                },
                "{status:?}"
            );
            assert!(
                position(&journal, &format!("pusher.storage_token {ep}")).is_none(),
                "{status:?}: {:?}",
                entries(&journal)
            );
            assert_eq!(w.storage_token_failure_count(&ep), 0, "{status:?}");
        }
    }
}

// Another job holding the endpoint's lock past the wait's bound: that endpoint is skipped this
// pass (counted in `skipped`), not a failure, and the pass ends; the next pass refreshes it.
#[tokio::test(start_paused = true)]
async fn an_endpoint_whose_lock_stays_taken_is_skipped_not_failed() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    let held = w.metadata.lock_endpoint(&ep).await.unwrap();
    journal.lock().unwrap().clear();

    let pass = tokio::time::timeout(Duration::from_secs(60), w.refresh_storage_tokens_once(NOW))
        .await
        .expect("the wait for the lock is bounded");

    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 0,
            failed: 0,
            skipped: 1
        }
    );
    assert_eq!(
        w.storage_token_failure_count(&ep),
        0,
        "a busy endpoint isn't a failure"
    );
    assert!(
        position(&journal, "pusher.storage_token").is_none(),
        "{:?}",
        entries(&journal)
    );

    drop(held);
    let pass = w.refresh_storage_tokens_once(NOW).await;
    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 1,
            failed: 0,
            skipped: 0
        }
    );
}

// One busy endpoint doesn't hold up the others: they are refreshed in the same pass.
#[tokio::test(start_paused = true)]
async fn a_busy_endpoint_does_not_stop_the_others() {
    let (w, _journal) = worker();
    let a = ready_named(&w, "a").await;
    let b = ready_named(&w, "b").await;
    for ep in [&a, &b] {
        w.pusher.set_capability(ep, true);
    }
    let held = w.metadata.lock_endpoint(&a).await.unwrap();

    let pass = tokio::time::timeout(Duration::from_secs(60), w.refresh_storage_tokens_once(NOW))
        .await
        .expect("bounded");

    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 1,
            failed: 0,
            skipped: 1
        }
    );
    assert_eq!(
        tokens(&w)
            .iter()
            .map(|(ep, _)| ep.clone())
            .collect::<Vec<_>>(),
        std::slice::from_ref(&b)
    );
    drop(held);
}
