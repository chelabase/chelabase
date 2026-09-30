//! Helpers shared by the live tests: the real worker, and cleanup for a failed run.
#![allow(dead_code)]

use std::path::PathBuf;

use cheladb_metadata::{MetadataClient, MetadataConfig};
use cheladb_neon::{ComputeBackend, NeonId};
use cheladb_secrets::SecretKey;
use cheladb_types::DatabaseId;
use worker::config::{RealWorker, WorkerConfig, build};

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub async fn setup() -> (RealWorker, MetadataClient, WorkerConfig) {
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

/// Best-effort cleanup for a failed run: every branch's compute, the tenant, and the rows.
pub async fn cleanup(worker: &RealWorker, metadata: &MetadataClient, id: DatabaseId) {
    for branch in metadata.list_branches(id).await.unwrap_or_default() {
        let Some(ep) = branch.endpoint_id else {
            continue;
        };
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

pub async fn sqlx_delete(metadata: &MetadataClient, id: DatabaseId) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM branches WHERE database_id = $1")
        .bind(id.as_uuid())
        .execute(metadata.pool())
        .await?;
    sqlx::query("DELETE FROM databases WHERE id = $1")
        .bind(id.as_uuid())
        .execute(metadata.pool())
        .await
        .map(drop)
}
