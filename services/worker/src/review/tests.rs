//! The review in the worker (PB T4 fix round, R8/R10): every live database scanned as
//! `cloud_admin`, fixes re-checked against the worker's own scan.

use chelabase_nats::{ReviewFixOrder, ReviewReply, ReviewRequest};
use chelabase_schema::review::{fix_sql, parse_review, review_hash};
use chelabase_types::BranchId;
use chelabase_types::protection::{FixAction, REVIEW_SCAN_MARK};

use super::REVIEW_DATABASES_MARK;
use crate::OWNER_ROLE;
use crate::fakes::{FakeWorker, ready_database, worker};

const PLANTED: &str = r#"[{"kind":"function","schema":"public","name":"planted","identity":"","function":"public.planted()","owner":"dev","reasons":[{"kind":"security_definer","via":null}],"opaque_body":true}]"#;

/// A ready database's main branch, its compute answering two live databases: `app` holds
/// [`PLANTED`], `analytics` nothing.
async fn setup(w: &FakeWorker) -> BranchId {
    let id = ready_database(w).await;
    let main = w.metadata.main(id);
    w.computes.sql.lock().unwrap().clear();
    w.computes.answer(REVIEW_DATABASES_MARK, "app\nanalytics");
    w.computes.answer_in("app", REVIEW_SCAN_MARK, PLANTED);
    w.computes.answer_in("analytics", REVIEW_SCAN_MARK, "[]");
    main.id
}

fn scan(branch: BranchId) -> ReviewRequest {
    ReviewRequest { branch_id: branch.as_uuid(), fix: None }
}

fn fix(branch: BranchId, key: &str, action: FixAction, hash: &str) -> ReviewRequest {
    ReviewRequest {
        branch_id: branch.as_uuid(),
        fix: Some(ReviewFixOrder { key: key.into(), action, hash: hash.into() }),
    }
}

fn scripts(w: &FakeWorker) -> Vec<(String, String)> {
    w.computes.sql.lock().unwrap().iter().map(|(_, db, s)| (db.clone(), s.clone())).collect()
}

#[tokio::test]
async fn the_scan_lists_databases_live_and_scans_each() {
    let (w, _journal) = worker();
    let branch = setup(&w).await;
    let reply = w.review(scan(branch)).await;
    assert_eq!(reply, ReviewReply::Scanned { items: parse_review("app", PLANTED).unwrap() });
    let all = scripts(&w);
    let list: Vec<&(String, String)> = all.iter().filter(|(_, s)| s.starts_with(REVIEW_DATABASES_MARK)).collect();
    assert_eq!(list.len(), 1, "{all:?}");
    assert!(list[0].1.contains("datistemplate") && list[0].1.contains("datallowconn"), "{}", list[0].1);
    let mut scanned: Vec<&str> = all.iter().filter(|(_, s)| s.starts_with(REVIEW_SCAN_MARK)).map(|(db, _)| db.as_str()).collect();
    scanned.sort();
    assert_eq!(scanned, ["analytics", "app"]);
    // Read-only: nothing else ran.
    assert_eq!(all.len(), 3, "{all:?}");
}

#[tokio::test]
async fn a_database_that_cannot_be_scanned_fails_the_whole_review() {
    let (w, _journal) = worker();
    let branch = setup(&w).await;
    w.computes.fail_when(REVIEW_SCAN_MARK);
    assert!(matches!(w.review(scan(branch)).await, ReviewReply::Failed { .. }));
}

#[tokio::test]
async fn a_fix_checks_the_hash_then_runs_in_its_database() {
    let (w, _journal) = worker();
    let branch = setup(&w).await;
    let items = parse_review("app", PLANTED).unwrap();
    let hash = review_hash(&items);
    let reply = w.review(fix(branch, &items[0].key, FixAction::Reassign, &hash)).await;
    let expected = fix_sql(&items[0], FixAction::Reassign, OWNER_ROLE).unwrap();
    assert!(matches!(&reply, ReviewReply::Fixed { sql, .. } if *sql == expected), "{reply:?}");
    let ran: Vec<(String, String)> = scripts(&w).into_iter().filter(|(_, s)| s.contains(&expected)).collect();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert_eq!(ran[0].0, "app");
    assert!(ran[0].1.contains("SET LOCAL lock_timeout = '5s'"), "{}", ran[0].1);
}

#[tokio::test]
async fn a_stale_hash_or_an_unknown_key_changes_nothing() {
    let (w, _journal) = worker();
    let branch = setup(&w).await;
    let items = parse_review("app", PLANTED).unwrap();
    let reply = w.review(fix(branch, &items[0].key, FixAction::Drop, &"0".repeat(64))).await;
    assert_eq!(reply, ReviewReply::Changed { items: items.clone() });
    let reply = w.review(fix(branch, "app/function/public.nope()", FixAction::Drop, &review_hash(&items))).await;
    assert_eq!(reply, ReviewReply::NotFound { items: items.clone() });
    assert!(scripts(&w).iter().all(|(_, s)| !s.contains("DROP ROUTINE")), "{:?}", scripts(&w));
}

#[tokio::test]
async fn a_failed_fix_says_so_with_the_current_list() {
    let (w, _journal) = worker();
    let branch = setup(&w).await;
    let items = parse_review("app", PLANTED).unwrap();
    w.computes.fail_when("DROP ROUTINE");
    let reply = w.review(fix(branch, &items[0].key, FixAction::Drop, &review_hash(&items))).await;
    assert!(matches!(&reply, ReviewReply::FixFailed { items: now, .. } if *now == items), "{reply:?}");
}
