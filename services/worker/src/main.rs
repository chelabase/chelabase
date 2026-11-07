use axum::{Json, Router, routing::get};
use chelabase_metadata::MetadataClient;
use chelabase_nats::{
    AppUsersCheckReply, AppUsersCheckRequest, CheckDataRoleReply, CheckDataRoleRequest,
    DataApiDatabaseRequest, JobEnvelope, MemberRolesReconcileRequest,
    MemberRolesReply, NatsConfig, SUBJECT_MEMBER_ROLES_ENQUEUE,
    SUBJECT_MEMBER_ROLES_HAND_OVER, SUBJECT_MEMBER_ROLES_RECONCILE, SUBJECT_REVIEW, ReviewReply,
    RESET_NOT_APPLIED, ResetPasswordReply, ResetPasswordRequest, ResyncBranchRequest, RoleReply,
    RoleRequest, RoleSessionsReply, SUBJECT_ROLE_SESSIONS,
    SUBJECT_APP_USERS_CHECK, SUBJECT_CHECK_DATA_ROLE, SUBJECT_DATA_API_DISABLE,
    SUBJECT_DATA_API_ENABLE, SUBJECT_DATA_API_REBOOTSTRAP,
    SUBJECT_RESET_PASSWORD, SUBJECT_RESYNC_BRANCH, SUBJECT_ROLE_CREATE, SUBJECT_ROLE_DELETE,
    SUBJECT_ROLE_DELETE_PREVIEW, SUBJECT_ROLE_RESET, SUBJECT_ROLE_UPDATE, connect, ensure_control_stream, publish_merge,
    run_app_users_consumer, run_app_users_sync_consumer, run_branch_consumer,
    run_delete_consumer, run_merge_consumer, run_project_delete_consumer,
    run_provision_consumer, run_settings_consumer, run_storage_sweep_consumer,
};
use chelabase_secrets::{SecretKey, key_check_value, verify_key_check};
use futures_util::StreamExt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use worker::branch_job::Attempt;
use worker::config::{RealWorker, WorkerConfig, build};
use chelabase_types::BranchId;
use worker::member_roles::{
    MEMBER_ROLES_DEADLINE, MEMBER_ROLES_ENQUEUE_DELAY, member_hand_over_reply, member_roles_reply,
};
use worker::resync::{PendingResyncs, TOUCHED_RESYNC_DELAY};
use worker::review::{REVIEW_DEADLINE, review_reply};
use worker::all_rows::{ALL_ROWS_CHECK_DEADLINE, all_rows_check_reply};
use chelabase_nats::{AllRowsCheckReply, SUBJECT_ALL_ROWS_CHECK};

const RECONCILE_EVERY: Duration = Duration::from_secs(30);
/// Hard cap on one reset, below the orchestrator's NATS request timeout (`RESET_TIMEOUT`,
/// 15 s in the orchestrator): keep at least 5 s between them.
const RESET_DEADLINE: Duration = Duration::from_secs(8);
/// Queue group: with several workers, each reset is handled exactly once.
const RESET_QUEUE_GROUP: &str = "chelabase-worker";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    chelabase_types::cli::handle("worker", "The Chelabase worker: background jobs for provisioning, branches, merges and roles.", env!("CARGO_PKG_VERSION"));
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| worker::DEFAULT_LOG_FILTER.into()),
        )
        .init();

    // Before anything is contacted, so a missing image is the first thing a user sees.
    let config = WorkerConfig::from_env()?;
    let metadata_config = worker::config::metadata_config(|name| std::env::var(name).ok())?;
    let nats_config = NatsConfig::from_env().expect("nats config");
    // object-storage m18: where storage webhooks may go (public addresses only, unless allowed).
    let webhooks = chelabase_http::outbound::Outbound::from_lookup(|name| std::env::var(name).ok())
        .map_err(|e| anyhow::anyhow!(e))?;
    for entry in webhooks.allow_hosts_with_port() {
        warn!(entry, "CHELABASE_WEBHOOKS_ALLOW_HOSTS: an entry with a port never matches; list the host alone");
    }

    let metadata = MetadataClient::connect(&metadata_config).await?;
    metadata.migrate().await?;

    let nats = connect(&nats_config).await?;
    let js = ensure_control_stream(&nats).await?;

    let key = SecretKey::from_env()?;
    ensure_secrets_key(&metadata, &key).await?;

    let publisher = Arc::new(worker::rt_events::NatsPublisher::new(nats.clone()));
    // Provisions wait for the orchestrator's tenant-attached events (carried gaps T4), read
    // with the worker's own NATS rights (filtered consumers only; no stream lookups).
    let attach = worker::ports::AttachSource::Nats(js.clone());
    let mut built = build(&config, metadata, key, publisher, attach)?;
    built.storage_sweep = worker::storage_sweep::SweepSettings::from_lookup(|name| std::env::var(name).ok())
        .map_err(|e| anyhow::anyhow!(e))?;
    // object-storage m8: the sweep needs the user-blob store; without one it doesn't run.
    if std::env::var(chelabase_storage::config::ENV_BACKEND).is_ok_and(|v| !v.trim().is_empty()) {
        let store_config = chelabase_storage::StorageConfig::from_env()?;
        built.blob_store = Some(Arc::new(chelabase_storage::S3Store::new(&store_config).await));
    } else {
        warn!("CHELABASE_STORAGE_BACKEND is unset: the storage sweep doesn't run");
    }
    let worker = Arc::new(built);
    // Only with a store: without one, no consumer would take the jobs.
    if worker.blob_store.is_some() {
        worker.sweep_requests.connect(js.clone());
    }
    worker.resync_requests.connect(nats.clone());
    worker.member_role_requests.connect(nats.clone());
    // E33a: the secret keys' BYPASSRLS marks, right from the start.
    tokio::spawn({
        let worker = worker.clone();
        async move { worker.sync_key_marks_at_start().await }
    });
    if config.owner_superuser_requested && !config.policy.owner_superuser_allowed() {
        warn!("CHELABASE_DB_OWNER_SUPERUSER is ignored in platform mode");
    }
    if worker.owner_superuser {
        warn!("CHELABASE_DB_OWNER_SUPERUSER is on: new databases' owner role is SUPERUSER");
    }

    let health_addr: SocketAddr = std::env::var("CHELABASE_WORKER_HEALTH_ADDR")
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
    tokio::spawn(serve_enable_data_api(worker.clone(), nats.clone()));
    for subject in [SUBJECT_DATA_API_DISABLE, SUBJECT_DATA_API_REBOOTSTRAP] {
        tokio::spawn(serve_data_api_rpc(worker.clone(), nats.clone(), subject));
    }
    for subject in [
        SUBJECT_ROLE_CREATE,
        SUBJECT_ROLE_RESET,
        SUBJECT_ROLE_DELETE,
        SUBJECT_ROLE_DELETE_PREVIEW,
        SUBJECT_ROLE_UPDATE,
    ] {
        tokio::spawn(serve_role_rpc(worker.clone(), nats.clone(), subject));
    }
    tokio::spawn(serve_role_sessions(worker.clone(), nats.clone()));
    tokio::spawn(serve_resync_branch(worker.clone(), nats.clone()));
    tokio::spawn(serve_member_roles_reconcile(worker.clone(), nats.clone()));
    tokio::spawn(serve_member_roles_hand_over(worker.clone(), nats.clone()));
    tokio::spawn(serve_review(worker.clone(), nats.clone()));
    tokio::spawn(serve_all_rows_check(worker.clone(), nats.clone()));
    tokio::spawn(serve_member_roles_enqueue(worker.clone(), nats.clone()));
    tokio::spawn(member_roles_sweep_loop(worker.clone()));
    tokio::spawn(cert_sweep_loop(worker.clone()));
    tokio::spawn(worker::token_refresh::storage_token_refresh_loop(worker.clone()));
    tokio::spawn(reconcile_loop(worker.clone(), js.clone()));
    tokio::spawn(serve_app_users_check(worker.clone(), nats.clone()));
    tokio::spawn(app_users_reconcile_loop(worker.clone()));
    if worker.identity.is_none() {
        warn!("the identity service isn't configured: app users and project delete jobs wait (CHELABASE_IDENTITY_URL, CHELABASE_IDENTITY_TOKEN_FILE)");
    }

    let project_delete_worker = worker.clone();
    let project_delete_js = js.clone();
    tokio::spawn(async move {
        let result = run_project_delete_consumer(project_delete_js, move |job| {
            let worker = project_delete_worker.clone();
            async move { worker.handle_project_delete(job.payload.project_id).await }
        })
        .await;
        if let Err(err) = result {
            warn!("project delete consumer exited: {err}");
        }
    });

    // object-storage m18: storage webhooks, one consumer per enabled endpoint, and their test
    // events on request.
    tokio::spawn(worker::storage_webhooks::run(
        worker.metadata.inner().clone(),
        worker.key.clone(),
        js.clone(),
        webhooks.clone(),
    ));
    tokio::spawn(serve_storage_webhook_test(worker.clone(), nats.clone(), webhooks));

    if worker.blob_store.is_some() {
        tokio::spawn(storage_sweep_loop(worker.clone()));
        let sweep_worker = worker.clone();
        let sweep_js = js.clone();
        tokio::spawn(async move {
            let result = run_storage_sweep_consumer(sweep_js, move |job, attempt| {
                let worker = sweep_worker.clone();
                async move {
                    worker
                        .handle_storage_sweep_attempt(job.payload.database_id, attempt)
                        .await
                }
            })
            .await;
            if let Err(err) = result {
                warn!("storage sweep consumer exited: {err}");
            }
        });
    }

    let app_users_worker = worker.clone();
    let app_users_js = js.clone();
    tokio::spawn(async move {
        let result = run_app_users_consumer(app_users_js, move |job| {
            let worker = app_users_worker.clone();
            async move { worker.handle_app_users_job(&job.payload).await }
        })
        .await;
        if let Err(err) = result {
            warn!("app users consumer exited: {err}");
        }
    });

    let sync_worker = worker.clone();
    let sync_js = js.clone();
    tokio::spawn(async move {
        let result = run_app_users_sync_consumer(sync_js, move |job| {
            let worker = sync_worker.clone();
            async move { worker.handle_app_users_sync(&job.payload).await }
        })
        .await;
        if let Err(err) = result {
            warn!("app users sync consumer exited: {err}");
        }
    });

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

    // P11: one merge job at a time, like the branch consumer.
    let merge_worker = worker.clone();
    let merge_js = js.clone();
    tokio::spawn(async move {
        let result = run_merge_consumer(merge_js, move |job, attempt| {
            let worker = merge_worker.clone();
            async move { worker.handle_merge_job(job.payload, Attempt(attempt)).await }
        })
        .await;
        if let Err(err) = result {
            warn!("merge consumer exited: {err}");
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

    info!("worker consuming provision, delete, branch, merge, settings, project delete and app users jobs");
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
    use chelabase_metadata::KeyCheck;
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
            "CHELABASE_SECRETS_KEY is not the key {sealed_rows} stored credential(s) were \
             encrypted with. Restore the original .chelabase/secrets/secrets.key; in local dev \
             you can instead remove every database with `just sweep-dbs`."
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

async fn reconcile_loop(worker: Arc<RealWorker>, js: async_nats::jetstream::Context) {
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
        // R3: merges whose worker went silent go back to the merge consumer (P11: one job at
        // a time there), as `Resume`; lost after-merge choices as `Finish` (T14 M8). A hand-back
        // is recorded only once published (T14 F3 M3).
        worker
            .hand_back_merges(|payload| {
                let js = js.clone();
                async move {
                    let job = JobEnvelope {
                        job_id: uuid::Uuid::new_v4(),
                        payload,
                    };
                    publish_merge(&js, &job).await.map_err(|e| e.to_string())
                }
            })
            .await;
    }
}

/// Every enabled project's users reconciled, old sessions purged, and stale app users jobs
/// and project deletes run again.
async fn app_users_reconcile_loop(worker: Arc<RealWorker>) {
    let every = worker.app_users_reconcile_every;
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        worker.app_users_reconcile_once().await;
    }
}

// --- App users location check (core NATS request/reply) -------------------------

/// Hard cap on one check, below the orchestrator's request timeout.
const APP_USERS_CHECK_DEADLINE: Duration = Duration::from_secs(25);

async fn serve_app_users_check(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_APP_USERS_CHECK, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("app-users-check subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_APP_USERS_CHECK, "serving app users location checks");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let unavailable = |error: String| AppUsersCheckReply {
                ok: false,
                code: Some("unavailable".to_string()),
                error: Some(error),
            };
            let reply = match serde_json::from_slice::<AppUsersCheckRequest>(&message.payload) {
                Ok(request) => match tokio::time::timeout(
                    APP_USERS_CHECK_DEADLINE,
                    worker.check_app_users_location(&request),
                )
                .await
                {
                    Ok(reply) => reply,
                    Err(_) => {
                        warn!(database_id = %request.database_id, "app users location check timed out");
                        unavailable("timed out".to_string())
                    }
                },
                Err(err) => unavailable(format!("bad request: {err}")),
            };
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!("app-users-check reply failed: {err}");
                }
            }
        });
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

// --- The data API per database (core NATS) ---------------------------------------

/// Serves the enable publish (no reply): the orchestrator already set the row `enabling`.
async fn serve_enable_data_api(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_DATA_API_ENABLE, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("enable-data-api subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_DATA_API_ENABLE, "serving enable-data-api requests");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        tokio::spawn(async move {
            match serde_json::from_slice::<DataApiDatabaseRequest>(&message.payload) {
                Ok(request) => {
                    if tokio::time::timeout(worker::data_api::ENABLE_DEADLINE, worker.enable_data_api(&request))
                        .await
                        .is_err()
                    {
                        warn!(branch_id = %request.branch_id, database = %request.name, "enable data API timed out; reconcile retries");
                    }
                }
                Err(err) => warn!("bad enable-data-api request: {err}"),
            }
        });
    }
}

/// Hard cap on one disable or rebootstrap, below the orchestrator's request timeout (15 s).
const DATA_API_DEADLINE: Duration = Duration::from_secs(12);

/// Serves the disable and rebootstrap requests (request/reply with a [`CheckDataRoleReply`]).
async fn serve_data_api_rpc(
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
            warn!(subject, "data API subscribe failed: {err}");
            return;
        }
    };
    info!(subject, "serving data API requests");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply = match serde_json::from_slice::<DataApiDatabaseRequest>(&message.payload) {
                Ok(request) => {
                    let work = async {
                        if subject == SUBJECT_DATA_API_DISABLE {
                            worker.disable_data_api(&request).await
                        } else {
                            worker.rebootstrap_data_api(&request).await
                        }
                    };
                    match tokio::time::timeout(DATA_API_DEADLINE, work).await {
                        Ok(reply) => reply,
                        Err(_) => {
                            warn!(subject, branch_id = %request.branch_id, "data API request timed out");
                            CheckDataRoleReply::failed("timed out")
                        }
                    }
                }
                Err(err) => CheckDataRoleReply::failed(format!("bad request: {err}")),
            };
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!(subject, "data API reply failed: {err}");
                }
            }
        });
    }
}

// --- Touched roles (core NATS publish) -------------------------------------------

/// Serves the touched-roles publish (no reply): the DDL receiver saw a role's attributes or
/// grants change. Each branch gets one resync a few seconds after its first message; the
/// periodic reconcile stays as the backstop.
async fn serve_resync_branch(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_RESYNC_BRANCH, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("resync-branch subscribe failed: {err}");
            return;
        }
    };
    info!(
        subject = SUBJECT_RESYNC_BRANCH,
        "serving touched-roles resyncs"
    );
    let pending = Arc::new(PendingResyncs::default());
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let pending = pending.clone();
        tokio::spawn(async move {
            match serde_json::from_slice::<ResyncBranchRequest>(&message.payload) {
                Ok(request) => {
                    if let Err(err) = worker
                        .resync_touched(&pending, request.branch_id, TOUCHED_RESYNC_DELAY)
                        .await
                    {
                        warn!(branch_id = %request.branch_id, "touched-roles resync: {err}");
                    }
                }
                Err(err) => warn!("bad resync-branch request: {err}"),
            }
        });
    }
}

// --- Member roles (core NATS) ----------------------------------------------------

/// Serves the synchronous member-role reconcile (request/reply with a [`MemberRolesReply`]):
/// the act-as check before a member's connection (T9) and the orchestrator's calls.
async fn serve_member_roles_reconcile(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_MEMBER_ROLES_RECONCILE, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("member-roles subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_MEMBER_ROLES_RECONCILE, "serving member-role reconciles");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply: MemberRolesReply =
                member_roles_reply(worker, &message.payload, MEMBER_ROLES_DEADLINE).await;
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!("member-roles reply failed: {err}");
                }
            }
        });
    }
}

/// Serves the hand-over after an act-as run (request/reply with a [`MemberRolesReply`]).
async fn serve_member_roles_hand_over(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_MEMBER_ROLES_HAND_OVER, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("member-role hand-over subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_MEMBER_ROLES_HAND_OVER, "serving member-role hand-overs");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply: MemberRolesReply =
                member_hand_over_reply(worker, &message.payload, MEMBER_ROLES_DEADLINE).await;
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!("member-role hand-over reply failed: {err}");
                }
            }
        });
    }
}

/// Serves the protection review and its fixes (request/reply with a [`ReviewReply`]).
async fn serve_review(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_REVIEW, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("review subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_REVIEW, "serving protection reviews");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply: ReviewReply = review_reply(worker, &message.payload, REVIEW_DEADLINE).await;
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!("review reply failed: {err}");
                }
            }
        });
    }
}

/// object-storage m18: sends one test event to a storage webhook endpoint on the orchestrator's
/// request (`chelabase.rpc.storage_webhooks.test`) and answers how the attempt went.
async fn serve_storage_webhook_test(
    worker: Arc<RealWorker>,
    client: async_nats::Client,
    outbound: chelabase_http::outbound::Outbound,
) {
    use chelabase_nats::storage_objects::{SUBJECT_TEST, TestReply, TestRequest};
    let http = match outbound.client(worker::storage_webhooks::TIMEOUT) {
        Ok(http) => http,
        Err(err) => {
            warn!("storage webhook tests unavailable: {err}");
            return;
        }
    };
    let mut requests = match client.queue_subscribe(SUBJECT_TEST, RESET_QUEUE_GROUP.to_string()).await {
        Ok(sub) => sub,
        Err(err) => {
            warn!("storage webhook test subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_TEST, "serving storage webhook tests");
    while let Some(message) = requests.next().await {
        let (worker, client, http) = (worker.clone(), client.clone(), http.clone());
        tokio::spawn(async move {
            let reply = match serde_json::from_slice::<TestRequest>(&message.payload) {
                Ok(request) => {
                    worker::storage_webhooks::test_reply(worker.metadata.inner(), &worker.key, &http, &request).await
                }
                Err(err) => TestReply::Failed { message: format!("bad request: {err}") },
            };
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!("storage webhook test reply failed: {err}");
                }
            }
        });
    }
}

/// EOS-H1: serves the all-rows check (request/reply with an [`AllRowsCheckReply`]); a reply
/// that can't be sent leaves the orchestrator to time out and refuse.
async fn serve_all_rows_check(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_ALL_ROWS_CHECK, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("all-rows check subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_ALL_ROWS_CHECK, "serving all-rows checks");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply: AllRowsCheckReply =
                all_rows_check_reply(worker, &message.payload, ALL_ROWS_CHECK_DEADLINE).await;
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!("all-rows check reply failed: {err}");
                }
            }
        });
    }
}

/// RT3-7: at start and then daily, every running compute's server certificate and the
/// orchestrator's and realtime's client pairs are renewed when due.
async fn cert_sweep_loop(worker: Arc<RealWorker>) {
    let mut ticks = tokio::time::interval(worker::certs::CERT_SWEEP_EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let (renewed, failed) = worker.renew_certs_once(std::time::SystemTime::now()).await;
        if failed > 0 {
            warn!(renewed, failed, "certificate sweep: some renewals failed; the next sweep retries");
        }
    }
}

/// T8 I2: every `CHELABASE_MEMBER_ROLES_SWEEP_SECS`, every ready normal branch is reconciled
/// (members or not: the protected-branch guard and born-closed databases need it too): the
/// backstop for reconcile publishes that were lost (core NATS, no redelivery)
/// or whose retries all failed.
async fn member_roles_sweep_loop(worker: Arc<RealWorker>) {
    let every = worker.member_roles_sweep_every;
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let (done, failed) = worker.sweep_member_roles_once().await;
        if failed > 0 {
            warn!(done, failed, "member-role sweep: some branches failed; the next sweep retries");
        }
    }
}

/// How soon after start the first storage sweep is queued (audit N4): a worker restarted more
/// often than the period still sweeps.
const STORAGE_SWEEP_FIRST: Duration = Duration::from_secs(600);

/// object-storage m8: every `CHELABASE_STORAGE_SWEEP_SECS`, a sweep of every ready database is
/// queued (one job per database; the consumer runs them one at a time).
async fn storage_sweep_loop(worker: Arc<RealWorker>) {
    let every = worker.storage_sweep.every;
    let first = every.min(STORAGE_SWEEP_FIRST);
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + first, every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        if let Err(err) = worker.queue_storage_sweeps().await {
            warn!("queueing storage sweeps: {err}");
        }
    }
}

/// Serves the enqueued member-role reconciles (no reply): each branch gets one run a moment
/// after its first message, however many arrived, retried with backoff.
async fn serve_member_roles_enqueue(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_MEMBER_ROLES_ENQUEUE, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("member-roles enqueue subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_MEMBER_ROLES_ENQUEUE, "serving enqueued member-role reconciles");
    let pending = Arc::new(PendingResyncs::default());
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let pending = pending.clone();
        tokio::spawn(async move {
            match serde_json::from_slice::<MemberRolesReconcileRequest>(&message.payload) {
                Ok(request) => {
                    let branch = BranchId::from(request.branch_id);
                    if let MemberRolesReply::Failed { message } = worker
                        .reconcile_member_roles_soon(&pending, branch, MEMBER_ROLES_ENQUEUE_DELAY, request.from_ddl)
                        .await
                    {
                        warn!(branch_id = %branch, "member-role reconcile: {message}");
                    }
                }
                Err(err) => warn!("bad member-roles request: {err}"),
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

/// Serves the role-sessions read (E29a) in the reset queue group; a request that doesn't
/// decode, or outlasts [`ROLE_DEADLINE`], answers `failed`.
async fn serve_role_sessions(worker: Arc<RealWorker>, client: async_nats::Client) {
    let mut requests = match client
        .queue_subscribe(SUBJECT_ROLE_SESSIONS, RESET_QUEUE_GROUP.to_string())
        .await
    {
        Ok(sub) => sub,
        Err(err) => {
            warn!("role-sessions subscribe failed: {err}");
            return;
        }
    };
    info!(subject = SUBJECT_ROLE_SESSIONS, "serving role-sessions requests");
    while let Some(message) = requests.next().await {
        let worker = worker.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let reply = match serde_json::from_slice::<RoleRequest>(&message.payload) {
                Ok(request) => {
                    tokio::time::timeout(ROLE_DEADLINE, worker.role_sessions_reply(&request))
                        .await
                        .unwrap_or_else(|_| {
                            warn!(branch_id = %request.branch_id, "role sessions timed out");
                            RoleSessionsReply::failed("timed out")
                        })
                }
                Err(err) => RoleSessionsReply::failed(format!("bad request: {err}")),
            };
            if let Some(reply_to) = message.reply {
                let body = serde_json::to_vec(&reply).unwrap_or_default();
                if let Err(err) = client.publish(reply_to, body.into()).await {
                    warn!("role-sessions reply failed: {err}");
                }
            }
        });
    }
}
