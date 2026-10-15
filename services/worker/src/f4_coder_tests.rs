//! Edge cases the F4 coder added beyond the RED set (U14, RT3-11, R-F4a's worker side).

use chelabase_nats::MemberRolesReply;

use crate::f4_tests::{fresh, h, set_protected, sql_log};
use crate::member_roles::BRANCH_PROTECTED_CHECK_MARK;
use crate::resync::PendingResyncs;

fn catalog() -> String {
    format!(
        "r {} f t f t\nd {} {}\n",
        h("postgres"),
        h("app"),
        h("postgres")
    )
}

#[tokio::test]
async fn the_flag_reads_on_in_any_case() {
    // `SHOW` prints the value as the GUC spells it; a different case still means on.
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "ON");
    assert!(matches!(
        w.reconcile_member_roles(branch, None).await,
        MemberRolesReply::Done { .. }
    ));
    assert!(w.pusher.pushed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unreadable_flag_on_an_unprotected_branch_pushes_false_anyway() {
    // The compute can't answer the flag check: an idempotent push of the flag off settles it,
    // and the reconcile's other work carries on.
    let (w, _journal, branch, ep) = fresh().await;
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes.fail_when(BRANCH_PROTECTED_CHECK_MARK);
    assert!(matches!(
        w.reconcile_member_roles(branch, None).await,
        MemberRolesReply::Done { .. }
    ));
    let pushed = w.pusher.pushed.lock().unwrap();
    assert_eq!(pushed.len(), 1);
    assert_eq!(
        pushed[0].1["spec"]["branch_protected"],
        serde_json::json!(false)
    );
}

#[tokio::test]
async fn the_protect_reconfigure_does_not_touch_the_database_list_before_the_flag_holds() {
    // The flag never reads on; the catalog is never read (fail closed), whatever the sweep
    // does next.
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog());
    w.computes.fail_when(BRANCH_PROTECTED_CHECK_MARK);
    tokio::time::pause();
    let reply = w.reconcile_member_roles(branch, None).await;
    assert!(
        matches!(reply, MemberRolesReply::Failed { .. }),
        "{reply:?}"
    );
    assert!(
        !sql_log(&w)
            .iter()
            .any(|(_, s)| s.contains("chelabase: member roles")),
        "the catalog was read without the flag"
    );
}

#[tokio::test]
async fn a_branchs_protection_is_read_from_its_endpoint() {
    let (w, _journal, branch, ep) = fresh().await;
    assert!(!w.endpoint_branch_protected(&ep).await.unwrap());
    set_protected(&w, branch, true);
    assert!(w.endpoint_branch_protected(&ep).await.unwrap());
    // An endpoint with no branch (a static compute) isn't protected.
    assert!(!w.endpoint_branch_protected("ep-unlinked").await.unwrap());
    // A metadata failure is an error, never `false`.
    w.metadata.state().fail = Some("endpoint_branch");
    assert!(w.endpoint_branch_protected(&ep).await.is_err());
}

#[test]
fn a_ddl_mark_ends_with_the_run_it_belongs_to() {
    let pending = PendingResyncs::default();
    let branch = chelabase_types::BranchId::new();
    // A DDL request merges into a waiting plain one: the run owns the mark.
    assert!(pending.claim_marked(branch, false));
    assert!(!pending.claim_marked(branch, true));
    assert!(pending.release_marked(branch));
    // The next run starts clean: a plain request doesn't inherit it.
    assert!(pending.claim_marked(branch, false));
    assert!(!pending.release_marked(branch));
    // A mark can't be lost by a later plain request merging in.
    assert!(pending.claim_marked(branch, true));
    assert!(!pending.claim_marked(branch, false));
    assert!(pending.release_marked(branch));
}
