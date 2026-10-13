//! Helpers shared by the live tests: the real worker, and cleanup for a failed run.
#![allow(dead_code)]

use std::path::PathBuf;

use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_neon::{ComputeBackend, NeonId};
use chelabase_secrets::SecretKey;
use chelabase_types::DatabaseId;
use worker::config::{RealWorker, WorkerConfig, build};

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The address to dial the proxy at while keeping the TLS host name `<ep>.<domain>`: the stack's
/// `CHELABASE_PROXY_HOSTADDR` when set (the runner cannot reach the proxy on loopback), else
/// the `proxy` service in a stack's runner, else loopback (a proxy published on this host).
pub fn proxy_hostaddr() -> String {
    std::env::var("CHELABASE_PROXY_HOSTADDR")
        .ok()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| {
            if std::env::var_os("CHELABASE_COMPOSE_PROJECT").is_some() {
                // In a stack's runner the proxy is the `proxy` service.
                "proxy".into()
            } else {
                std::net::Ipv4Addr::LOCALHOST.to_string()
            }
        })
}

/// The proxy's CA certificate: `CHELABASE_PROXY_CA_FILE` when set, else the checkout's
/// `.chelabase/neon/proxy-ca.crt`.
pub fn proxy_ca_path() -> PathBuf {
    std::env::var("CHELABASE_PROXY_CA_FILE")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map_or_else(
            || workspace_root().join(".chelabase/neon/proxy-ca.crt"),
            PathBuf::from,
        )
}

/// The stack's compose project name (`CHELABASE_COMPOSE_PROJECT`), else `chelabase`.
pub fn compose_project() -> String {
    std::env::var("CHELABASE_COMPOSE_PROJECT").unwrap_or_else(|_| "chelabase".into())
}

/// The stack's pageserver container.
pub fn pageserver_container() -> String {
    format!("{}-pageserver-1", compose_project())
}

pub async fn setup() -> (RealWorker, MetadataClient, WorkerConfig) {
    let mut config =
        WorkerConfig::from_env().expect("worker config (is CHELABASE_COMPUTE_IMAGE set?)");
    if config.compute_jwt_dir.is_relative() {
        config.compute_jwt_dir = workspace_root().join(&config.compute_jwt_dir);
    }
    let metadata = MetadataClient::connect(&MetadataConfig {
        database_url: std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
        }),
    })
    .await
    .expect("metadata database");
    let key =
        SecretKey::from_env().expect("CHELABASE_SECRETS_KEY (run through just workbench test)");
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
            let _ = worker.storage.storcon.delete_tenant(&tenant).await;
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
