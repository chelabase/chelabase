//! C3 edge tests (fork round 4, personal logins) beyond the RED set.

use chelabase_metadata::{RoleAttributes, RoleCopy, RoleLevel, RoleOrigin};
use chelabase_types::role_name::mark_personal_login_sql;
use chrono::{DateTime, Utc};

use crate::OWNER_ROLE;
use crate::c2_tests::ready_named;
use crate::fakes::worker;
use crate::resync::{Observed, plan_resync};

/// F2: a whole verifier (RFC 7677's "pencil"): the recreate and `create_personal_login` take
/// no other.
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

// Invariant: whatever the catalog shows, a personal row is never removed and never recorded
// as another origin, and only a missing one is made again (with the marker).
#[test]
fn a_personal_row_is_never_removed_or_downgraded() {
    for name in ["alice", "Al\"ice", "a b"] {
        let copies = [personal(name, &[])];
        let (repairs, write) = plan_resync(OWNER_ROLE, &copies, &Observed::default());
        assert!(write.removals.is_empty(), "{name}: {write:?}");
        assert_eq!(repairs.len(), 1, "{name}");
        let script = repairs[0].statements.join("\n");
        assert!(script.starts_with("BEGIN;\nCREATE ROLE "), "{script}");
        assert!(script.ends_with("COMMIT;"), "{script}");
        assert!(script.contains(&mark_personal_login_sql(name)), "{script}");
        assert!(
            write
                .upserts
                .iter()
                .all(|u| u.origin == RoleOrigin::Personal),
            "{write:?}"
        );
    }
}

// Names the platform keeps for itself are never made by a resync, whatever a row says.
#[test]
fn a_personal_row_with_a_reserved_prefix_is_not_recreated() {
    for name in ["cb_m_0123", "chelabase_realtime", "CHELABASE_x"] {
        let (repairs, write) =
            plan_resync(OWNER_ROLE, &[personal(name, &[])], &Observed::default());
        assert!(
            repairs.is_empty() && write.upserts.is_empty() && write.removals.is_empty(),
            "{name}"
        );
    }
}

// Memberships in groups that no longer exist are dropped from the script, not failed on.
#[test]
fn the_recreate_script_keeps_only_groups_that_exist() {
    let (repairs, write) = plan_resync(
        OWNER_ROLE,
        &[personal("alice", &["gone"])],
        &Observed::default(),
    );
    let script = repairs[0].statements.join("\n");
    assert!(!script.contains("gone"), "{script}");
    assert!(write.upserts[0].attributes.member_of.is_empty());
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
