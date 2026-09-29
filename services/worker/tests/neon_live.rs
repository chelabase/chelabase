//! The whole lifecycle against the real Neon stack: provision, reset, delete.
//!
//! Needs `just deps-up` (storage controller, proxy, Valkey, metadata database, Docker)
//! and `just dev` running (computes pull their spec from the orchestrator's internal
//! listener on :8083), plus `CHELADB_COMPUTE_IMAGE` exported from `scripts/neon-images.sh`:
//!
//! ```text
//! set -a; . .cheladb/zitadel/demo.env; eval "$(scripts/neon-images.sh)"
//! CHELADB_COMPUTE_IMAGE=$COMPUTE_NODE_IMAGE \
//!   cargo test -p worker --test neon_live -- --ignored --test-threads=1 --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cheladb_metadata::{
    DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus, MetadataClient, MetadataConfig,
};
use cheladb_neon::{ComputeBackend, NeonId, StorconClient, StorconError};
use cheladb_secrets::SecretKey;
use cheladb_types::DatabaseId;
use worker::config::{RealWorker, WorkerConfig, build};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

async fn setup() -> (RealWorker, MetadataClient, WorkerConfig) {
    let mut config =
        WorkerConfig::from_env().expect("worker config (is CHELADB_COMPUTE_IMAGE set?)");
    if config.compute_jwt_dir.is_relative() {
        config.compute_jwt_dir = workspace_root().join(&config.compute_jwt_dir);
    }
    let metadata = MetadataClient::connect(&MetadataConfig {
        database_url: std::env::var("CHELADB_METADATA_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".into()
        }),
    })
    .await
    .expect("metadata database");
    let key =
        SecretKey::from_env().expect("CHELADB_SECRETS_KEY (source .cheladb/zitadel/demo.env)");
    let worker = build(&config, metadata.clone(), key).expect("worker");
    (worker, metadata, config)
}

/// Best-effort cleanup for a failed run: the compute, the tenant, and the rows.
async fn cleanup(worker: &RealWorker, metadata: &MetadataClient, id: DatabaseId) {
    if let Ok(Some(record)) = metadata.get_database(id).await
        && let Some(ep) = record.endpoint_id
    {
        let _ = worker.computes.delete(&ep).await;
        if let Ok(Some(endpoint)) = metadata.neon_endpoint(&ep).await
            && let Ok(tenant) = endpoint.tenant_id.parse::<NeonId>()
        {
            let _ = worker.storage.delete_tenant(&tenant).await;
        }
        let _ = metadata.delete_endpoint(&ep).await;
    }
    let _ = sqlx_delete(metadata, id).await;
}

async fn sqlx_delete(metadata: &MetadataClient, id: DatabaseId) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM databases WHERE id = $1")
        .bind(id.as_uuid())
        .execute(metadata.pool())
        .await
        .map(drop)
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn provision_reset_and_delete_a_database() {
    let (worker, metadata, config) = setup().await;
    let worker = Arc::new(worker);
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");

    // A panic inside the task is caught here, so the cleanup below always runs.
    let outcome = tokio::spawn({
        let (worker, metadata, config) = (worker.clone(), metadata.clone(), config.clone());
        async move { lifecycle(&worker, &metadata, &config, id, &name).await }
    })
    .await;
    if let Err(panic) = outcome {
        cleanup(&worker, &metadata, id).await;
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn lifecycle(
    worker: &RealWorker,
    metadata: &MetadataClient,
    config: &WorkerConfig,
    id: DatabaseId,
    name: &str,
) {
    // --- provision ---
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(90), worker.handle_provision(id, name))
        .await
        .expect("provision took over 90 s")
        .expect("handle_provision");
    println!("provision took {:?}", started.elapsed());
    let record = metadata.get_database(id).await.unwrap().unwrap();
    assert_eq!(
        record.status,
        DatabaseStatus::Ready,
        "status {:?}, error {:?}",
        record.status,
        record.error
    );
    let ep = record.endpoint_id.clone().expect("endpoint id");
    let database_name = record.database_name.clone().expect("database name");
    let endpoint = metadata.neon_endpoint(&ep).await.unwrap().unwrap();
    let tenant: NeonId = endpoint.tenant_id.parse().unwrap();

    let roles = worker
        .computes
        .admin_sql(
            &ep,
            &database_name,
            "SELECT rolname || ',' || rolcanlogin || ',' || rolbypassrls FROM pg_roles \
             WHERE rolname IN ('postgres','anon','authenticated','authenticator') ORDER BY 1;",
        )
        .await
        .expect("admin_sql");
    println!("roles:\n{roles}");
    for expected in [
        "postgres,true,true",
        "anon,false,false",
        "authenticated,false,false",
        "authenticator,false,false",
    ] {
        assert!(roles.contains(expected), "missing {expected} in:\n{roles}");
    }
    assert_eq!(roles.matches("anon,").count(), 1);
    let members = worker
        .computes
        .admin_sql(
            &ep,
            &database_name,
            "SELECT count(*) FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.roleid \
             JOIN pg_roles u ON u.oid = m.member WHERE u.rolname = 'authenticator' \
             AND r.rolname IN ('anon','authenticated');",
        )
        .await
        .unwrap();
    assert!(members.contains('2'), "authenticator's grants: {members}");

    // --- reset ---
    let before = metadata.neon_roles(&ep).await.unwrap();
    let before = before
        .iter()
        .find(|r| r.name == "postgres")
        .unwrap()
        .scram_verifier
        .clone();
    let started = Instant::now();
    worker.reset_password(id).await.expect("reset_password");
    println!("reset took {:?}", started.elapsed());
    let after = metadata.neon_roles(&ep).await.unwrap();
    let after = after
        .iter()
        .find(|r| r.name == "postgres")
        .unwrap()
        .scram_verifier
        .clone();
    assert_ne!(before, after, "the verifier didn't change");
    let states = metadata.list_endpoints_for_reconcile().await.unwrap();
    let state = states.iter().find(|s| s.id == ep).expect("endpoint state");
    assert!(!state.needs_reconfigure, "flag still set after reset");

    // --- delete ---
    worker.handle_delete(id).await.expect("handle_delete");
    assert_eq!(
        worker.computes.status(&ep).await.unwrap(),
        None,
        "container still there"
    );
    let storcon = StorconClient::new(&config.storcon_url, 0);
    match storcon.describe_tenant(&tenant).await {
        Err(StorconError::Rejected { status, .. }) => assert_eq!(status.as_u16(), 404),
        other => panic!("tenant still described: {other:?}"),
    }
    assert!(
        metadata.neon_endpoint(&ep).await.unwrap().is_none(),
        "endpoint row remains"
    );
    let record = metadata.get_database(id).await.unwrap().unwrap();
    assert_eq!(record.status, DatabaseStatus::Deleted);
    // The tombstone of a test database isn't worth keeping.
    sqlx_delete(metadata, id).await.unwrap();
}
