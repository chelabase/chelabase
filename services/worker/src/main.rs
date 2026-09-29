use axum::{routing::get, Json, Router};
use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, StartContainerOptions,
};
use bollard::Docker;
use cheladb_metadata::{DatabaseStatus, MetadataClient, MetadataConfig, ReadyDatabase};
use cheladb_nats::{
    connect, ensure_control_stream, run_delete_consumer, run_provision_consumer, JobEnvelope,
    NatsConfig, ProvisionDatabaseJob, ResetPasswordReply, ResetPasswordRequest,
    SUBJECT_RESET_PASSWORD,
};
use cheladb_secrets::{
    key_check_value, open_credential, seal_credential, verify_key_check, Purpose, SecretKey,
};
use cheladb_types::DatabaseId;
use futures_util::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tracing::{info, warn};
use worker::credentials::generate_password;
use worker::docker_ops::{
    container_name, ensure_volume, labels, observe, remove_database_resources,
};
use worker::names::{sanitize_db_name, volume_name};
use worker::reconcile::{plan, should_abort_provision, Action, STALE_PROVISION_SECS};
use worker::roles::{
    owner_schema_sql, owner_setup_sql, owner_superuser_from_env, platform_connection_config,
    reset_password_statements, rotate_platform_password_sql, OWNER_ROLE, PLATFORM_ROLE,
};

const RECONCILE_EVERY: Duration = Duration::from_secs(30);
/// Every cluster's `postgres` database; the owner role doesn't own it.
const MAINTENANCE_DATABASE: &str = "postgres";
/// Tenant databases run this image until provisioning moves to Neon (roadmap step 4).
const POSTGRES_IMAGE: &str = "postgres:17.11";
/// Hard cap on one reset, below the orchestrator's NATS request timeout.
const RESET_DEADLINE: Duration = Duration::from_secs(8);
/// Hard cap on opening a platform session (TCP + startup handshake).
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// Queue group: with several workers, each reset is handled exactly once.
const RESET_QUEUE_GROUP: &str = "cheladb-worker";

/// Everything a job handler needs.
#[derive(Clone)]
struct Ctx {
    metadata: MetadataClient,
    docker: Docker,
    key: SecretKey,
    /// `CHELADB_DB_OWNER_SUPERUSER`: new databases' owner role is SUPERUSER.
    owner_superuser: bool,
}

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

    let key = SecretKey::from_env()?;
    ensure_secrets_key(&metadata, &key).await?;

    let ctx = Ctx {
        metadata,
        docker: Docker::connect_with_local_defaults()?,
        key,
        owner_superuser: owner_superuser_from_env(
            std::env::var("CHELADB_DB_OWNER_SUPERUSER").ok().as_deref(),
        ),
    };
    if ctx.owner_superuser {
        warn!("CHELADB_DB_OWNER_SUPERUSER is on: new databases' owner role is SUPERUSER");
    }

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

    tokio::spawn(serve_reset_password(ctx.clone(), nats.clone()));
    tokio::spawn(reconcile_loop(ctx.clone()));

    let delete_ctx = ctx.clone();
    let delete_js = js.clone();
    tokio::spawn(async move {
        let result = run_delete_consumer(delete_js, move |job| {
            let ctx = delete_ctx.clone();
            async move { handle_delete(&ctx, job.payload.database_id).await }
        })
        .await;
        if let Err(err) = result {
            warn!("delete consumer exited: {err}");
        }
    });

    info!("worker consuming provision and delete jobs");
    run_provision_consumer(js, move |job| {
        let ctx = ctx.clone();
        async move { handle_provision(&ctx, job).await }
    })
    .await?;

    Ok(())
}

/// Refuse to run with a secrets key that can't open existing credentials.
async fn ensure_secrets_key(metadata: &MetadataClient, key: &SecretKey) -> anyhow::Result<()> {
    use cheladb_metadata::KeyCheck;
    match metadata
        .ensure_key_check(&key_check_value(key), |stored| verify_key_check(key, stored))
        .await?
    {
        KeyCheck::Recorded | KeyCheck::Matches => Ok(()),
        KeyCheck::Replaced => {
            tracing::warn!("secrets key changed; no stored credentials existed, so the new key was adopted");
            Ok(())
        }
        KeyCheck::Mismatch { sealed_rows } => anyhow::bail!(
            "CHELADB_SECRETS_KEY is not the key {sealed_rows} database(s) were encrypted with. \
             Restore the original .cheladb/secrets/secrets.key; in local dev you can instead \
             remove every database with `just sweep-dbs`."
        ),
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "service": "worker" }))
}

// --- Provision ---------------------------------------------------------------

/// A container that's up, with both roles in place.
struct Provisioned {
    container_id: String,
    port: u16,
    database_name: String,
    owner_password: String,
    platform_password: String,
}

async fn handle_provision(ctx: &Ctx, job: JobEnvelope<ProvisionDatabaseJob>) -> Result<(), String> {
    let database_id = job.payload.database_id;
    info!(job_id = %job.job_id, %database_id, name = %job.payload.name, "provisioning database");

    match ctx.metadata.get_database(database_id).await {
        Ok(Some(existing)) if existing.status == DatabaseStatus::Ready => {
            info!(%database_id, "already ready — skipping");
            return Ok(());
        }
        Ok(Some(existing)) if should_abort_provision(existing.status) => {
            info!(%database_id, status = existing.status.as_str(), "deleted before provisioning — skipping");
            return Ok(());
        }
        Ok(None) => {
            warn!(%database_id, "no metadata row — dropping provision job");
            return Ok(());
        }
        Ok(Some(_)) => {}
        Err(err) => return Err(err.to_string()),
    }

    let claimed = ctx
        .metadata
        .set_status(database_id, DatabaseStatus::Provisioning)
        .await
        .map_err(|e| e.to_string())?;
    if !claimed {
        info!(%database_id, "deleted before provisioning — skipping");
        return Ok(());
    }

    let provisioned = match provision(ctx, database_id, &job.payload.name).await {
        Ok(p) => p,
        Err(err) => {
            warn!(%database_id, "provision failed: {err}");
            let _ = remove_database_resources(&ctx.docker, database_id).await;
            // Recorded as `failed` and acked: retrying a broken provision
            // forever helps nobody; the user can delete and recreate.
            ctx.metadata
                .set_failed(database_id, &err)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
    };

    // A delete may have started while the container was coming up.
    let status = ctx
        .metadata
        .get_database(database_id)
        .await
        .map_err(|e| e.to_string())?
        .map(|r| r.status);
    if status.is_some_and(should_abort_provision) {
        info!(%database_id, "deleted during provisioning — tearing down");
        remove_database_resources(&ctx.docker, database_id).await?;
        ctx.metadata
            .mark_deleted(database_id)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    let aad = database_id.to_string();
    let owner_password_enc =
        seal_credential(&ctx.key, &aad, Purpose::Owner, &provisioned.owner_password);
    let platform_password_enc =
        seal_credential(&ctx.key, &aad, Purpose::Platform, &provisioned.platform_password);
    let applied = ctx
        .metadata
        .set_ready(
            database_id,
            ReadyDatabase {
                container_id: &provisioned.container_id,
                host: "127.0.0.1",
                port: provisioned.port,
                database_name: &provisioned.database_name,
                owner_role: OWNER_ROLE,
                owner_password_enc: &owner_password_enc,
                platform_password_enc: &platform_password_enc,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    if !applied {
        // A delete landed between the check above and this write.
        info!(%database_id, "deleted during provisioning — tearing down");
        remove_database_resources(&ctx.docker, database_id).await?;
        return Ok(());
    }
    info!(%database_id, container_id = %provisioned.container_id, port = provisioned.port, "database ready");
    Ok(())
}

async fn provision(ctx: &Ctx, database_id: DatabaseId, name: &str) -> Result<Provisioned, String> {
    let docker = &ctx.docker;
    let database_name = sanitize_db_name(name)?;
    // Only used to initialise Postgres; replaced below (see `platform_password`).
    let init_password = generate_password();
    let platform_password = generate_password();
    let owner_password = generate_password();

    // Not ready yet, so nothing here is worth keeping: start from scratch.
    remove_database_resources(docker, database_id).await?;
    ensure_volume(docker, database_id).await?;

    let port_bindings = HashMap::from([(
        "5432/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_string()),
            host_port: Some("0".to_string()),
        }]),
    )]);
    let config = ContainerCreateBody {
        image: Some(POSTGRES_IMAGE.to_string()),
        env: Some(vec![
            format!("POSTGRES_USER={PLATFORM_ROLE}"),
            format!("POSTGRES_PASSWORD={init_password}"),
            format!("POSTGRES_DB={database_name}"),
        ]),
        labels: Some(labels(database_id)),
        host_config: Some(HostConfig {
            port_bindings: Some(port_bindings),
            binds: Some(vec![format!(
                "{}:/var/lib/postgresql/data",
                volume_name(database_id)
            )]),
            auto_remove: Some(false),
            ..Default::default()
        }),
        exposed_ports: Some(vec!["5432/tcp".to_string()]),
        ..Default::default()
    };

    docker
        .create_image(
            Some(CreateImageOptions {
                from_image: Some(POSTGRES_IMAGE.to_string()),
                ..Default::default()
            }),
            None,
            None,
        )
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| format!("pull {POSTGRES_IMAGE}: {e}"))?;

    let created = docker
        .create_container(
            Some(CreateContainerOptions {
                name: Some(container_name(database_id)),
                ..Default::default()
            }),
            config,
        )
        .await
        .map_err(|e| format!("create container: {e}"))?;
    docker
        .start_container(&created.id, None::<StartContainerOptions>)
        .await
        .map_err(|e| format!("start container: {e}"))?;

    let port = wait_for_published_port(docker, &created.id).await?;
    wait_for_tcp(port, Duration::from_secs(60)).await?;

    // The image's entrypoint restarts Postgres once after init, so retry.
    let admin = connect_platform_with_retry(port, &init_password, MAINTENANCE_DATABASE).await?;
    admin
        .batch_execute(&rotate_platform_password_sql(&platform_password))
        .await
        .map_err(|e| pg_error("rotate platform password", &e))?;
    for statement in owner_setup_sql(&database_name, &owner_password, ctx.owner_superuser) {
        admin
            .batch_execute(&statement)
            .await
            .map_err(|e| pg_error("owner role setup", &e))?;
    }
    let in_db = connect_platform(port, &platform_password, &database_name).await?;
    for statement in owner_schema_sql() {
        in_db
            .batch_execute(&statement)
            .await
            .map_err(|e| pg_error("owner schema setup", &e))?;
    }

    Ok(Provisioned {
        container_id: created.id,
        port,
        database_name,
        owner_password,
        platform_password,
    })
}

// --- Delete ------------------------------------------------------------------

async fn handle_delete(ctx: &Ctx, database_id: DatabaseId) -> Result<(), String> {
    remove_database_resources(&ctx.docker, database_id).await?;
    ctx.metadata
        .mark_deleted(database_id)
        .await
        .map_err(|e| e.to_string())?;
    info!(%database_id, "database deleted");
    Ok(())
}

// --- Reset password (core NATS request/reply) --------------------------------

async fn serve_reset_password(ctx: Ctx, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_RESET_PASSWORD, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("reset-password subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_RESET_PASSWORD, "serving reset-password requests");
    // One task per request, each with a hard deadline: a stuck database can
    // only fail its own reset, never delay another tenant's.
    while let Some(message) = requests.next().await {
        let ctx = ctx.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply = match serde_json::from_slice::<ResetPasswordRequest>(&message.payload) {
                Ok(request) => {
                    let id = request.database_id;
                    match tokio::time::timeout(RESET_DEADLINE, reset_password(&ctx, id)).await {
                        Ok(Ok(())) => ResetPasswordReply { ok: true, error: None },
                        Ok(Err(err)) => {
                            warn!(database_id = %id, "reset password failed: {err}");
                            ResetPasswordReply { ok: false, error: Some(err) }
                        }
                        Err(_) => {
                            warn!(database_id = %id, "reset password timed out");
                            ResetPasswordReply { ok: false, error: Some("timed out".into()) }
                        }
                    }
                }
                Err(err) => ResetPasswordReply {
                    ok: false,
                    error: Some(format!("bad request: {err}")),
                },
            };
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!("reset-password reply failed: {err}");
                }
            }
        });
    }
}

/// Errors are sent back to the orchestrator; they never contain a password.
async fn reset_password(ctx: &Ctx, database_id: DatabaseId) -> Result<(), String> {
    let row = ctx
        .metadata
        .get_database(database_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("not found")?;
    if row.status != DatabaseStatus::Ready {
        return Err("not ready".into());
    }
    let (Some(port), Some(platform_enc)) = (row.port, row.platform_password_enc) else {
        return Err("credentials unavailable".into());
    };
    let aad = database_id.to_string();
    let platform_password = open_credential(&ctx.key, &aad, Purpose::Platform, &platform_enc)
        .map_err(|e| e.to_string())?;
    let new_password = generate_password();

    // ALTER ROLE is cluster-wide: use the maintenance database, which the owner
    // can't rename, drop, lock or reconfigure (unlike their own database).
    let client = connect_platform(port, &platform_password, MAINTENANCE_DATABASE).await?;
    for statement in reset_password_statements(&new_password) {
        client
            .batch_execute(&statement)
            .await
            .map_err(|e| pg_error("alter role", &e))?;
    }
    let stored = ctx
        .metadata
        .set_owner_password_enc(
            database_id,
            &seal_credential(&ctx.key, &aad, Purpose::Owner, &new_password),
        )
        .await
        .map_err(|e| e.to_string())?;
    if !stored {
        // Deleted while we were resetting; nothing left to update.
        return Err("not ready".into());
    }
    info!(%database_id, "owner password reset");
    Ok(())
}

// --- Reconcile ---------------------------------------------------------------

async fn reconcile_loop(ctx: Ctx) {
    // First pass after one period, so queued jobs are picked up (and their rows
    // refreshed) before reconcile judges anything stale.
    let mut every =
        tokio::time::interval_at(tokio::time::Instant::now() + RECONCILE_EVERY, RECONCILE_EVERY);
    loop {
        every.tick().await;
        if let Err(err) = reconcile_once(&ctx).await {
            warn!("reconcile: {err}");
        }
    }
}

async fn reconcile_once(ctx: &Ctx) -> Result<(), String> {
    // Observe Docker *before* reading rows: a row always exists before its
    // container, so nothing created in between can look like an orphan.
    let observed = observe(&ctx.docker).await?;
    let rows = ctx
        .metadata
        .list_live_for_reconcile()
        .await
        .map_err(|e| e.to_string())?;
    let observed_ids: Vec<DatabaseId> = observed
        .containers
        .union(&observed.volumes)
        .copied()
        .collect();
    let deleted = ctx
        .metadata
        .deleted_among(&observed_ids)
        .await
        .map_err(|e| e.to_string())?;

    for action in plan(&rows, &deleted, &observed) {
        info!(?action, "reconcile");
        let result = match action {
            Action::FailStale(id) => {
                // Re-checked at write time: only tear down if it really is stale.
                match ctx.metadata.fail_if_stale(id, STALE_PROVISION_SECS).await {
                    Ok(true) => remove_database_resources(&ctx.docker, id).await,
                    Ok(false) => Ok(()),
                    Err(err) => Err(err.to_string()),
                }
            }
            Action::FailMissingContainer(id) => ctx
                .metadata
                .set_failed(id, "container missing")
                .await
                .map_err(|e| e.to_string()),
            Action::RetryDelete(id) => handle_delete(ctx, id).await,
            Action::RemoveOrphan(id) => remove_database_resources(&ctx.docker, id).await,
            Action::ReportUnknown(id) => {
                warn!(
                    database_id = %id,
                    "managed container/volume has no metadata row; keeping it. Restore the \
                     metadata, or remove it with `just sweep-dbs` if it's disposable"
                );
                Ok(())
            }
        };
        if let Err(err) = result {
            warn!("reconcile action failed: {err}");
        }
    }
    Ok(())
}

// --- Postgres / Docker helpers -----------------------------------------------

/// A Postgres error with the server's own message and SQLSTATE (tokio-postgres
/// only says "db error" in its Display). Never includes the SQL text, so a
/// password in the statement can't leak into logs or replies.
fn pg_error(context: &str, err: &tokio_postgres::Error) -> String {
    match err.as_db_error() {
        Some(db) => format!("{context}: {} ({})", db.message(), db.code().code()),
        None => format!("{context}: {err}"),
    }
}

async fn connect_platform(
    port: u16,
    password: &str,
    database: &str,
) -> Result<tokio_postgres::Client, String> {
    let config = platform_connection_config(port, password, database);
    let (client, connection) = tokio::time::timeout(
        CONNECT_DEADLINE,
        tokio_postgres::connect(&config, tokio_postgres::NoTls),
    )
    .await
    .map_err(|_| format!("connect as {PLATFORM_ROLE} to {database}: timed out"))?
    .map_err(|e| format!("connect as {PLATFORM_ROLE} to {database}: {e}"))?;
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            warn!("postgres connection: {err}");
        }
    });
    Ok(client)
}

async fn connect_platform_with_retry(
    port: u16,
    password: &str,
    database: &str,
) -> Result<tokio_postgres::Client, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match connect_platform(port, password, database).await {
            Ok(client) => return Ok(client),
            Err(err) if std::time::Instant::now() < deadline => {
                tracing::debug!("waiting for postgres: {err}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(err) => return Err(err),
        }
    }
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
            && let Some(Some(bindings)) = ports.get("5432/tcp")
            && let Some(binding) = bindings.first()
            && let Some(host_port) = &binding.host_port
            && let Ok(port) = host_port.parse::<u16>()
        {
            return Ok(port);
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
