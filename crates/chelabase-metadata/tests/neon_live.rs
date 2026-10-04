//! Opt-in tests of the Neon endpoint queries against a real metadata Postgres
//! (`just deps-up` first):
//!
//! ```sh
//! cargo test -p chelabase-metadata --test neon_live -- --ignored --test-threads=1
//! ```
//!
//! Each test creates its own endpoint and deletes it.

use chelabase_metadata::{MetadataClient, MetadataConfig, NeonDatabase, NeonEndpoint, NeonRole};

async fn client() -> MetadataClient {
    let database_url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata".to_string()
    });
    let client = MetadataClient::connect(&MetadataConfig { database_url })
        .await
        .expect("metadata Postgres reachable (just deps-up)");
    client.migrate().await.unwrap();
    client
}

fn hex32() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// An endpoint with roles `app` and `ro`, and database `app` owned by `app`.
async fn endpoint(client: &MetadataClient) -> NeonEndpoint {
    let ep = NeonEndpoint {
        id: format!("ep-{}", &hex32()[..12]),
        tenant_id: hex32(),
        timeline_id: hex32(),
        compute_token_sha256: "ab".repeat(32),
    };
    let pool = client.pool();
    sqlx::query("INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256) VALUES ($1, $2, $3, $4)")
        .bind(&ep.id).bind(&ep.tenant_id).bind(&ep.timeline_id).bind(&ep.compute_token_sha256)
        .execute(pool).await.unwrap();
    // `app` is the owner; `ro` is a role made through the API (an endpoint has one owner).
    sqlx::query("INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, 'app', 'SCRAM-SHA-256$app', 'owner')")
        .bind(&ep.id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin, password_enc)
         VALUES ($1, 'ro', 'SCRAM-SHA-256$ro', 'api', 'v1:ro')",
    )
    .bind(&ep.id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO neon_databases (endpoint_id, name, owner) VALUES ($1, 'app', 'app')")
        .bind(&ep.id)
        .execute(pool)
        .await
        .unwrap();
    ep
}

/// Runs `checks` on a fresh endpoint and deletes it afterwards, also when a check panics.
async fn with_endpoint<F, Fut>(checks: F)
where
    F: FnOnce(MetadataClient, NeonEndpoint) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let client = client().await;
    let ep = endpoint(&client).await;
    let outcome = tokio::spawn(checks(client.clone(), ep.clone())).await;
    delete(&client, &ep.id).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn delete(client: &MetadataClient, id: &str) {
    sqlx::query("DELETE FROM neon_endpoints WHERE id = $1")
        .bind(id)
        .execute(client.pool())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn finds_an_endpoint_its_roles_and_databases() {
    with_endpoint(|client, ep| async move {
        assert_eq!(
            client.neon_endpoint(&ep.id).await.unwrap(),
            Some(ep.clone())
        );
        assert_eq!(client.neon_endpoint("ep-missing").await.unwrap(), None);
        assert_eq!(
            client.neon_roles(&ep.id).await.unwrap(),
            vec![NeonRole {
                name: "app".into(),
                scram_verifier: "SCRAM-SHA-256$app".into(),
            }]
        );
        assert_eq!(
            client.neon_databases(&ep.id).await.unwrap(),
            vec![NeonDatabase {
                name: "app".into(),
                owner: "app".into()
            }]
        );
        assert_eq!(
            client
                .neon_endpoints_for_tenant(&ep.tenant_id)
                .await
                .unwrap(),
            vec![ep.id.clone()]
        );
        assert_eq!(
            client
                .neon_endpoints_for_timeline(&ep.timeline_id)
                .await
                .unwrap(),
            vec![ep.id.clone()]
        );
        assert!(
            client
                .neon_endpoints_for_tenant(&hex32())
                .await
                .unwrap()
                .is_empty()
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn role_secrets_tell_unknown_endpoints_from_unknown_roles() {
    with_endpoint(|client, ep| async move {
        assert_eq!(
            client.neon_role_secret(&ep.id, "app").await.unwrap(),
            Some(Some("SCRAM-SHA-256$app".into()))
        );
        assert_eq!(
            client.neon_role_secret(&ep.id, "nobody").await.unwrap(),
            Some(None)
        );
        assert_eq!(
            client.neon_role_secret("ep-missing", "app").await.unwrap(),
            None
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn deleting_an_endpoint_deletes_its_roles_and_databases() {
    with_endpoint(|client, ep| async move {
        delete(&client, &ep.id).await;

        assert!(client.neon_roles(&ep.id).await.unwrap().is_empty());
        assert!(client.neon_databases(&ep.id).await.unwrap().is_empty());
        let (left,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM neon_databases WHERE endpoint_id = $1")
                .bind(&ep.id)
                .fetch_one(client.pool())
                .await
                .unwrap();
        assert_eq!(left, 0);
    })
    .await;
}

async fn needs_reconfigure(client: &MetadataClient, ep: &str) -> bool {
    sqlx::query_scalar("SELECT needs_reconfigure FROM neon_endpoints WHERE id = $1")
        .bind(ep)
        .fetch_one(client.pool())
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn clear_needs_reconfigure_only_for_the_applied_version() {
    with_endpoint(|client, ep| async move {
        sqlx::query("UPDATE neon_endpoints SET needs_reconfigure = true WHERE id = $1")
            .bind(&ep.id)
            .execute(client.pool())
            .await
            .unwrap();
        let (_, version) = client.neon_roles_versioned(&ep.id).await.unwrap();

        // A stale version (a newer write landed meanwhile) and an unknown endpoint leave the
        // flag alone.
        assert!(!client.clear_needs_reconfigure(&ep.id, version + 1).await.unwrap());
        assert!(!client.clear_needs_reconfigure("ep-missing", version).await.unwrap());
        assert!(needs_reconfigure(&client, &ep.id).await);

        assert!(client.clear_needs_reconfigure(&ep.id, version).await.unwrap());
        assert!(!needs_reconfigure(&client, &ep.id).await);
        // Already clear: nothing to do.
        assert!(!client.clear_needs_reconfigure(&ep.id, version).await.unwrap());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn replace_compute_token_stores_the_new_hash() {
    with_endpoint(|client, ep| async move {
        client.replace_compute_token(&ep.id, &"ef".repeat(32)).await.unwrap();
        let found = client.neon_endpoint(&ep.id).await.unwrap().unwrap();
        assert_eq!(found.compute_token_sha256, "ef".repeat(32));
    })
    .await;
}

// A newer write commits while the clear waits on the endpoint row: the clear must recheck the
// committed version, not its old snapshot, or the newer write is never pushed.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn clear_needs_reconfigure_rechecks_the_version_after_a_lock_wait() {
    with_endpoint(|client, ep| async move {
        sqlx::query("UPDATE neon_endpoints SET needs_reconfigure = true WHERE id = $1")
            .bind(&ep.id)
            .execute(client.pool())
            .await
            .unwrap();
        let (_, version) = client.neon_roles_versioned(&ep.id).await.unwrap();

        // Write B, not yet committed: it holds the endpoint row (every writer locks it first).
        let mut tx = client.pool().begin().await.unwrap();
        sqlx::query("UPDATE neon_roles SET scram_verifier = 'SCRAM-SHA-256$v2' WHERE endpoint_id = $1 AND name = 'app'")
            .bind(&ep.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE neon_endpoints SET needs_reconfigure = true, roles_version = roles_version + 1 WHERE id = $1")
            .bind(&ep.id)
            .execute(&mut *tx)
            .await
            .unwrap();

        let (c, id) = (client.clone(), ep.id.clone());
        let clear = tokio::spawn(async move {
            c.clear_needs_reconfigure(&id, version).await.unwrap()
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tx.commit().await.unwrap();

        assert!(!clear.await.unwrap(), "cleared a flag whose version had changed");
        assert!(needs_reconfigure(&client, &ep.id).await);
    })
    .await;
}

// Review I1: one holder per endpoint across clients (worker processes); dropping releases it.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn lock_endpoint_admits_one_holder_until_dropped() {
    use std::time::Duration;
    let (a, b) = (client().await, client().await);
    let ep = format!("ep-{}", &hex32()[..12]);
    let other = format!("ep-{}", &hex32()[..12]);

    let held = a.lock_endpoint(&ep).await.unwrap();
    let waiting = tokio::time::timeout(Duration::from_millis(300), b.lock_endpoint(&ep)).await;
    assert!(waiting.is_err(), "a second client got a held lock");
    // Other endpoints aren't blocked.
    drop(b.lock_endpoint(&other).await.unwrap());

    drop(held);
    tokio::time::timeout(Duration::from_secs(5), b.lock_endpoint(&ep))
        .await
        .expect("the lock is released on drop")
        .unwrap();
}

// A waiter gives up on its own, well inside the reset deadline, with an error that says why.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn lock_endpoint_gives_up_with_a_clear_error() {
    use std::time::{Duration, Instant};
    let (a, b) = (client().await, client().await);
    let ep = format!("ep-{}", &hex32()[..12]);

    let held = a.lock_endpoint(&ep).await.unwrap();
    let started = Instant::now();
    let err = tokio::time::timeout(Duration::from_secs(8), b.lock_endpoint(&ep))
        .await
        .expect("the wait is bounded below the 8 s reset deadline")
        .err()
        .expect("a second holder while the first still holds it");
    assert!(started.elapsed() >= Duration::from_secs(4), "gave up too early");
    assert_eq!(
        err.to_string(),
        format!("another password change for {ep} is still being applied")
    );
    drop(held);
    drop(b.lock_endpoint(&ep).await.unwrap());
}

// Waiters are served first come, first served. Polling would pick one at random each round.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn lock_endpoint_serves_waiters_in_order() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    let (a, b, c) = (client().await, client().await, client().await);
    let ep = format!("ep-{}", &hex32()[..12]);

    for round in 0..5 {
        let held = a.lock_endpoint(&ep).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiters = Vec::new();
        for (name, client) in [("b", b.clone()), ("c", c.clone())] {
            let (ep, order) = (ep.clone(), order.clone());
            waiters.push(tokio::spawn(async move {
                let lock = client.lock_endpoint(&ep).await.unwrap();
                order.lock().unwrap().push(name);
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(lock);
            }));
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        drop(held);
        for waiter in waiters {
            waiter.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), ["b", "c"], "round {round}");
    }
}
