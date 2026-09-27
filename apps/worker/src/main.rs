use axum::{routing::get, Json, Router};
use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, StartContainerOptions,
};
use bollard::models::{HostConfig, PortBinding};
use bollard::Docker;
use cheladb_metadata::{DatabaseStatus, MetadataClient, MetadataConfig};
use cheladb_nats::{
    connect, ensure_control_stream, run_provision_consumer, JobEnvelope, NatsConfig,
    ProvisionDatabaseJob,
};
use cheladb_types::DatabaseId;
use futures_util::TryStreamExt;
use rand::{distributions::Alphanumeric, Rng};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "worker=info".into()),
        )
        .init();

    let metadata_config = MetadataConfig::from_env().unwrap_or_else(|_| MetadataConfig {
        database_url: "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".into(),
    });
    let nats_config = NatsConfig::from_env().expect("nats config");

    let metadata = MetadataClient::connect(&metadata_config).await?;
    metadata.migrate().await?;

    let nats = connect(&nats_config).await?;
    let js = ensure_control_stream(&nats).await?;

    let docker = Docker::connect_with_local_defaults()?;

    let health_addr: SocketAddr = std::env::var("CHELADB_WORKER_HEALTH_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8082".into())
        .parse()?;
    let health_app = Router::new().route("/health", get(health));
    let listener = tokio::net::TcpListener::bind(health_addr).await?;
    info!("worker health listening on {health_addr}");
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, health_app).await {
            warn!("health server exited: {err}");
        }
    });

    info!("worker consuming provision jobs");
    let meta = metadata.clone();
    run_provision_consumer(js, move |job| {
        let meta = meta.clone();
        let docker = docker.clone();
        async move { handle_provision(meta, docker, job).await }
    })
    .await?;

    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "service": "worker" }))
}

async fn handle_provision(
    metadata: MetadataClient,
    docker: Docker,
    job: JobEnvelope<ProvisionDatabaseJob>,
) -> Result<(), String> {
    let database_id = job.payload.database_id;
    info!(
        job_id = %job.job_id,
        %database_id,
        name = %job.payload.name,
        "provisioning database"
    );

    if let Ok(Some(existing)) = metadata.get_database(database_id).await {
        if existing.status == DatabaseStatus::Ready {
            info!(%database_id, "already ready — skipping");
            return Ok(());
        }
    }

    metadata
        .set_status(database_id, DatabaseStatus::Provisioning)
        .await
        .map_err(|e| e.to_string())?;

    match provision_container(&docker, database_id, &job.payload.name).await {
        Ok((container_id, port, password)) => {
            metadata
                .set_ready(database_id, &container_id, "127.0.0.1", port, &password)
                .await
                .map_err(|e| e.to_string())?;
            info!(
                %database_id,
                %container_id,
                port,
                "database ready (dev password stored in metadata)"
            );
            Ok(())
        }
        Err(err) => {
            warn!(%database_id, "provision failed: {err}");
            let _ = metadata.set_failed(database_id, &err).await;
            Err(err)
        }
    }
}

async fn provision_container(
    docker: &Docker,
    database_id: DatabaseId,
    name: &str,
) -> Result<(String, u16, String), String> {
    let password: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();

    let container_name = format!("cheladb-db-{}", database_id.as_uuid());
    let _ = docker.stop_container(&container_name, None).await;
    let _ = docker.remove_container(&container_name, None).await;

    let mut port_bindings = HashMap::new();
    port_bindings.insert(
        "5432/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_string()),
            host_port: Some("0".to_string()),
        }]),
    );

    let mut labels = HashMap::new();
    labels.insert(
        "cheladb.database_id".to_string(),
        database_id.to_string(),
    );
    labels.insert("cheladb.managed".to_string(), "true".to_string());

    let env = vec![
        format!("POSTGRES_PASSWORD={password}"),
        "POSTGRES_USER=cheladb".to_string(),
        format!("POSTGRES_DB={}", sanitize_db_name(name)),
    ];

    let config = Config {
        image: Some("postgres:17".to_string()),
        env: Some(env),
        labels: Some(labels),
        host_config: Some(HostConfig {
            port_bindings: Some(port_bindings),
            auto_remove: Some(false),
            ..Default::default()
        }),
        exposed_ports: Some(HashMap::from([("5432/tcp".to_string(), HashMap::new())])),
        ..Default::default()
    };

    docker
        .create_image(
            Some(bollard::image::CreateImageOptions {
                from_image: "postgres:17",
                ..Default::default()
            }),
            None,
            None,
        )
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| format!("pull postgres:17: {e}"))?;

    let created = docker
        .create_container(
            Some(CreateContainerOptions {
                name: container_name.clone(),
                platform: None,
            }),
            config,
        )
        .await
        .map_err(|e| format!("create container: {e}"))?;

    docker
        .start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| format!("start container: {e}"))?;

    let port = wait_for_published_port(docker, &created.id).await?;
    wait_for_tcp(port, Duration::from_secs(60)).await?;

    Ok((created.id, port, password))
}

async fn wait_for_published_port(docker: &Docker, container_id: &str) -> Result<u16, String> {
    for _ in 0..30 {
        let inspect = docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| format!("inspect: {e}"))?;

        if let Some(ports) = inspect
            .network_settings
            .as_ref()
            .and_then(|n| n.ports.as_ref())
        {
            if let Some(Some(bindings)) = ports.get("5432/tcp") {
                if let Some(binding) = bindings.first() {
                    if let Some(host_port) = &binding.host_port {
                        if let Ok(port) = host_port.parse::<u16>() {
                            return Ok(port);
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("timed out waiting for published host port".into())
}

async fn wait_for_tcp(port: u16, timeout: Duration) -> Result<(), String> {
    let start = std::time::Instant::now();
    let addr = format!("127.0.0.1:{port}");
    while start.elapsed() < timeout {
        if TcpStream::connect(&addr).await.is_ok() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!("timed out waiting for postgres on {addr}"))
}

fn sanitize_db_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "cheladb".into()
    } else {
        cleaned
    }
}
