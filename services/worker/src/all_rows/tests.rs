//! final B2 (EOS-H1, user ruling 2026-10-23: fail closed, via the worker): the all-rows check
//! runs as `cloud_admin` through `admin_sql` on the branch's compute: the live databases
//! listed, the cluster's memberships read, each database's "all rows" roles read; an exposure
//! answers the chain, and anything that can't be read answers `Failed` (never `Clear`).

use std::sync::Arc;

use chelabase_metadata::BranchStatus;
use chelabase_nats::{AllRowsCheckReply, AllRowsCheckRequest, MembershipChange};
use chelabase_types::BranchId;

use super::{
    ALL_ROWS_CHECK_DEADLINE, ALL_ROWS_MEMBERSHIPS_MARK, ALL_ROWS_ROLES_MARK, all_rows_check_reply,
};
use crate::fakes::{FakeWorker, ready_database, worker};
use crate::review::REVIEW_DATABASES_MARK;

/// One line of the memberships read: `member` is a member of `role`.
fn membership(member: &str, role: &str) -> String {
    format!("{}|{}\n", hex::encode(member), hex::encode(role))
}

/// One line of an "all rows" roles read.
fn all_rows_role(role: &str) -> String {
    format!("{}\n", hex::encode(role))
}

/// A ready main branch whose compute lists two live databases, `app` and `analytics`, with
/// these memberships (member → role) and these "all rows" roles in each; the SQL log emptied.
async fn setup(
    w: &FakeWorker,
    memberships: &[(&str, &str)],
    in_app: &[&str],
    in_analytics: &[&str],
) -> BranchId {
    let id = ready_database(w).await;
    let main = w.metadata.main(id);
    w.computes.sql.lock().unwrap().clear();
    w.computes.answer(REVIEW_DATABASES_MARK, "app\nanalytics\n");
    let lines: String = memberships.iter().map(|(m, r)| membership(m, r)).collect();
    w.computes.answer(ALL_ROWS_MEMBERSHIPS_MARK, &lines);
    let app: String = in_app.iter().map(|r| all_rows_role(r)).collect();
    w.computes.answer_in("app", ALL_ROWS_ROLES_MARK, &app);
    let analytics: String = in_analytics.iter().map(|r| all_rows_role(r)).collect();
    w.computes
        .answer_in("analytics", ALL_ROWS_ROLES_MARK, &analytics);
    main.id
}

fn key(branch: BranchId, role: &str) -> AllRowsCheckRequest {
    AllRowsCheckRequest {
        branch_id: branch,
        starting: vec![role.into()],
        change: None,
    }
}

/// A membership change of `role` to `member_of`, from the data API roles and a key on `k`.
fn change(branch: BranchId, role: &str, member_of: &[&str]) -> AllRowsCheckRequest {
    AllRowsCheckRequest {
        branch_id: branch,
        starting: ["anon", "authenticated", "authenticator", "k"]
            .map(String::from)
            .to_vec(),
        change: Some(MembershipChange {
            role: role.into(),
            member_of: member_of.iter().map(|r| r.to_string()).collect(),
        }),
    }
}

fn exposed(chain: &[&str]) -> AllRowsCheckReply {
    AllRowsCheckReply::Exposed {
        chain: chain.iter().map(|r| r.to_string()).collect(),
    }
}

fn scripts(w: &FakeWorker) -> Vec<(String, String)> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .map(|(_, db, s)| (db.clone(), s.clone()))
        .collect()
}

fn failed(reply: &AllRowsCheckReply) -> bool {
    matches!(reply, AllRowsCheckReply::Failed { .. })
}

// A key's role that is, or reaches, an "all rows" role is exposed, with the shortest chain;
// a role that reaches none is clear. Read-only: the databases listed, the memberships read,
// each database's "all rows" roles read, nothing else.
#[tokio::test]
async fn a_key_role_reaching_an_all_rows_role_is_exposed_with_the_chain() {
    let (w, _journal) = worker();
    let branch = setup(&w, &[("k", "g"), ("g", "r"), ("s", "x")], &["r"], &[]).await;
    assert_eq!(
        w.all_rows_check(key(branch, "k")).await,
        exposed(&["k", "g", "r"])
    );
    let all = scripts(&w);
    let listed = all
        .iter()
        .filter(|(_, s)| s.starts_with(REVIEW_DATABASES_MARK))
        .count();
    assert_eq!(listed, 1, "{all:?}");
    assert!(
        all.iter()
            .any(|(_, s)| s.starts_with(ALL_ROWS_MEMBERSHIPS_MARK)),
        "{all:?}"
    );
    let mut read: Vec<&str> = all
        .iter()
        .filter(|(_, s)| s.starts_with(ALL_ROWS_ROLES_MARK))
        .map(|(db, _)| db.as_str())
        .collect();
    read.sort();
    assert_eq!(
        read,
        ["analytics", "app"],
        "every live database read: {all:?}"
    );
    let roles_sql = &all
        .iter()
        .find(|(_, s)| s.starts_with(ALL_ROWS_ROLES_MARK))
        .unwrap()
        .1;
    assert!(
        roles_sql.contains("chelabase:all_rows") && roles_sql.contains("pg_policy"),
        "{roles_sql}"
    );
    let members_sql = &all
        .iter()
        .find(|(_, s)| s.starts_with(ALL_ROWS_MEMBERSHIPS_MARK))
        .unwrap()
        .1;
    assert!(members_sql.contains("pg_auth_members"), "{members_sql}");
    assert_eq!(all.len(), 4, "read-only, nothing else ran: {all:?}");

    assert_eq!(w.all_rows_check(key(branch, "r")).await, exposed(&["r"]));
    assert_eq!(
        w.all_rows_check(key(branch, "s")).await,
        AllRowsCheckReply::Clear
    );
}

// An "all rows" role in any live database counts, not only the branch's main database.
#[tokio::test]
async fn an_all_rows_role_in_any_live_database_counts() {
    let (w, _journal) = worker();
    let branch = setup(&w, &[("k", "r")], &[], &["r"]).await;
    assert_eq!(
        w.all_rows_check(key(branch, "k")).await,
        exposed(&["k", "r"])
    );
}

// A membership change counts only what it newly connects: a role joining the chain exposes;
// an exposure already there, a removal, or a role no data API role reaches doesn't.
#[tokio::test]
async fn a_change_counts_only_what_it_newly_connects() {
    let (w, _journal) = worker();
    let branch = setup(&w, &[("k", "m")], &["r"], &[]).await;
    assert_eq!(
        w.all_rows_check(change(branch, "m", &["r"])).await,
        exposed(&["k", "m", "r"])
    );
    assert_eq!(
        w.all_rows_check(change(branch, "k", &["r"])).await,
        exposed(&["k", "r"])
    );
    assert_eq!(
        w.all_rows_check(change(branch, "x", &["r"])).await,
        AllRowsCheckReply::Clear
    );

    let (w, _journal) = worker();
    let branch = setup(&w, &[("k", "m"), ("m", "r")], &["r"], &[]).await;
    assert_eq!(
        w.all_rows_check(change(branch, "m", &["r"])).await,
        AllRowsCheckReply::Clear,
        "already exposed before the change"
    );
    assert_eq!(
        w.all_rows_check(change(branch, "m", &[])).await,
        AllRowsCheckReply::Clear,
        "a removal exposes nothing"
    );
}

// Names come back exactly: quotes, `|`, newlines and non-ASCII survive the read.
#[tokio::test]
async fn odd_role_names_come_back_exactly() {
    let (w, _journal) = worker();
    let odd = "we|ird \"Name\"\nline";
    let all = "Ünïcode all";
    let branch = setup(&w, &[(odd, all)], &[all], &[]).await;
    assert_eq!(
        w.all_rows_check(key(branch, odd)).await,
        exposed(&[odd, all])
    );
}

// No endpoint to read on, or a branch that isn't ready or doesn't exist: `Failed` before any
// SQL, never `Clear`.
#[tokio::test]
async fn a_branch_without_a_running_compute_fails_closed_before_any_sql() {
    let (w, _journal) = worker();
    let branch = setup(&w, &[("k", "r")], &["r"], &[]).await;
    let reply = w.all_rows_check(key(BranchId::new(), "k")).await;
    assert!(failed(&reply), "unknown branch: {reply:?}");
    w.metadata.state().branches.get_mut(&branch).unwrap().status = BranchStatus::Resetting;
    let reply = w.all_rows_check(key(branch, "k")).await;
    assert!(failed(&reply), "not ready: {reply:?}");
    w.metadata.state().branches.get_mut(&branch).unwrap().status = BranchStatus::Ready;
    w.metadata
        .state()
        .branches
        .get_mut(&branch)
        .unwrap()
        .endpoint_id = None;
    let reply = w.all_rows_check(key(branch, "s")).await;
    assert!(failed(&reply), "no endpoint: {reply:?}");
    assert!(scripts(&w).is_empty(), "{:?}", scripts(&w));
}

// Any read that fails, times out or answers a line it can't parse fails the whole check:
// never a partial `Clear` (here the readable parts expose nothing).
#[tokio::test]
async fn any_read_that_fails_fails_the_whole_check() {
    type Break = fn(&FakeWorker);
    let breaks: [(&str, Break); 6] = [
        ("listing the databases fails", |w| {
            w.computes.fail_when(REVIEW_DATABASES_MARK)
        }),
        ("the memberships read fails", |w| {
            w.computes.fail_when(ALL_ROWS_MEMBERSHIPS_MARK)
        }),
        ("one database's read fails", |w| {
            w.computes.fail_in("analytics", ALL_ROWS_ROLES_MARK)
        }),
        ("a read times out", |w| {
            w.computes.time_out_when(ALL_ROWS_ROLES_MARK, false)
        }),
        ("a membership line isn't two hex names", |w| {
            w.computes.answers.lock().unwrap().clear();
            w.computes.answer(REVIEW_DATABASES_MARK, "app\nanalytics\n");
            w.computes.answer(ALL_ROWS_MEMBERSHIPS_MARK, "6b|6d|72\n");
        }),
        ("a role line isn't hex", |w| {
            w.computes.answers_in.lock().unwrap().clear();
            w.computes
                .answer_in("app", ALL_ROWS_ROLES_MARK, "not hex\n");
        }),
    ];
    for (what, broken) in breaks {
        let (w, _journal) = worker();
        let branch = setup(&w, &[("k", "m")], &[], &["r"]).await;
        broken(&w);
        let reply = w.all_rows_check(key(branch, "k")).await;
        assert!(failed(&reply), "{what}: {reply:?}");
    }
}

// The RPC: a request it can't parse is `Failed`; a good one answers the check's reply.
#[tokio::test]
async fn the_rpc_answers_the_check_and_fails_closed_on_a_bad_request() {
    let (w, _journal) = worker();
    let branch = setup(&w, &[("k", "r")], &["r"], &[]).await;
    let w = Arc::new(w);
    let reply = all_rows_check_reply(w.clone(), b"{\"branch_id\":", ALL_ROWS_CHECK_DEADLINE).await;
    assert!(failed(&reply), "{reply:?}");
    let payload = serde_json::to_vec(&key(branch, "k")).unwrap();
    assert_eq!(
        all_rows_check_reply(w.clone(), &payload, ALL_ROWS_CHECK_DEADLINE).await,
        exposed(&["k", "r"])
    );
}
