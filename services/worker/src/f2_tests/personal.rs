//! Personal logins (F2, part 2; C3's latent findings; members hub T6): the resync never makes
//! a lost personal login again (the member-role reconciler converges them from their
//! records), and `create_personal_login` takes only a whole verifier. The marker rule is
//! `personal_marker.rs`.

use chelabase_metadata::{RoleAttributes, RoleCopy, RoleLevel, RoleOrigin};
use chelabase_types::role_name::is_reserved_target;
use chrono::{DateTime, Utc};

use super::{PENCIL, PREFIX_ONLY};
use crate::OWNER_ROLE;
use crate::c2_tests::ready_named;
use crate::fakes::worker;
use crate::resync::{Observed, plan_resync};

/// Every kind of name a row could carry, and whether a personal login may never take it.
pub(crate) const NAMES: &[(&str, bool)] = &[
    ("alice", false),
    ("Al\"ice", false),
    ("a b", false),
    // Role names are case-sensitive: `Anon` is not `anon`.
    ("Anon", false),
    ("Owner", false),
    ("anon", true),
    ("authenticated", true),
    ("authenticator", true),
    ("owner", true),
    ("public", true),
    ("none", true),
    ("cloud_admin", true),
    ("neon_superuser", true),
    ("chelabase_admin", true),
    ("chelabase_auth_sync", true),
    ("chelabase_realtime", true),
    ("Chelabase_X", true),
    ("cb_m_0123", true),
    ("CB_M_x", true),
    ("pg_monitor", true),
    ("PG_x", true),
];

/// A personal login's row with `verifier`.
pub(crate) fn personal(name: &str, verifier: Option<&str>) -> RoleCopy {
    row(name, RoleOrigin::Personal, verifier)
}

pub(crate) fn row(name: &str, origin: RoleOrigin, verifier: Option<&str>) -> RoleCopy {
    RoleCopy {
        name: name.into(),
        origin,
        scram_verifier: verifier.map(str::to_string),
        password_enc: (origin == RoleOrigin::Api).then(|| "sealed".to_string()),
        login: true,
        level: RoleLevel::Plain,
        api_owner_level: false,
        pending: false,
        attributes_pending: false,
        created_at: DateTime::<Utc>::UNIX_EPOCH,
        attributes: RoleAttributes::default(),
    }
}

#[test]
fn the_name_table_agrees_with_is_reserved_target() {
    for (name, refused) in NAMES {
        assert_eq!(is_reserved_target(name, OWNER_ROLE), *refused, "{name}");
    }
}

// Invariant (the catalog lost the role; T6 changes it deliberately, spec Part 6
// "Convergence"): the resync never makes a lost personal login again, records or removes its
// row, whatever its name or verifier: the member-role reconciler makes it from its record.
// Exhaustive over the name kinds and the verifier kinds.
#[test]
fn a_lost_personal_login_is_never_made_again_by_the_resync() {
    for (name, _refused) in NAMES {
        for verifier in [
            Some(PENCIL),
            Some(PREFIX_ONLY),
            Some("md5000102030405060708090a0b0c0d0e0f"),
            None,
        ] {
            let (repairs, write) = plan_resync(
                OWNER_ROLE,
                &[personal(name, verifier)],
                &Observed::default(),
            );
            let case = format!("{name:?} verifier {verifier:?}");
            assert!(repairs.is_empty(), "{case}: {repairs:?}");
            assert!(
                write.upserts.iter().all(|u| u.name != *name),
                "{case}: {write:?}"
            );
            assert!(
                write.removals.is_empty(),
                "{case}: a personal row is never removed: {write:?}"
            );
        }
    }
}

// The owner's own name is never a personal login, whatever a row says.
#[test]
fn a_personal_row_named_as_the_owner_is_left_alone() {
    let (repairs, write) = plan_resync(
        OWNER_ROLE,
        &[personal(OWNER_ROLE, Some(PENCIL))],
        &Observed::default(),
    );
    assert!(repairs.is_empty() && write.upserts.is_empty() && write.removals.is_empty());
}

// `create_personal_login` takes only a whole verifier: anything else is refused before any SQL
// (`CREATE ROLE … PASSWORD '<not a verifier>'` would set it as a plaintext password).
#[tokio::test]
async fn create_personal_login_takes_only_a_whole_verifier() {
    let (w, journal) = worker();
    let ep = ready_named(&w, "app").await;
    journal.lock().unwrap().clear();
    w.computes.sql.lock().unwrap().clear();

    let (head, keys) = PENCIL.rsplit_once('$').unwrap();
    let (stored, _server) = keys.split_once(':').unwrap();
    for bad in [
        PREFIX_ONLY.to_string(),
        "SCRAM-SHA-256$x'; DROP ROLE y; --".to_string(),
        "SCRAM-SHA-256$".to_string(),
        format!("{head}${stored}"),
        format!("{PENCIL}\n"),
        PENCIL.replace("$4096:", "$0:"),
    ] {
        assert!(
            w.create_personal_login(&ep, "dave", &bad).await.is_err(),
            "{bad:?}"
        );
    }
    assert!(
        w.computes.sql.lock().unwrap().is_empty(),
        "a refusal runs no SQL: {:?}",
        w.computes.sql.lock().unwrap()
    );
    w.create_personal_login(&ep, "dave", PENCIL)
        .await
        .expect("a whole verifier");
    let sql = w.computes.sql.lock().unwrap().clone();
    assert_eq!(sql.len(), 1, "{sql:?}");
    assert!(
        sql[0].2.contains(&format!("PASSWORD '{PENCIL}'")),
        "{}",
        sql[0].2
    );
}
