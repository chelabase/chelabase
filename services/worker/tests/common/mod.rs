//! Helpers shared by the live tests: the real worker, and cleanup for a failed run.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_neon::{ComputeBackend, NeonId};
use chelabase_secrets::SecretKey;
use chelabase_types::DatabaseId;
use worker::config::{RealWorker, WorkerConfig, build};
use worker::ports::AttachSource;
use worker::rt_events::{NoopPublisher, RtPublisher};

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

/// The stack whose computes a live test acts on. In a stack's runner the Compose project
/// (`CHELABASE_COMPOSE_PROJECT`) names it: `chelabase` is main's, `chelabase-<stack>` any
/// other's. Elsewhere the worker's own `CHELABASE_STACK` (`config_stack`).
pub fn stack_name(config_stack: &str) -> String {
    let project = std::env::var("CHELABASE_COMPOSE_PROJECT")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    match project {
        Some(p) if p == "chelabase" => chelabase_neon::DEFAULT_STACK.to_string(),
        Some(p) => p.strip_prefix("chelabase-").unwrap_or(&p).to_string(),
        None => config_stack.to_string(),
    }
}

/// The stack's pageserver container.
pub fn pageserver_container() -> String {
    format!("{}-pageserver-1", compose_project())
}

/// The stack's storage broker container.
pub fn storage_broker_container() -> String {
    format!("{}-storage-broker-1", compose_project())
}

pub async fn setup() -> (RealWorker, MetadataClient, WorkerConfig) {
    setup_with(Arc::new(NoopPublisher)).await
}

/// [`setup`] with realtime's events going to `publisher` (RT1 T5, S20).
pub async fn setup_with(
    publisher: Arc<dyn RtPublisher>,
) -> (RealWorker, MetadataClient, WorkerConfig) {
    let mut config =
        WorkerConfig::from_env().expect("worker config (is CHELABASE_COMPUTE_IMAGE set?)");
    if config.compute_jwt_dir.is_relative() {
        config.compute_jwt_dir = workspace_root().join(&config.compute_jwt_dir);
    }
    for dir in [
        &mut config.compute_ca_dir,
        &mut config.compute_tls_dir,
        &mut config.realtime_tls_dir,
    ] {
        if dir.is_relative() {
            *dir = workspace_root().join(&*dir);
        }
    }
    if let Some(file) = config.realtime_pg_password_file.as_mut()
        && file.is_relative()
    {
        *file = workspace_root().join(&*file);
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
    // Carried gaps T4: provisions wait for the stack orchestrator's attach events, read with
    // the runner's NATS credentials (it has the worker's rights).
    let nats = chelabase_nats::connect(
        &chelabase_nats::NatsConfig::from_env().expect("the runner's NATS config"),
    )
    .await
    .expect("NATS reachable (run through just workbench test)");
    let attach = AttachSource::Nats(async_nats::jetstream::new(nats));
    let worker = build(&config, metadata.clone(), key, publisher, attach).expect("worker");
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
    // `branches.endpoint_id` is ON DELETE SET NULL, so endpoints no branch links to would stay
    // behind: sweep every endpoint of the database's tenant.
    if let Ok(Some(record)) = metadata.get_database(id).await
        && let Some(tenant_hex) = record.neon_tenant_id.as_deref()
    {
        for ep in metadata
            .neon_endpoints_for_tenant(tenant_hex)
            .await
            .unwrap_or_default()
        {
            let _ = worker.computes.delete(&ep).await;
            let _ = metadata.delete_endpoint(&ep).await;
        }
        if let Ok(tenant) = tenant_hex.parse::<NeonId>() {
            let _ = worker.storage.storcon.delete_tenant(&tenant).await;
        }
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
