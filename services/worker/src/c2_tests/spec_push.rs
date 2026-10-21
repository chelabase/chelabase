//! RT4-2 and SP2/SP4: what the worker asks the compute before each spec push, and what the push
//! carries. Driven through `apply_pending` (reset's and reconcile's push), over the fakes.

use std::time::Duration;

use chelabase_neon::jwt::STORAGE_TOKEN_TTL;
use chelabase_neon::spec::MemberLogins;

use super::{claims, last_pushed_token, ready_named, tenant_of};
use crate::fakes::{entries, position, worker};

fn pushes(w: &crate::fakes::FakeWorker) -> usize {
    w.pusher.pushed.lock().unwrap().len()
}

#[tokio::test]
async fn a_capable_compute_is_asked_first_and_gets_an_expiring_token() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    journal.lock().unwrap().clear();

    w.apply_pending(&ep).await.unwrap();

    let lock = position(&journal, "metadata.lock_endpoint").expect("locked");
    let report = position(&journal, &format!("pusher.report {ep}"))
        .unwrap_or_else(|| panic!("no report() before the push: {:?}", entries(&journal)));
    let push = position(&journal, &format!("pusher.push {ep}")).expect("pushed");
    assert!(lock < report && report < push, "{:?}", entries(&journal));
    let c = claims(&last_pushed_token(&w));
    assert_eq!(c["tenant_id"], tenant_of(&w, &ep));
    assert_eq!(c["scope"], "tenant");
    let (iat, exp) = (c["iat"].as_u64().unwrap(), c["exp"].as_u64().unwrap());
    assert_eq!(exp - iat, STORAGE_TOKEN_TTL.as_secs(), "{c}");
}

#[tokio::test]
async fn a_compute_that_does_not_advertise_the_capability_keeps_a_non_expiring_token() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    // The fake's default is an old compute_ctl: no capability.
    journal.lock().unwrap().clear();

    w.apply_pending(&ep).await.unwrap();

    assert!(
        position(&journal, &format!("pusher.report {ep}")).is_some(),
        "the worker asks every compute: {:?}",
        entries(&journal)
    );
    let c = claims(&last_pushed_token(&w));
    assert!(
        c.get("exp").is_none(),
        "an old compute must not get an expiring token: {c}"
    );
    assert_eq!(c["tenant_id"], tenant_of(&w, &ep));
}

#[tokio::test]
async fn every_push_asks_again() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    journal.lock().unwrap().clear();

    w.apply_pending(&ep).await.unwrap();
    w.apply_pending(&ep).await.unwrap();

    let reports = entries(&journal)
        .iter()
        .filter(|e| **e == format!("pusher.report {ep}"))
        .count();
    assert_eq!(reports, 2, "{:?}", entries(&journal));
}

#[tokio::test]
async fn a_compute_that_stops_advertising_gets_a_non_expiring_token_at_its_next_push() {
    // An image rolled back under a running worker (RT4-2).
    let (w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    w.apply_pending(&ep).await.unwrap();
    assert!(claims(&last_pushed_token(&w)).get("exp").is_some());

    w.pusher.set_capability(&ep, false);
    w.apply_pending(&ep).await.unwrap();

    let c = claims(&last_pushed_token(&w));
    assert!(c.get("exp").is_none(), "{c}");
}

#[tokio::test]
async fn a_failed_report_fails_the_push_and_never_falls_back_to_a_non_expiring_token() {
    // SP2: for a capable compute a silent fallback would hand out a token that never expires.
    for capable in [true, false] {
        let (w, journal) = worker();
        let ep = ready_named(&w, "app").await;
        w.pusher.set_capability(&ep, capable);
        *w.pusher.fail_report.lock().unwrap() = Some("compute_ctl is unreachable".into());
        journal.lock().unwrap().clear();
        let before = pushes(&w);

        let err = w.apply_pending(&ep).await.unwrap_err();

        assert!(
            format!("{err:#}").contains("compute_ctl is unreachable"),
            "capable={capable}: {err:#}"
        );
        assert_eq!(pushes(&w), before, "capable={capable}: nothing was pushed");
        assert!(
            position(&journal, "pusher.push ").is_none(),
            "capable={capable}: {:?}",
            entries(&journal)
        );
        // The push failed, so the cache isn't told and the flag isn't cleared.
        assert!(position(&journal, "cache.password_updated").is_none());
        assert!(position(&journal, "metadata.clear_needs_reconfigure").is_none());
        // Retried as a push failure is: the next attempt, with compute_ctl back, succeeds.
        *w.pusher.fail_report.lock().unwrap() = None;
        w.apply_pending(&ep).await.unwrap();
        assert_eq!(pushes(&w), before + 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_hung_report_is_bounded_and_fails_the_push() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    *w.pusher.hang_report.lock().unwrap() = true;
    journal.lock().unwrap().clear();

    let err = tokio::time::timeout(Duration::from_secs(60), w.apply_pending(&ep))
        .await
        .expect("report() is bounded by DOCKER_TIMEOUT")
        .unwrap_err();

    assert!(!format!("{err:#}").is_empty());
    assert!(
        position(&journal, "pusher.push ").is_none(),
        "{:?}",
        entries(&journal)
    );
    // The endpoint's lock was released: nothing waits on it once compute_ctl answers again.
    *w.pusher.hang_report.lock().unwrap() = false;
    w.apply_pending(&ep).await.unwrap();
}

#[tokio::test]
async fn the_push_carries_the_workers_realtime_logins_not_the_interim_value() {
    // SP4: C1 passed `MemberLogins::realtime(Vec::new())` to compile; the worker's own
    // `CHELABASE_REALTIME_LOGIN_CIDRS` replaces it.
    let (mut w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    let logins = MemberLogins::realtime(vec!["10.213.4.12/32".into(), "10.213.4.11/32".into()]);
    w.realtime_logins = logins.clone();

    w.apply_pending(&ep).await.unwrap();

    let pushed = w.pusher.pushed.lock().unwrap();
    let spec = &pushed.last().unwrap().1["spec"];
    assert_eq!(
        spec["realtime_logins"],
        serde_json::to_value(&logins).unwrap()
    );
    assert_eq!(
        spec["realtime_logins"]["allowed_cidrs"],
        serde_json::json!(["10.213.4.12/32", "10.213.4.11/32"])
    );
    assert_eq!(
        spec["realtime_logins"]["client_cns"],
        serde_json::json!(["chelabase-realtime"])
    );
    // The member logins are a separate setting.
    assert_ne!(spec["member_logins"], spec["realtime_logins"]);
}

#[tokio::test]
async fn an_empty_cidr_list_is_pushed_as_is_not_replaced() {
    // Empty means "no address rule" (certificate only): the worker must not substitute a default.
    let (mut w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.realtime_logins = MemberLogins::realtime(Vec::new());

    w.apply_pending(&ep).await.unwrap();

    let pushed = w.pusher.pushed.lock().unwrap();
    assert_eq!(
        pushed.last().unwrap().1["spec"]["realtime_logins"]["allowed_cidrs"],
        serde_json::json!([])
    );
}
