//! Opt-in tests of the Neon endpoint queries against a real metadata Postgres
//! (`just deps-up` first):
//!
//! ```sh
//! cargo test -p cheladb-metadata --test neon_live -- --ignored --test-threads=1
//! ```
//!
//! Each test creates its own endpoint and deletes it.

use cheladb_metadata::{MetadataClient, MetadataConfig, NeonDatabase, NeonEndpoint, NeonRole};

async fn client() -> MetadataClient {
    let database_url = std::env::var("CHELADB_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".to_string()
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
    for (name, verifier) in [("app", "SCRAM-SHA-256$app"), ("ro", "SCRAM-SHA-256$ro")] {
        sqlx::query(
            "INSERT INTO neon_roles (endpoint_id, name, scram_verifier) VALUES ($1, $2, $3)",
        )
        .bind(&ep.id)
        .bind(name)
        .bind(verifier)
        .execute(pool)
        .await
        .unwrap();
    }
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
            vec![
                NeonRole {
                    name: "app".into(),
                    scram_verifier: "SCRAM-SHA-256$app".into()
                },
                NeonRole {
                    name: "ro".into(),
                    scram_verifier: "SCRAM-SHA-256$ro".into()
                },
            ]
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
