//! W30/S23: `chelabase_realtime` is never a secret key's role, a login role the API manages, a
//! member grant or an act-as target. The prefix rules (`chelabase_`) already cover the login
//! role API, the member grants and act-as (these tests guard them); the data API's explicit
//! list does not. The resync's role list (`resync.rs`, `!~* '^(cb_m_|chelabase_)'`, pinned by
//! its own test) keeps the role out of the metadata, and so out of the proxy's role secrets.
//! The orchestrator's `sql_editor` list is T6's.

use std::collections::BTreeSet;

use chelabase_nats::{CheckDataRoleReply, CheckDataRoleRequest};
use chelabase_types::BranchId;
use chelabase_types::access::{Grant, PlatformRole, RoleSet, act_as_reserved};

use crate::data_api::RoleCheck;
use crate::fakes::{entries, ready_database, worker};
use crate::member_roles::{MemberCatalog, grantable, granted_roles};
use crate::realtime::REALTIME_ROLE;
use crate::roles::{RoleOutcome, RoleSpec};

#[tokio::test]
async fn a_secret_key_is_never_bound_to_the_realtime_role() {
    let (w, journal) = worker();
    let db = ready_database(&w).await;
    journal.lock().unwrap().clear();

    let check = w.check_data_role(BranchId::from(db), REALTIME_ROLE).await;

    assert_eq!(
        check,
        RoleCheck::Refused("the role chelabase_realtime is reserved".into())
    );
    assert!(
        entries(&journal)
            .iter()
            .all(|e| !e.starts_with("compute.sql")),
        "refused before any SQL: {:?}",
        entries(&journal)
    );
    // And through the RPC the orchestrator calls.
    let reply = w
        .check_data_role_reply(&CheckDataRoleRequest {
            branch_id: BranchId::from(db),
            role: REALTIME_ROLE.into(),
        })
        .await;
    assert_eq!(
        reply,
        CheckDataRoleReply::refused("the role chelabase_realtime is reserved")
    );
}

#[tokio::test]
async fn the_login_role_api_refuses_the_realtime_role() {
    let (w, _journal) = worker();
    let db = ready_database(&w).await;
    let branch = BranchId::from(db);
    for outcome in [
        w.create_role(branch, &RoleSpec::login(REALTIME_ROLE, false))
            .await,
        w.delete_role(branch, REALTIME_ROLE).await,
    ] {
        assert!(
            matches!(outcome, RoleOutcome::Refused("reserved_role_name", _)),
            "{outcome:?}"
        );
    }
}

#[test]
fn member_roles_never_grant_the_realtime_role() {
    assert!(!grantable(REALTIME_ROLE));
    let catalog = MemberCatalog {
        roles: [REALTIME_ROLE, "billing"]
            .into_iter()
            .map(String::from)
            .collect(),
        ..Default::default()
    };
    let all = Grant {
        platform_role: PlatformRole::Admin,
        roles: RoleSet::All,
    };
    assert_eq!(
        granted_roles(&all, &catalog).0,
        BTreeSet::from(["billing".to_string()])
    );
}

#[test]
fn act_as_never_offers_the_realtime_role() {
    assert!(act_as_reserved(REALTIME_ROLE));
}
