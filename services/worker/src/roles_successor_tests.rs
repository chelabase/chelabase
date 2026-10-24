//! T10 (coder's edge cases): a role delete's successor, beyond the RED set.

use super::*;
use crate::fakes::{FakeWorker, Journal, ready_database, worker};

const Q: &str = QUARANTINE_ROLE;

/// `app` created (plain) on a ready main branch; the journal and SQL log emptied.
async fn with_app(w: &FakeWorker, journal: &Journal) -> (BranchId, String, String) {
    let id = ready_database(w).await;
    let main = w.metadata.main(id);
    let ep = main.endpoint_id.unwrap();
    let database = w.metadata.state().endpoints[&ep].databases[0].name.clone();
    assert_eq!(
        w.create_role(main.id, &RoleSpec::login("app", false)).await,
        RoleOutcome::Ok
    );
    journal.lock().unwrap().clear();
    w.computes.sql.lock().unwrap().clear();
    (main.id, ep, database)
}

fn sql(w: &FakeWorker) -> Vec<String> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, s)| s.clone())
        .collect()
}

fn has_row(w: &FakeWorker, ep: &str, name: &str) -> bool {
    w.metadata.state().endpoints[ep].logins.contains_key(name)
}

fn at(s: &str, needle: &str) -> usize {
    s.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} missing from {s}"))
}

// User ruling (2026-10-23): a database the deleted role owns goes to the branch owner, before
// the reassignment (which would otherwise hand it to the successor); everything inside it goes
// to the successor.
#[test]
fn a_database_the_role_owns_goes_to_the_owner_first() {
    for successor in ["heir", Q] {
        let s = role_delete_script("app", successor);
        let alter = at(&s, "ALTER DATABASE %I OWNER TO %I");
        assert!(s.contains("db.datdba = (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = ''app'')"), "{s}");
        assert!(s.contains(&format!("d.datname, ''{OWNER_ROLE}''")), "{s}");
        assert!(alter > at(&s, "BEGIN;"), "inside the transaction: {s}");
        assert!(alter < at(&s, "REASSIGN OWNED BY"), "{s}");
        assert!(s.contains(&format!("REASSIGN OWNED BY \"app\" TO \"{successor}\";")), "{s}");
    }
}

// A named successor's rule is checked again inside the transaction (a membership granted
// between the check and the move fails the delete); the quarantine role needs none.
#[test]
fn a_named_successor_is_checked_again_inside_the_transaction() {
    let s = role_delete_script("app", "heir");
    let guard = at(&s, "can no longer receive objects");
    assert!(at(&s, "BEGIN;") < guard && guard < at(&s, "REASSIGN OWNED BY"), "{s}");
    assert!(s.contains(&successor_predicate("heir").replace('\'', "''")), "{s}");
    assert!(!role_delete_script("app", Q).contains("can no longer receive objects"));
}

// The count is this database's ownership rows of the role (shared objects, its databases
// included, aren't counted once per database).
#[test]
fn the_count_is_this_databases_ownership_rows() {
    let s = role_delete_script("app", Q);
    for needle in ["pg_catalog.pg_shdepend", "d.deptype = 'o'", "pg_catalog.current_database()"] {
        assert!(s.contains(needle), "{needle}: {s}");
    }
}

// The script never carries the needles other delete steps are told apart by.
#[test]
fn the_script_is_none_of_the_other_steps() {
    let s = role_delete_script("app", "heir");
    assert!(!s.contains("FROM pg_database"), "{s}");
    assert!(!s.contains(&exists_sql("app")), "{s}");
    assert!(!successor_check_sql("heir").contains("REASSIGN OWNED BY"));
}

#[test]
fn the_successor_rules_before_anything_is_read() {
    assert!(check_successor("app", Q).is_ok());
    assert_eq!(chelabase_nats::QUARANTINE_SUCCESSOR, Q);
    for ok in ["heir", "My Heir", "a\"b'c", "Anon2"] {
        assert!(check_successor("app", ok).is_ok(), "{ok}");
    }
    for bad in ["app", "Chelabase_Orphaned", "OWNER", "Postgres", "CB_M_X", "none"] {
        assert!(
            matches!(check_successor("app", bad), Err(RoleOutcome::Refused("invalid_successor", _))),
            "{bad}"
        );
    }
}

#[test]
fn delete_output_sums_and_rejects_shapes() {
    assert_eq!(
        parse_reassigned("count|2\n\ncount|3\n").unwrap(),
        Reassigned { count: 5, definers: vec![], confirmed: vec![] }
    );
    for bad in ["count|", "count|1|2\n", "Count|1\n", "1\n", "definer|0\n"] {
        assert!(parse_reassigned(bad).is_err(), "{bad:?}");
    }
}

// The successor check failing (the compute unreachable) touches nothing; the row stays.
#[tokio::test]
async fn a_failed_successor_check_touches_nothing() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    *w.computes.fail_sql_containing.lock().unwrap() =
        Some(("chelabase: successor check".into(), "connection refused".into()));

    let outcome = w.delete_role(branch, "app", "heir").await;

    assert!(matches!(outcome, RoleOutcome::Failed(_)), "{outcome:?}");
    assert!(!sql(&w).iter().any(|s| s.contains("NOLOGIN") || s.contains("REASSIGN")));
    assert!(has_row(&w, &ep, "app"));
}

// A role already gone from SQL with a valid named successor: checked, then done with nothing
// moved, its row deleted.
#[tokio::test]
async fn a_role_gone_from_sql_with_a_named_successor_moves_nothing() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    w.computes.answer(&successor_check_sql("heir"), "t\n");

    assert_eq!(
        w.delete_role(branch, "app", "heir").await,
        RoleOutcome::Deleted(Reassigned::default())
    );
    assert_eq!(sql(&w), [successor_check_sql("heir"), exists_sql("app")]);
    assert!(!has_row(&w, &ep, "app"));
}

// Output the parser doesn't know fails the delete before the
// role is dropped; the row stays, and a retry is safe.
#[tokio::test]
async fn unknown_outputs_fail_the_delete_and_keep_the_row() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    w.computes.answer(&exists_sql("app"), "1\n");
    w.computes.answer("encode(convert_to(datname", &format!("{}\n", hex::encode("app")));
    w.computes.answer("REASSIGN OWNED BY", "WARNING: something\n");

    let outcome = w.delete_role(branch, "app", Q).await;

    assert!(matches!(outcome, RoleOutcome::Failed(ref m) if m.contains("reading what moved")), "{outcome:?}");
    assert!(has_row(&w, &ep, "app"));
    assert!(!sql(&w).iter().any(|s| s.starts_with("DROP ROLE")));
}

// --- Final round, area B ----------------------------------------------------------------------

/// The statement T10-1 adds: memberships are frozen for the rest of the delete's transaction.
fn lock_at(s: &str) -> usize {
    ["LOCK TABLE pg_catalog.pg_auth_members IN SHARE MODE", "LOCK pg_catalog.pg_auth_members IN SHARE MODE"]
        .iter()
        .find_map(|l| s.find(l))
        .unwrap_or_else(|| panic!("no SHARE lock on pg_catalog.pg_auth_members in {s}"))
}

// T10-1: `LOCK pg_catalog.pg_auth_members IN SHARE MODE` inside the transaction, before the
// successor's in-transaction check, so a membership granted between that check and the
// reassignment (`GRANT owner_level TO heir`) waits for the delete instead of slipping through.
#[test]
fn the_delete_script_freezes_memberships_before_its_checks() {
    for successor in ["heir", Q] {
        let s = role_delete_script("app", successor);
        let lock = lock_at(&s);
        assert!(at(&s, "BEGIN;") < lock, "{successor}: inside the transaction: {s}");
        assert!(lock < at(&s, "REASSIGN OWNED BY"), "{successor}: {s}");
    }
    let s = role_delete_script("app", "heir");
    assert!(lock_at(&s) < at(&s, "can no longer receive objects"), "before the guard: {s}");
}

// T10-3: the delete script bounds its waits: a `lock_timeout` and a `statement_timeout`
// (neither 0, which is no bound) set before anything takes a lock.
#[test]
fn the_delete_script_bounds_its_lock_and_statement_waits() {
    for successor in ["heir", Q] {
        let s = role_delete_script("app", successor);
        for setting in ["lock_timeout", "statement_timeout"] {
            let set = [format!("SET {setting} = "), format!("SET LOCAL {setting} = ")]
                .iter()
                .find_map(|p| s.find(p.as_str()))
                .unwrap_or_else(|| panic!("{successor}: no {setting} in {s}"));
            assert!(set < lock_at(&s) && set < at(&s, "REASSIGN OWNED BY"), "{successor}: {setting}: {s}");
            for unbounded in [format!("{setting} = 0;"), format!("{setting} = '0'")] {
                assert!(!s.contains(&unbounded), "{successor}: {unbounded} in {s}");
            }
        }
    }
}

// T10-5: the successor check reads its literals with `standard_conforming_strings = on`, as
// the delete script does (a name with a backslash means what it says).
#[test]
fn the_successor_check_sets_standard_conforming_strings() {
    for successor in ["heir", "a\\b'c"] {
        let s = successor_check_sql(successor);
        let set = at(&s, "SET standard_conforming_strings = on");
        assert!(set < at(&s, "SELECT"), "{successor}: {s}");
    }
}

/// psql's answer when a database's in-transaction successor check raises (the role got a
/// membership that makes it unfit since the check before the delete).
const GUARD_RAISED: &str = "psql:<stdin>:9: ERROR:  the successor heir can no longer receive objects\n\
                            CONTEXT:  PL/pgSQL function inline_code_block line 3 at RAISE";

/// `app` in SQL, owning objects in `d1` and `d2` (in that order), `heir` valid before the delete.
fn two_databases(w: &FakeWorker) {
    w.computes.answer(&exists_sql("app"), "1\n");
    w.computes.answer(
        "encode(convert_to(datname",
        &format!("{}\n{}\n", hex::encode("d1"), hex::encode("d2")),
    );
    w.computes.answer(&successor_check_sql("heir"), "t\n");
}

// T10-2: the in-transaction successor check raising in the first database is 422
// `invalid_successor` (not a 500): that transaction rolled back, nothing moved anywhere, the
// role and its row stay (without LOGIN), and the answer says nothing already moved.
#[tokio::test]
async fn a_guard_raise_in_the_first_database_is_invalid_successor_and_moves_nothing() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    two_databases(&w);
    w.computes.fail_in_with("d1", "REASSIGN OWNED BY", GUARD_RAISED);

    let outcome = w.delete_role(branch, "app", "heir").await;

    match &outcome {
        RoleOutcome::Refused("invalid_successor", message) => {
            assert!(!message.contains("already moved"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    let ran = w.computes.sql.lock().unwrap().clone();
    assert!(
        !ran.iter().any(|(_, db, s)| db == "d2" && s.contains("REASSIGN OWNED BY")),
        "the second database was never touched: {ran:?}"
    );
    assert!(!sql(&w).iter().any(|s| s.starts_with("DROP ROLE")), "{:?}", sql(&w));
    assert!(has_row(&w, &ep, "app"));
}

// T10-2: a raise in a later database, after an earlier one moved: still 422
// `invalid_successor`, never a 500, and the answer states precisely what is left: the
// databases whose objects already went to the successor (`objects already moved in: d1`),
// the role kept without LOGIN and its row kept, so the Admin can finish with a valid
// successor.
#[tokio::test]
async fn a_guard_raise_after_a_database_moved_says_what_moved() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    two_databases(&w);
    w.computes.answer_in("d1", "REASSIGN OWNED BY", "count|2\n");
    w.computes.fail_in_with("d2", "REASSIGN OWNED BY", GUARD_RAISED);

    let outcome = w.delete_role(branch, "app", "heir").await;

    match &outcome {
        RoleOutcome::Refused("invalid_successor", message) => {
            assert!(message.contains("objects already moved in: d1"), "{message}");
            assert!(!message.contains("d2"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!sql(&w).iter().any(|s| s.starts_with("DROP ROLE")), "{:?}", sql(&w));
    assert!(has_row(&w, &ep, "app"));
}
