use axum::{Json, Router, routing::get};
use cheladb_metadata::{MetadataClient, MetadataConfig};
use cheladb_nats::{
    CheckDataRoleReply, CheckDataRoleRequest, NatsConfig, RESET_NOT_APPLIED, ResetPasswordReply,
    ResetPasswordRequest, RoleReply, SUBJECT_CHECK_DATA_ROLE, SUBJECT_RESET_PASSWORD,
    SUBJECT_ROLE_CREATE, SUBJECT_ROLE_DELETE, SUBJECT_ROLE_RESET, connect, ensure_control_stream,
    run_branch_consumer, run_delete_consumer, run_provision_consumer, run_settings_consumer,
};
use cheladb_secrets::{SecretKey, key_check_value, verify_key_check};
use cheladb_types::mode::Mode;
use futures_util::StreamExt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use worker::branch_job::Attempt;
use worker::config::{RealWorker, WorkerConfig, build};

const RECONCILE_EVERY: Duration = Duration::from_secs(30);
/// Hard cap on one reset, below the orchestrator's NATS request timeout (`RESET_TIMEOUT`,
/// 15 s in the orchestrator): keep at least 5 s between them.
const RESET_DEADLINE: Duration = Duration::from_secs(8);
/// Queue group: with several workers, each reset is handled exactly once.
const RESET_QUEUE_GROUP: &str = "cheladb-worker";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "worker=info".into()),
        )
        .init();

    // Before anything is contacted, so a missing image is the first thing a user sees.
    let config = WorkerConfig::from_env()?;
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

    let worker = Arc::new(build(&config, metadata, key)?);
    if config.owner_superuser_requested && config.mode == Mode::Platform {
        warn!("CHELADB_DB_OWNER_SUPERUSER is ignored in platform mode");
    }
    if worker.owner_superuser {
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

    tokio::spawn(serve_reset_password(worker.clone(), nats.clone()));
    tokio::spawn(serve_check_data_role(worker.clone(), nats.clone()));
    for subject in [SUBJECT_ROLE_CREATE, SUBJECT_ROLE_RESET, SUBJECT_ROLE_DELETE] {
        tokio::spawn(serve_role_rpc(worker.clone(), nats.clone(), subject));
    }
    tokio::spawn(reconcile_loop(worker.clone()));

    let delete_worker = worker.clone();
    let delete_js = js.clone();
    tokio::spawn(async move {
        let result = run_delete_consumer(delete_js, move |job| {
            let worker = delete_worker.clone();
            async move { worker.handle_delete(job.payload.database_id).await }
        })
        .await;
        if let Err(err) = result {
            warn!("delete consumer exited: {err}");
        }
    });

    let branch_worker = worker.clone();
    let branch_js = js.clone();
    tokio::spawn(async move {
        let result = run_branch_consumer(branch_js, move |job, attempt| {
            let worker = branch_worker.clone();
            async move {
                worker
                    .handle_branch_job_attempt(job.payload, Attempt(attempt))
                    .await
            }
        })
        .await;
        if let Err(err) = result {
            warn!("branch consumer exited: {err}");
        }
    });

    let settings_worker = worker.clone();
    let settings_js = js.clone();
    tokio::spawn(async move {
        let result = run_settings_consumer(settings_js, move |job| {
            let worker = settings_worker.clone();
            async move { worker.handle_database_settings(job.payload.database_id).await }
        })
        .await;
        if let Err(err) = result {
            warn!("settings consumer exited: {err}");
        }
    });

    info!("worker consuming provision, delete, branch and settings jobs");
    run_provision_consumer(js, move |job| {
        let worker = worker.clone();
        async move {
            worker
                .handle_provision(job.payload.database_id, &job.payload.name)
                .await
        }
    })
    .await?;

    Ok(())
}

/// Refuse to run with a secrets key that can't open existing credentials.
async fn ensure_secrets_key(metadata: &MetadataClient, key: &SecretKey) -> anyhow::Result<()> {
    use cheladb_metadata::KeyCheck;
    match metadata
        .ensure_key_check(&key_check_value(key), |stored| {
            verify_key_check(key, stored)
        })
        .await?
    {
        KeyCheck::Recorded | KeyCheck::Matches => Ok(()),
        KeyCheck::Replaced => {
            tracing::warn!(
                "secrets key changed; no stored credentials existed, so the new key was adopted"
            );
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

// --- Reset password (core NATS request/reply) --------------------------------

async fn serve_reset_password(worker: Arc<RealWorker>, client: async_nats::Client) {
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
    info!(
        subject = SUBJECT_RESET_PASSWORD,
        "serving reset-password requests"
    );
    // One task per request, each with a hard deadline: a stuck database can
    // only fail its own reset, never delay another tenant's.
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply = match serde_json::from_slice::<ResetPasswordRequest>(&message.payload) {
                Ok(request) => {
                    let id = request.branch_id;
                    match tokio::time::timeout(RESET_DEADLINE, worker.reset_password(id)).await {
                        Ok(Ok(())) => ResetPasswordReply {
                            ok: true,
                            error: None,
                        },
                        Ok(Err(err)) => {
                            warn!(branch_id = %id, "reset password failed: {err}");
                            ResetPasswordReply {
                                ok: false,
                                error: Some(err),
                            }
                        }
                        // The metadata transaction almost always committed before the slow
                        // push or publish, so the password is changed but not yet applied.
                        Err(_) => {
                            warn!(branch_id = %id, "reset password timed out");
                            ResetPasswordReply {
                                ok: false,
                                error: Some(RESET_NOT_APPLIED.into()),
                            }
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

// --- Reconcile ---------------------------------------------------------------

async fn reconcile_loop(worker: Arc<RealWorker>) {
    // First pass after one period, so queued jobs are picked up (and their rows
    // refreshed) before reconcile judges anything stale.
    let mut every = tokio::time::interval_at(
        tokio::time::Instant::now() + RECONCILE_EVERY,
        RECONCILE_EVERY,
    );
    loop {
        every.tick().await;
        if let Err(err) = worker.reconcile_once().await {
            warn!("reconcile: {err}");
        }
    }
}

// --- Check a secret key's role (core NATS request/reply) ----------------------

/// Hard cap on one check, below the orchestrator's request timeout (15 s).
const CHECK_ROLE_DEADLINE: Duration = Duration::from_secs(10);

async fn serve_check_data_role(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_CHECK_DATA_ROLE, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("check-data-role subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_CHECK_DATA_ROLE, "serving check-data-role requests");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply = match serde_json::from_slice::<CheckDataRoleRequest>(&message.payload) {
                Ok(request) => {
                    match tokio::time::timeout(
                        CHECK_ROLE_DEADLINE,
                        worker.check_data_role_reply(&request),
                    )
                    .await
                    {
                        Ok(reply) => reply,
                        Err(_) => {
                            warn!(branch_id = %request.branch_id, "check data role timed out");
                            CheckDataRoleReply::not_ready()
                        }
                    }
                }
                Err(err) => CheckDataRoleReply::failed(format!("bad request: {err}")),
            };
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!("check-data-role reply failed: {err}");
                }
            }
        });
    }
}

// --- Login roles (core NATS request/reply) -------------------------------------

/// Hard cap on one role request, below the orchestrator's request timeout (15 s).
const ROLE_DEADLINE: Duration = Duration::from_secs(10);

/// Serves one of the role subjects (create, reset, delete) in the reset queue group; a request
/// that doesn't decode answers `failed`.
async fn serve_role_rpc(
    worker: Arc<RealWorker>,
    client: async_nats::Client,
    subject: &'static str,
) {
    let mut requests = match client
        .queue_subscribe(subject, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!(subject, "role subscribe failed: {err}");
            return;
        }
    };
    info!(subject, "serving role requests");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply: RoleReply = worker
                .role_rpc_reply(subject, &message.payload, ROLE_DEADLINE)
                .await;
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!(subject, "role reply failed: {err}");
                }
            }
        });
    }
}
