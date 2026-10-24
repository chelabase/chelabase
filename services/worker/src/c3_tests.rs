//! C3 edge tests (fork round 4, personal logins) beyond the RED set.

use chelabase_metadata::{RoleAttributes, RoleCopy, RoleLevel, RoleOrigin};
use chrono::{DateTime, Utc};

use crate::OWNER_ROLE;
use crate::c2_tests::ready_named;
use crate::fakes::worker;
use crate::resync::{Observed, ObservedRole, plan_resync};

/// F2: a whole verifier (RFC 7677's "pencil"): `create_personal_login` takes no other.
const VERIFIER: &str = crate::f2_tests::PENCIL;

fn personal(name: &str, member_of: &[&str]) -> RoleCopy {
    RoleCopy {
        name: name.into(),
        origin: RoleOrigin::Personal,
        scram_verifier: Some(VERIFIER.into()),
        password_enc: None,
        login: true,
        level: RoleLevel::Plain,
        api_owner_level: false,
        pending: false,
        attributes_pending: false,
        created_at: DateTime::<Utc>::UNIX_EPOCH,
        attributes: RoleAttributes {
            member_of: member_of.iter().map(|g| g.to_string()).collect(),
            ..RoleAttributes::default()
        },
    }
}

fn sql_of(w: &crate::fakes::FakeWorker) -> Vec<String> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, s)| s.clone())
        .collect()
}

// T6 (spec Part 6 "Convergence", S1; replaces the RT4-4 invariant deliberately): whatever the
// catalog shows (the role lost, present and marked, present unmarked), a personal row is
// never made again, repaired, removed or recorded by the resync, for any name; the member-role
// reconciler alone converges personal logins.
#[test]
fn a_personal_row_is_never_recreated_removed_or_recorded() {
    for name in ["alice", "Al\"ice", "a b", "cb_m_0123", "chelabase_realtime", "CHELABASE_x"] {
        let mut marked = ObservedRole::plain(name);
        marked.password = Some(VERIFIER.into());
        marked.owner_admin = false;
        marked.marked = true;
        let mut unmarked = marked.clone();
        unmarked.marked = false;
        for (what, roles) in [("lost", vec![]), ("marked", vec![marked]), ("unmarked", vec![unmarked])] {
            let observed = Observed { roles, databases: Vec::new() };
            let (repairs, write) = plan_resync(OWNER_ROLE, &[personal(name, &["readers"])], &observed);
            assert!(repairs.iter().all(|g| g.role != name), "{name} {what}: {repairs:?}");
            assert!(write.upserts.iter().all(|u| u.name != name), "{name} {what}: {write:?}");
            assert!(write.removals.iter().all(|r| r != name), "{name} {what}: {write:?}");
        }
    }
}

#[tokio::test]
async fn the_script_is_one_transaction_and_the_verifier_is_a_literal() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    journal.lock().unwrap().clear();
    w.computes.sql.lock().unwrap().clear();

    // F2: a "verifier" carrying a quote isn't a whole verifier, so it never reaches the script.
    let odd = "SCRAM-SHA-256$x'; DROP ROLE y; --";
    assert!(w.create_personal_login(&ep, "eve", odd).await.is_err());
    assert!(sql_of(&w).is_empty(), "{:?}", sql_of(&w));
    w.create_personal_login(&ep, "eve", VERIFIER).await.unwrap();

    let script = sql_of(&w).remove(0);
    assert!(script.starts_with("BEGIN;"), "{script}");
    assert!(script.trim_end().ends_with("COMMIT;"), "{script}");
    assert!(script.contains(&format!("PASSWORD '{VERIFIER}'")), "{script}");
}

#[tokio::test]
async fn a_name_postgres_cannot_hold_is_refused_before_any_sql() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    journal.lock().unwrap().clear();
    w.computes.sql.lock().unwrap().clear();

    for name in [
        "",
        &"x".repeat(64),
        "a\0b",
        "PG_x",
        "Chelabase_x",
        "CB_M_1",
        "public",
        "none",
    ] {
        assert!(
            w.create_personal_login(&ep, name, VERIFIER).await.is_err(),
            "{name:?}"
        );
    }
    assert!(sql_of(&w).is_empty());
}
