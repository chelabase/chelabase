//! RT4-3: the storage-token refresh pass and loop, over the fakes. A pass mints a new expiring
//! token for every running compute that advertises `storage_token_refresh`, under the
//! endpoint's lock, through `POST /storage_token` (`SpecPusher::push_storage_token`), never a
//! spec push.

use std::sync::Arc;
use std::time::Duration;

use chelabase_neon::jwt::STORAGE_TOKEN_TTL;

use super::{claims, ready_named, tenant_of};
use crate::fakes::{FakeWorker, entries, position, worker};
use crate::ports::WorkerMetadata;
use crate::token_refresh::{RefreshPass, storage_token_refresh_loop};

const NOW: u64 = 1_800_000_000;

fn tokens(w: &FakeWorker) -> Vec<(String, String)> {
    w.pusher.storage_tokens.lock().unwrap().clone()
}

fn spec_pushes(w: &FakeWorker) -> usize {
    w.pusher.pushed.lock().unwrap().len()
}

#[tokio::test]
async fn a_pass_pushes_an_expiring_token_through_storage_token_under_the_lock() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    journal.lock().unwrap().clear();
    let specs_before = spec_pushes(&w);
    let sql_before = w.computes.sql.lock().unwrap().len();

    let pass = w.refresh_storage_tokens_once(NOW).await;

    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 1,
            failed: 0,
            skipped: 0
        }
    );
    let lock = position(&journal, "metadata.lock_endpoint").expect("locked");
    let report = position(&journal, &format!("pusher.report {ep}")).expect("asked first");
    let push = position(&journal, &format!("pusher.storage_token {ep}")).expect("refreshed");
    assert!(lock < report && report < push, "{:?}", entries(&journal));
    // RT4-3: never a spec push, no SQL, no status change.
    assert!(
        position(&journal, "pusher.push ").is_none(),
        "{:?}",
        entries(&journal)
    );
    assert_eq!(spec_pushes(&w), specs_before);
    assert_eq!(w.computes.sql.lock().unwrap().len(), sql_before);
    let sent = tokens(&w);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, ep);
    let c = claims(&sent[0].1);
    assert_eq!(c["tenant_id"], tenant_of(&w, &ep));
    assert_eq!(c["scope"], "tenant");
    assert_eq!(c["iat"], NOW);
    assert_eq!(c["exp"], NOW + STORAGE_TOKEN_TTL.as_secs());
}

#[tokio::test]
async fn two_passes_mint_different_tokens() {
    let (w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);

    w.refresh_storage_tokens_once(NOW).await;
    w.refresh_storage_tokens_once(NOW + 3600).await;

    let sent = tokens(&w);
    assert_eq!(sent.len(), 2);
    assert_ne!(sent[0].1, sent[1].1);
    assert_eq!(
        claims(&sent[1].1)["exp"],
        NOW + 3600 + STORAGE_TOKEN_TTL.as_secs()
    );
}

#[tokio::test]
async fn each_compute_gets_a_token_for_its_own_tenant() {
    let (w, _journal) = worker();
    let a = ready_named(&w, "a").await;
    let b = ready_named(&w, "b").await;
    for ep in [&a, &b] {
        w.pusher.set_capability(ep, true);
    }
    assert_ne!(tenant_of(&w, &a), tenant_of(&w, &b));

    let pass = w.refresh_storage_tokens_once(NOW).await;

    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 2,
            failed: 0,
            skipped: 0
        }
    );
    for (ep, token) in tokens(&w) {
        assert_eq!(claims(&token)["tenant_id"], tenant_of(&w, &ep), "{ep}");
    }
}

#[tokio::test]
async fn the_pass_waits_for_the_endpoint_lock() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    let held = w.metadata.lock_endpoint(&ep).await.unwrap();
    journal.lock().unwrap().clear();

    {
        let pass = w.refresh_storage_tokens_once(NOW);
        tokio::pin!(pass);
        let waited = tokio::time::timeout(Duration::from_millis(100), &mut pass).await;
        assert!(
            waited.is_err(),
            "the pass ran while another job held the endpoint's lock"
        );
        assert!(
            position(&journal, "pusher.storage_token").is_none(),
            "{:?}",
            entries(&journal)
        );
        drop(held);
        let done = tokio::time::timeout(Duration::from_secs(5), &mut pass)
            .await
            .expect("the pass finishes once the lock is free");
        assert_eq!(done.refreshed, 1);
    }
}

#[tokio::test]
async fn stopped_computes_and_computes_without_the_capability_are_skipped() {
    let (w, journal) = worker();
    let capable = ready_named(&w, "capable").await;
    let stopped = ready_named(&w, "stopped").await;
    let old = ready_named(&w, "old").await;
    w.pusher.set_capability(&capable, true);
    w.pusher.set_capability(&stopped, true);
    w.computes.set_running(&stopped, false);
    // `old` is a compute_ctl that advertises nothing (the fake's default).
    journal.lock().unwrap().clear();

    let pass = w.refresh_storage_tokens_once(NOW).await;

    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 1,
            failed: 0,
            skipped: 2
        }
    );
    let sent = tokens(&w);
    assert_eq!(
        sent.iter().map(|(ep, _)| ep.as_str()).collect::<Vec<_>>(),
        [capable.as_str()]
    );
    assert!(
        position(&journal, &format!("pusher.storage_token {stopped}")).is_none(),
        "{:?}",
        entries(&journal)
    );
    assert!(
        position(&journal, &format!("pusher.storage_token {old}")).is_none(),
        "{:?}",
        entries(&journal)
    );
    // Neither skipped compute counts as a failure.
    assert_eq!(w.storage_token_failure_count(&stopped), 0);
    assert_eq!(w.storage_token_failure_count(&old), 0);
}

#[tokio::test]
async fn a_failure_is_counted_per_endpoint_retried_and_reset_by_a_success() {
    let (w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    w.pusher
        .fail_storage_token_for(&ep, Some("compute_ctl said 500".into()));

    for expected in 1..=3 {
        let pass = w.refresh_storage_tokens_once(NOW).await;
        assert_eq!(
            pass,
            RefreshPass {
                refreshed: 0,
                failed: 1,
                skipped: 0
            }
        );
        assert_eq!(w.storage_token_failure_count(&ep), expected);
    }
    assert!(tokens(&w).is_empty());

    w.pusher.fail_storage_token_for(&ep, None);
    let pass = w.refresh_storage_tokens_once(NOW).await;
    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 1,
            failed: 0,
            skipped: 0
        }
    );
    assert_eq!(
        w.storage_token_failure_count(&ep),
        0,
        "a success resets the count"
    );
    assert_eq!(tokens(&w).len(), 1, "retried at the next pass");
}

#[tokio::test]
async fn a_412_counts_and_is_retried() {
    // SP7: compute_ctl answers 412 outside running/configuration_pending (e.g. while the start
    // path is still writing the file). It is a failure like any other, never a fatal one.
    let (w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    w.pusher.fail_storage_token_for(
        &ep,
        Some(
            "POST /storage_token of compute x: 412 Precondition Failed: the compute is starting"
                .into(),
        ),
    );

    let pass = w.refresh_storage_tokens_once(NOW).await;
    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 0,
            failed: 1,
            skipped: 0
        }
    );
    assert_eq!(w.storage_token_failure_count(&ep), 1);

    w.pusher.fail_storage_token_for(&ep, None);
    let pass = w.refresh_storage_tokens_once(NOW).await;
    assert_eq!(pass.refreshed, 1);
    assert_eq!(w.storage_token_failure_count(&ep), 0);
}

#[tokio::test]
async fn one_failing_compute_does_not_stop_the_others_and_counts_stay_separate() {
    let (w, _journal) = worker();
    let a = ready_named(&w, "a").await;
    let b = ready_named(&w, "b").await;
    for ep in [&a, &b] {
        w.pusher.set_capability(ep, true);
    }
    // The fake lists endpoints in id order; fail whichever sorts first.
    let (first, second) = if a < b { (&a, &b) } else { (&b, &a) };
    w.pusher
        .fail_storage_token_for(first, Some("compute_ctl said 500".into()));

    let pass = w.refresh_storage_tokens_once(NOW).await;

    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 1,
            failed: 1,
            skipped: 0
        }
    );
    assert_eq!(w.storage_token_failure_count(first), 1);
    assert_eq!(w.storage_token_failure_count(second), 0);
    assert_eq!(tokens(&w)[0].0, *second);
}

#[tokio::test]
async fn a_failed_report_is_a_counted_failure_and_pushes_nothing() {
    let (w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    *w.pusher.fail_report.lock().unwrap() = Some("compute_ctl is unreachable".into());

    let pass = w.refresh_storage_tokens_once(NOW).await;

    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 0,
            failed: 1,
            skipped: 0
        }
    );
    assert_eq!(w.storage_token_failure_count(&ep), 1);
    assert!(tokens(&w).is_empty());
}

#[tokio::test(start_paused = true)]
async fn report_and_push_storage_token_are_bounded_and_release_the_lock() {
    let (w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);

    for knob in ["report", "storage_token"] {
        match knob {
            "report" => *w.pusher.hang_report.lock().unwrap() = true,
            _ => *w.pusher.hang_storage_token.lock().unwrap() = true,
        }
        let pass =
            tokio::time::timeout(Duration::from_secs(60), w.refresh_storage_tokens_once(NOW))
                .await
                .unwrap_or_else(|_| {
                    panic!("a hung {knob} stalled the pass (and the endpoint's lock)")
                });
        assert_eq!(pass.failed, 1, "{knob}");
        *w.pusher.hang_report.lock().unwrap() = false;
        *w.pusher.hang_storage_token.lock().unwrap() = false;
    }
    assert_eq!(w.storage_token_failure_count(&ep), 2);
    // The lock was released both times.
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

#[tokio::test]
async fn an_endpoint_without_a_metadata_row_is_not_refreshed() {
    // A container no endpoint row records (an orphan reconcile hasn't removed yet): there is no
    // tenant to mint for.
    let (w, _journal) = worker();
    w.computes.insert("ep-ffffffffffff");
    w.pusher.set_capability("ep-ffffffffffff", true);

    let pass = w.refresh_storage_tokens_once(NOW).await;

    assert_eq!(pass.refreshed, 0);
    assert!(tokens(&w).is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_loop_runs_a_pass_at_start_then_every_interval_and_survives_failures() {
    let (mut w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    w.storage_token_refresh_every = Duration::from_secs(600);
    w.pusher
        .fail_storage_token_for(&ep, Some("compute_ctl said 500".into()));
    let w = Arc::new(w);
    let handle = tokio::spawn(storage_token_refresh_loop(w.clone()));

    // Start: one pass, failing; the loop goes on.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.storage_token_failure_count(&ep), 1);
    assert!(tokens(&w).is_empty());

    // The next pass is one interval later, not sooner.
    w.pusher.fail_storage_token_for(&ep, None);
    tokio::time::sleep(Duration::from_secs(500)).await;
    assert!(tokens(&w).is_empty(), "ran before its interval");
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(tokens(&w).len(), 1, "the retry at the next pass");
    assert_eq!(w.storage_token_failure_count(&ep), 0);

    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(tokens(&w).len(), 2);
    handle.abort();
}
