//! F2 round 2 (audit hardening): a resync never removes a personal row, whatever its write
//! asks, and a refresh pass skips an endpoint whose lock-pool wait timed out.

use chelabase_metadata::{ResyncWrite, RoleOrigin};

use crate::c2_tests::ready_named;
use crate::fakes::{FakeLogin, worker};
use crate::ports::WorkerMetadata;
use crate::token_refresh::RefreshPass;

fn login(origin: RoleOrigin) -> FakeLogin {
    FakeLogin {
        origin,
        password_enc: None,
        pending: false,
        login: true,
        level: chelabase_metadata::RoleLevel::Plain,
        api_owner_level: false,
        attributes: Default::default(),
        attributes_pending: false,
        attributes_change_id: None,
    }
}

// The store's removal guard, like `apply_resync`'s `origin <> 'personal'`: a personal row is
// kept even when a write lists it for removal; an `sql` row is removed.
#[tokio::test]
async fn a_resync_write_never_removes_a_personal_row() {
    let (w, _journal) = worker();
    let id = crate::fakes::ready_database(&w).await;
    let main = w.metadata.main(id);
    let ep = main.endpoint_id.clone().unwrap();
    {
        let mut state = w.metadata.state();
        let endpoint = state.endpoints.get_mut(&ep).unwrap();
        endpoint.logins.insert("alice".into(), login(RoleOrigin::Personal));
        endpoint.logins.insert("bob".into(), login(RoleOrigin::Sql));
    }
    let (_, version) = w.metadata.role_copies(&ep).await.unwrap();
    let databases = w.metadata.state().endpoints[&ep].branch_databases.clone();
    let write = ResyncWrite {
        upserts: vec![],
        removals: vec!["alice".into(), "bob".into()],
        databases,
    };
    w.metadata
        .apply_resync(&ep, main.id, version, &write)
        .await
        .unwrap();
    let state = w.metadata.state();
    let logins = &state.endpoints[&ep].logins;
    assert!(logins.contains_key("alice"), "a personal row is never removed");
    assert!(!logins.contains_key("bob"));
}

// A lock-pool wait that timed out (every metadata connection busy) is a busy endpoint too:
// skipped, not failed, and its count is left alone.
#[tokio::test]
async fn a_lock_pool_timeout_is_skipped_not_failed() {
    let (w, _journal) = worker();
    let ep = ready_named(&w, "app").await;
    w.pusher.set_capability(&ep, true);
    w.metadata.state().lock_pool_timeout = true;
    let pass = w.refresh_storage_tokens_once(1_800_000_000).await;
    assert_eq!(
        pass,
        RefreshPass {
            refreshed: 0,
            failed: 0,
            skipped: 1
        }
    );
    assert_eq!(w.storage_token_failure_count(&ep), 0);
    w.metadata.state().lock_pool_timeout = false;
    let pass = w.refresh_storage_tokens_once(1_800_000_000).await;
    assert_eq!(pass.refreshed, 1);
}
