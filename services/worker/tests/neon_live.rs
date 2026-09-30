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

use std::sync::Arc;
use std::time::{Duration, Instant};

use cheladb_metadata::{
    DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus, MetadataClient,
};
use cheladb_neon::{ComputeBackend, NeonId, StorconClient, StorconError};
use cheladb_types::{BranchId, DatabaseId};
use worker::config::{RealWorker, WorkerConfig};

mod common;
use common::{cleanup, setup, sqlx_delete};

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
    let main = metadata
        .get_branch(BranchId::from(id))
        .await
        .unwrap()
        .expect("main branch");
    let ep = main.endpoint_id.clone().expect("endpoint id");
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
    worker.reset_password(cheladb_types::BranchId::from(id)).await.expect("reset_password");
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
    let main = metadata.get_branch(BranchId::from(id)).await.unwrap().unwrap();
    assert_eq!(main.status, cheladb_metadata::BranchStatus::Deleted);
    // The tombstone of a test database isn't worth keeping.
    sqlx_delete(metadata, id).await.unwrap();
}
