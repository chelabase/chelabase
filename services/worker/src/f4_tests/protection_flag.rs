//! U14 (worker side): the branch's protection reaches the compute as `spec.branch_protected`.
//!
//! - every spec the worker pushes carries the branch's protection;
//! - protect: the flag goes on (a reconfigure) **before** any guard is installed, and the
//!   reconcile confirms it from a fresh session (`SHOW neon.branch_protected` = on: `pg_ctl
//!   reload` returns before every backend has absorbed SIGHUP) before it lists databases for the
//!   guard loop; a reconfigure that fails, or a flag that never takes effect, fails the protect
//!   and nothing is guarded or opened;
//! - unprotect: the guards go first, then the flag goes off; a failed unguard keeps the flag on,
//!   a failed reconfigure fails the run and the sweep retries it.

use std::sync::{Arc, Mutex};

use chelabase_nats::MemberRolesReply;

use super::{fresh, h, set_protected, sql_log};
use crate::fakes::{FakeWorker, entries};
use crate::member_roles::{BRANCH_PROTECTED_CHECK_MARK, branch_protected_check_sql};
use crate::schema_guard::{GUARD_PRESENT_MARK, UNGUARD_MARK};

fn catalog() -> String {
    format!(
        "r {} f t f t\nd {} {}\nd {} {}\ng {}\n",
        h("postgres"),
        h("app"),
        h("postgres"),
        h("analytics"),
        h("postgres"),
        h("template1"),
    )
}

/// Records how many SQL calls the compute saw (since now) when the pusher was called: an index
/// into [`sql_log`], which starts empty at the same moment.
fn note_sql_count_at_push(
    w: &FakeWorker,
    journal: &crate::fakes::Journal,
) -> Arc<Mutex<Option<usize>>> {
    let seen = Arc::new(Mutex::new(None));
    let sql_calls = |journal: &crate::fakes::Journal| {
        entries(journal)
            .iter()
            .filter(|e| e.starts_with("compute.sql"))
            .count()
    };
    let before = sql_calls(journal);
    let (journal, slot) = (journal.clone(), seen.clone());
    *w.pusher.on_push.lock().unwrap() = Some(Box::new(move || {
        Box::pin(async move {
            *slot.lock().unwrap() = Some(sql_calls(&journal) - before);
        })
    }));
    seen
}

fn pushed_flags(w: &FakeWorker) -> Vec<serde_json::Value> {
    w.pusher
        .pushed
        .lock()
        .unwrap()
        .iter()
        .map(|(_, config)| config["spec"]["branch_protected"].clone())
        .collect()
}

fn is_install(text: &str) -> bool {
    text.contains("CREATE EVENT TRIGGER")
}

fn is_open(text: &str) -> bool {
    text.contains("ALLOW_CONNECTIONS true")
}

#[test]
fn the_flag_check_is_a_show_of_the_guc() {
    let sql = branch_protected_check_sql();
    assert!(sql.contains(BRANCH_PROTECTED_CHECK_MARK), "{sql}");
    assert!(sql.contains("neon.branch_protected"), "{sql}");
    assert!(
        BRANCH_PROTECTED_CHECK_MARK.starts_with("-- chelabase:"),
        "a comment marker, like the other fakes' needles"
    );
}

#[tokio::test]
async fn protect_turns_the_flag_on_before_any_guard_and_confirms_it_before_listing_databases() {
    let (w, journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    // The compute starts with the flag off; once the spec lands it reads on.
    w.computes
        .answer_once_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "off");
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
    let at_push = note_sql_count_at_push(&w, &journal);

    let reply = w.reconcile_member_roles(branch, None).await;

    assert!(matches!(reply, MemberRolesReply::Done { .. }), "{reply:?}");
    assert_eq!(
        pushed_flags(&w),
        [serde_json::json!(true)],
        "one reconfigure, flag on"
    );
    let log = sql_log(&w);
    let calls_before_push = at_push.lock().unwrap().expect("the spec was pushed");
    assert!(
        log[..calls_before_push]
            .iter()
            .all(|(_, s)| !is_install(s) && !is_open(s)),
        "a guard was installed before the flag was on: {:?}",
        &log[..calls_before_push]
    );
    // The flag is read again from a fresh session after the push, and only then are the
    // databases listed.
    let checks: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, (_, s))| s.contains(BRANCH_PROTECTED_CHECK_MARK))
        .map(|(i, _)| i)
        .collect();
    let catalog_read = log
        .iter()
        .position(|(_, s)| s.contains("chelabase: member roles"))
        .expect("the catalog was read");
    assert!(
        checks.len() >= 2,
        "a check before the push and one after: {checks:?}"
    );
    assert!(
        checks[0] < calls_before_push && calls_before_push <= checks[1],
        "{checks:?} vs push at {calls_before_push}"
    );
    assert!(
        checks[1] < catalog_read,
        "the catalog is read only once the flag is confirmed: {checks:?}, catalog at {catalog_read}"
    );
    // And the guards went in.
    let installs: Vec<&str> = log
        .iter()
        .filter(|(_, s)| is_install(s))
        .map(|(d, _)| d.as_str())
        .collect();
    assert!(
        installs.contains(&"app") && installs.contains(&"analytics"),
        "{installs:?}"
    );
}

#[tokio::test]
async fn a_compute_that_already_carries_the_flag_isnt_reconfigured() {
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
    assert!(matches!(
        w.reconcile_member_roles(branch, None).await,
        MemberRolesReply::Done { .. }
    ));
    assert!(pushed_flags(&w).is_empty());
    assert!(sql_log(&w).iter().any(|(_, s)| is_install(s)));
}

#[tokio::test]
async fn a_failed_reconfigure_fails_the_protect_and_guards_nothing() {
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes
        .answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "off");
    *w.pusher.fail.lock().unwrap() = Some("compute_ctl is down".into());
    let reply = w.reconcile_member_roles(branch, None).await;
    assert!(
        matches!(reply, MemberRolesReply::Failed { .. }),
        "{reply:?}"
    );
    let log = sql_log(&w);
    assert!(
        !log.iter()
            .any(|(_, s)| s.contains("chelabase: member roles") || is_install(s) || is_open(s)),
        "nothing is listed, guarded or opened without the flag: {log:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_flag_that_never_takes_effect_fails_the_protect() {
    // The push succeeds, but a fresh session still reads `off` (a backend that hasn't absorbed
    // the reload): the guard loop must not run on a database list taken before the flag held.
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes
        .answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "off");
    let reply = w.reconcile_member_roles(branch, None).await;
    assert!(
        matches!(reply, MemberRolesReply::Failed { .. }),
        "{reply:?}"
    );
    assert_eq!(pushed_flags(&w).first(), Some(&serde_json::json!(true)));
    let log = sql_log(&w);
    assert!(
        !log.iter()
            .any(|(_, s)| s.contains("chelabase: member roles") || is_install(s) || is_open(s)),
        "{log:?}"
    );
}

#[tokio::test]
async fn unprotect_removes_the_guards_then_turns_the_flag_off() {
    let (w, journal, branch, ep) = fresh().await;
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes.answer_at(&ep, GUARD_PRESENT_MARK, "t");
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
    let at_push = note_sql_count_at_push(&w, &journal);

    let reply = w.reconcile_member_roles(branch, None).await;

    assert!(matches!(reply, MemberRolesReply::Done { .. }), "{reply:?}");
    assert_eq!(pushed_flags(&w), [serde_json::json!(false)]);
    let log = sql_log(&w);
    let calls_before_push = at_push.lock().unwrap().expect("the spec was pushed");
    let unguards = |range: &[(String, String)]| {
        range
            .iter()
            .filter(|(_, s)| s.contains(UNGUARD_MARK))
            .count()
    };
    assert!(unguards(&log) >= 3, "app, analytics and template1: {log:?}");
    assert_eq!(
        unguards(&log[..calls_before_push]),
        unguards(&log),
        "the flag went off before a guard was removed: push after {calls_before_push} calls"
    );
}

#[tokio::test]
async fn a_failed_unguard_keeps_the_flag_on() {
    let (w, _journal, branch, ep) = fresh().await;
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes.answer_at(&ep, GUARD_PRESENT_MARK, "t");
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
    w.computes.fail_in("app", UNGUARD_MARK);
    let reply = w.reconcile_member_roles(branch, None).await;
    assert!(
        matches!(reply, MemberRolesReply::Failed { .. }),
        "{reply:?}"
    );
    assert!(
        pushed_flags(&w).is_empty(),
        "protection holds while a guard remains"
    );
}

#[tokio::test]
async fn an_unprotected_compute_with_the_flag_off_isnt_reconfigured() {
    for answer in ["off", ""] {
        let (w, _journal, branch, ep) = fresh().await;
        w.computes
            .answer_at(&ep, "chelabase: member roles", &catalog());
        w.computes
            .answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, answer);
        assert!(matches!(
            w.reconcile_member_roles(branch, None).await,
            MemberRolesReply::Done { .. }
        ));
        assert!(pushed_flags(&w).is_empty(), "{answer:?}");
    }
}

#[tokio::test]
async fn a_failed_flag_off_fails_the_run_and_the_sweep_retries_it() {
    let (w, _journal, branch, ep) = fresh().await;
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
    *w.pusher.fail.lock().unwrap() = Some("compute_ctl is down".into());
    let reply = w.reconcile_member_roles(branch, None).await;
    assert!(
        matches!(reply, MemberRolesReply::Failed { .. }),
        "{reply:?}"
    );
    assert!(pushed_flags(&w).is_empty());

    *w.pusher.fail.lock().unwrap() = None;
    let (done, failed) = w.sweep_member_roles_once().await;
    assert_eq!((done, failed), (1, 0));
    assert_eq!(
        pushed_flags(&w),
        [serde_json::json!(false)],
        "the sweep turned the flag off"
    );
}

#[tokio::test]
async fn every_spec_the_worker_pushes_carries_the_branchs_protection() {
    for protected in [true, false] {
        let (w, _journal, branch, ep) = fresh().await;
        set_protected(&w, branch, protected);
        w.apply_pending(&ep).await.unwrap();
        assert_eq!(
            pushed_flags(&w),
            [serde_json::json!(protected)],
            "protected={protected}"
        );
    }
}
