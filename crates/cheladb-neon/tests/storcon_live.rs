//! Opt-in round trip through `StorconClient` against a running storage controller
//! (`just deps-up` and `just dev` first; the storage controller calls the orchestrator's internal listener on attach):
//!
//! ```sh
//! just neon-test-live
//! ```
//!
//! It creates a throwaway tenant and deletes it, even when an assertion fails.

use cheladb_neon::{NeonId, StorconClient};

fn client() -> StorconClient {
    let url = std::env::var("STORCON_URL")
        .or_else(|_| std::env::var("CHELADB_STORCON_URL"))
        .unwrap_or_else(|_| "http://127.0.0.1:1234".to_string());
    StorconClient::new(&url, 0)
}

#[tokio::test]
#[ignore = "needs the Neon storage stack and the internal listener: just deps-up, then just dev"]
async fn tenant_and_timeline_round_trip() {
    let client = client();
    let tenant = NeonId::new();
    let result = tokio::spawn(round_trip(client.clone(), tenant.clone())).await;
    if let Err(panic) = result {
        // The round trip stopped part-way; don't leave the tenant behind.
        if let Err(e) = client.delete_tenant(&tenant).await {
            eprintln!("could not delete tenant {tenant}: {e}");
        }
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn round_trip(client: StorconClient, tenant: NeonId) {
    let safekeepers = client
        .active_safekeepers()
        .await
        .expect("storage controller reachable (just deps-up)");
    assert!(!safekeepers.is_empty(), "no Active safekeeper");

    client.create_tenant(&tenant).await.unwrap();
    let described = client.describe_tenant(&tenant).await.unwrap();
    assert!(!described.shards.is_empty());
    // No worktree slot knows a throwaway tenant, so its compute notification stays pending (the
    // hook answers 404, which the storage controller expects during a create).
    for shard in &described.shards {
        assert_eq!(shard.last_error, "", "{shard:?}");
    }

    let main = NeonId::new();
    let created = client
        .create_timeline(&tenant, &main, 17, None)
        .await
        .unwrap();
    let fetched = client.get_timeline(&tenant, &main).await.unwrap();
    assert!(fetched.last_record_lsn >= created.last_record_lsn);

    let branch = NeonId::new();
    let at = fetched.last_record_lsn;
    let branched = client
        .create_timeline(&tenant, &branch, 17, Some((&main, Some(at))))
        .await
        .unwrap();
    assert_eq!(branched.ancestor_lsn, Some(at));

    assert_eq!(
        client.pageserver_connstring(&tenant).await.unwrap(),
        "postgresql://no_user@pageserver:6400"
    );

    client.delete_timeline(&tenant, &branch).await.unwrap();
    client.delete_timeline(&tenant, &main).await.unwrap();
    client.delete_tenant(&tenant).await.unwrap();
}
