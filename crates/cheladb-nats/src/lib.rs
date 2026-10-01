//! NATS JetStream subjects, envelopes, and helpers for Cheladb control plane.

use cheladb_types::{BranchId, DatabaseId, ProjectId, TenantId};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

/// Subject for database provisioning jobs.
pub const SUBJECT_PROVISION: &str = "cheladb.provision";

/// Subject for reconciliation jobs (reserved).
pub const SUBJECT_RECONCILE: &str = "cheladb.reconcile";

/// JetStream stream name for control-plane jobs.
pub const STREAM_CONTROL: &str = "CHELADB_CONTROL";

/// Durable consumer name for the provision worker.
pub const CONSUMER_PROVISION: &str = "cheladb-worker-provision";

/// How long a failed job waits before JetStream redelivers it.
pub const REDELIVERY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

/// Durable delete jobs (container + volume teardown).
pub const SUBJECT_DELETE: &str = "cheladb.delete";
pub const CONSUMER_DELETE: &str = "cheladb-worker-delete";

/// How long a failed branch job waits before JetStream redelivers it: longer than other jobs,
/// so that its few transient retries (the worker's `BRANCH_JOB_ATTEMPTS`) span a
/// storage-controller restart, about 2 minutes.
pub const BRANCH_REDELIVERY_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// Durable branch jobs: create, reset to parent, restore and delete.
pub const SUBJECT_BRANCH: &str = "cheladb.branch";
pub const CONSUMER_BRANCH: &str = "cheladb-worker-branch";

/// Durable database settings jobs: push the history window to the tenant.
pub const SUBJECT_DATABASE_SETTINGS: &str = "cheladb.database_settings";
pub const CONSUMER_DATABASE_SETTINGS: &str = "cheladb-worker-settings";

/// Core request/reply (not JetStream): reset a database's owner password.
pub const SUBJECT_RESET_PASSWORD: &str = "cheladb.rpc.reset_password";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEnvelope<T> {
    pub job_id: Uuid,
    pub payload: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionDatabaseJob {
    pub database_id: DatabaseId,
    pub project_id: ProjectId,
    pub tenant_id: TenantId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteDatabaseJob {
    pub database_id: DatabaseId,
}

/// A point on a branch's history: what the user asked for, before the worker resolves it to
/// an LSN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointInTime {
    Timestamp(DateTime<Utc>),
    Lsn(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchJob {
    /// The branch row exists (pending); build its timeline and compute. `at` is the point on
    /// the parent, `None` for now.
    Create {
        branch_id: BranchId,
        at: Option<PointInTime>,
    },
    /// The branch is `resetting`.
    ResetToParent { branch_id: BranchId },
    /// The branch is `resetting`; the old timeline becomes the backup `backup_id` named
    /// `backup_name`.
    Restore {
        branch_id: BranchId,
        backup_id: BranchId,
        backup_name: String,
        at: PointInTime,
    },
    /// The branch (and its backups) are `deleting`.
    Delete { branch_id: BranchId },
}

/// Push the database's settings (the history window) to its Neon tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseSettingsJob {
    pub database_id: DatabaseId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ResetPasswordRequestWire")]
pub struct ResetPasswordRequest {
    pub database_id: DatabaseId,
    /// Main's id (the database's) for the old routes; a request without one means main.
    pub branch_id: BranchId,
}

/// What a request may look like on the wire: `branch_id` is absent in step 4's requests.
#[derive(Deserialize)]
struct ResetPasswordRequestWire {
    database_id: DatabaseId,
    #[serde(default)]
    branch_id: Option<BranchId>,
}

impl From<ResetPasswordRequestWire> for ResetPasswordRequest {
    fn from(wire: ResetPasswordRequestWire) -> Self {
        Self {
            database_id: wire.database_id,
            branch_id: wire.branch_id.unwrap_or(BranchId::from(wire.database_id)),
        }
    }
}

/// The worker's reply error when the new password is stored but the compute doesn't have it yet.
pub const RESET_NOT_APPLIED: &str = "the password was changed but not yet applied; it will be retried";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetPasswordReply {
    pub ok: bool,
    pub error: Option<String>,
}

/// Core request/reply (not JetStream): may a secret key use this role on this branch?
pub const SUBJECT_CHECK_DATA_ROLE: &str = "cheladb.rpc.check_data_role";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckDataRoleRequest {
    pub branch_id: BranchId,
    pub role: String,
}

/// The worker's answer. `kind` says why when `ok` is false: `refused` (the caller's fault;
/// `error` says why), `not_ready` (the branch isn't ready or its compute can't be reached) or
/// `failed` (our side; `error` is for logs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDataRoleReply {
    pub ok: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

impl CheckDataRoleReply {
    pub fn ok() -> Self {
        Self { ok: true, error: None, kind: None }
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(message.into()), kind: Some("refused".into()) }
    }

    pub fn not_ready() -> Self {
        Self { ok: false, error: Some("not ready".into()), kind: Some("not_ready".into()) }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(message.into()), kind: Some("failed".into()) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileDatabaseJob {
    pub database_id: DatabaseId,
}

/// NATS connection settings.
#[derive(Debug, Clone)]
pub struct NatsConfig {
    pub url: String,
}

impl NatsConfig {
    pub fn from_env() -> Result<Self, String> {
        let url = std::env::var("CHELADB_NATS_URL")
            .unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
        Ok(Self { url })
    }
}

#[derive(Debug, Error)]
pub enum NatsError {
    #[error("nats connect error: {0}")]
    Connect(#[from] async_nats::ConnectError),
    #[error("nats error: {0}")]
    Client(#[from] async_nats::Error),
    #[error("jetstream error: {0}")]
    JetStream(String),
    #[error("serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub use async_nats::jetstream::Context as JetStreamContext;

pub async fn connect(config: &NatsConfig) -> Result<async_nats::Client, NatsError> {
    Ok(async_nats::connect(&config.url).await?)
}

/// Every subject the control stream must carry.
pub fn control_subjects() -> Vec<String> {
    vec![
        SUBJECT_PROVISION.to_string(),
        SUBJECT_RECONCILE.to_string(),
        SUBJECT_DELETE.to_string(),
        SUBJECT_BRANCH.to_string(),
        SUBJECT_DATABASE_SETTINGS.to_string(),
    ]
}

/// Control subjects an existing stream doesn't carry yet.
pub fn missing_subjects(existing: &[String]) -> Vec<String> {
    control_subjects()
        .into_iter()
        .filter(|s| !existing.contains(s))
        .collect()
}

/// Create the control stream, or add subjects an older stream is missing.
pub async fn ensure_control_stream(
    client: &async_nats::Client,
) -> Result<async_nats::jetstream::Context, NatsError> {
    let js = async_nats::jetstream::new(client.clone());
    match js.get_stream(STREAM_CONTROL).await {
        Ok(stream) => {
            let mut config = stream.cached_info().config.clone();
            let missing = missing_subjects(&config.subjects);
            if missing.is_empty() {
                info!(stream = STREAM_CONTROL, "jetstream stream exists");
            } else {
                config.subjects.extend(missing.iter().cloned());
                js.update_stream(config)
                    .await
                    .map_err(|e| NatsError::JetStream(e.to_string()))?;
                info!(stream = STREAM_CONTROL, ?missing, "jetstream stream subjects added");
            }
        }
        Err(_) => {
            js.create_stream(async_nats::jetstream::stream::Config {
                name: STREAM_CONTROL.to_string(),
                subjects: control_subjects(),
                retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
                ..Default::default()
            })
            .await
            .map_err(|e| NatsError::JetStream(e.to_string()))?;
            info!(stream = STREAM_CONTROL, "jetstream stream created");
        }
    }
    Ok(js)
}

pub async fn publish_provision(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<ProvisionDatabaseJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_PROVISION.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_delete(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<DeleteDatabaseJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_DELETE.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_branch(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<BranchJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_BRANCH.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_database_settings(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<DatabaseSettingsJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_DATABASE_SETTINGS.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

/// Run the branch consumer loop, calling `handler` for each job with its delivery attempt
/// (1 for the first delivery), so a handler can bound its own retries.
pub async fn run_branch_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<BranchJob>, u64) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    consume(js, ConsumerSpec::branch(), handler).await
}

/// Run the settings consumer loop, calling `handler` for each job.
pub async fn run_settings_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<DatabaseSettingsJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_DATABASE_SETTINGS, SUBJECT_DATABASE_SETTINGS, handler).await
}

/// Run the delete consumer loop, calling `handler` for each job.
pub async fn run_delete_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<DeleteDatabaseJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_DELETE, SUBJECT_DELETE, handler).await
}

/// Run the provision consumer loop, calling `handler` for each job.
pub async fn run_provision_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<ProvisionDatabaseJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_PROVISION, SUBJECT_PROVISION, handler).await
}

/// How long a job may run before NATS redelivers it. The worst case is a provision that fails
/// late and tears down: two storage-controller calls (10 s each), the 60 s readiness wait, up
/// to 30 s of admin SQL, then the teardown (a tenant delete waits up to 60 s, plus 10 s per
/// request), about 180 s. A redelivery in the middle would run a second copy of the job.
///
/// Limits:
/// - Docker calls (bollard) have no timeout of their own, so a hung Docker can outlast it.
/// - The consumer prefetches messages and handles them one at a time. Each one's timer starts
///   when it's delivered, so a job queued behind slow ones can be redelivered before it runs.
/// - A redelivered provision whose first copy failed provisions again: `failed` rows aren't
///   skipped, so the database can still become ready.
const ACK_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// Where and how a consumer pulls: its stream, durable name and subject, how long a delivery
/// may run before it's redelivered, and how long a failed job waits before redelivery.
struct ConsumerSpec<'a> {
    stream: &'a str,
    durable: &'a str,
    subject: &'a str,
    ack_wait: std::time::Duration,
    nak_delay: std::time::Duration,
}

impl<'a> ConsumerSpec<'a> {
    fn control(durable: &'a str, subject: &'a str) -> Self {
        Self {
            stream: STREAM_CONTROL,
            durable,
            subject,
            ack_wait: ACK_WAIT,
            nak_delay: REDELIVERY_DELAY,
        }
    }

    /// The branch consumer: a control consumer that waits [`BRANCH_REDELIVERY_DELAY`] before
    /// a failed job's redelivery.
    fn branch() -> ConsumerSpec<'static> {
        ConsumerSpec {
            nak_delay: BRANCH_REDELIVERY_DELAY,
            ..ConsumerSpec::control(CONSUMER_BRANCH, SUBJECT_BRANCH)
        }
    }
}

/// Pull `subject` with durable consumer `durable`, acking handled jobs and
/// nak-ing failures for redelivery. Unparseable payloads are acked (dropped).
async fn run_consumer<T, F, Fut>(
    js: async_nats::jetstream::Context,
    durable: &str,
    subject: &str,
    mut handler: F,
) -> Result<(), NatsError>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(JobEnvelope<T>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    consume(js, ConsumerSpec::control(durable, subject), move |job, _| {
        handler(job)
    })
    .await
}

/// The consumer loop: each job goes to `handler` with its delivery attempt (1-based, from
/// JetStream's count, so it survives a worker restart).
async fn consume<T, F, Fut>(
    js: async_nats::jetstream::Context,
    spec: ConsumerSpec<'_>,
    mut handler: F,
) -> Result<(), NatsError>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(JobEnvelope<T>, u64) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let durable = spec.durable;
    let stream = js
        .get_stream(spec.stream)
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    let consumer = stream
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            durable_name: Some(durable.to_string()),
            filter_subject: spec.subject.to_string(),
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            ack_wait: spec.ack_wait,
            ..Default::default()
        })
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    info!(consumer = durable, "consumer ready");

    let mut messages = consumer
        .messages()
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    while let Some(message) = messages.next().await {
        let message = match message {
            Ok(m) => m,
            Err(err) => {
                warn!("consumer message error: {err}");
                continue;
            }
        };

        let job: JobEnvelope<T> = match serde_json::from_slice(&message.payload)
        {
            Ok(j) => j,
            Err(err) => {
                warn!(consumer = durable, "invalid job payload: {err}");
                let _ = message.ack().await;
                continue;
            }
        };
        let attempt = message
            .info()
            .map(|info| u64::try_from(info.delivered).unwrap_or(1).max(1))
            .unwrap_or(1);

        // While the handler runs, tell JetStream it's still in progress (every third of the
        // ack wait), so a long job (a detach can take minutes) isn't delivered to a second
        // copy of the handler meanwhile.
        let result = {
            let run = handler(job, attempt);
            tokio::pin!(run);
            let mut beat = tokio::time::interval(spec.ack_wait / 3);
            beat.tick().await;
            loop {
                tokio::select! {
                    result = &mut run => break result,
                    _ = beat.tick() => {
                        if let Err(err) = message.ack_with(async_nats::jetstream::AckKind::Progress).await {
                            warn!(consumer = durable, "in-progress ack failed: {err}");
                        }
                    }
                }
            }
        };
        match result {
            Ok(()) => {
                if let Err(err) = message.ack().await {
                    warn!("ack failed: {err}");
                }
            }
            Err(err) => {
                warn!(consumer = durable, attempt, "job handler failed: {err}");
                // Back off before redelivery: a failing dependency (e.g. Docker
                // down) would otherwise spin through redeliveries.
                if let Err(nak_err) = message
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(spec.nak_delay)))
                    .await
                {
                    warn!("nak failed: {nak_err}");
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_data_role_wire_shapes() {
        let request = CheckDataRoleRequest {
            branch_id: BranchId::new(),
            role: "reporting".into(),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["role"], "reporting");
        assert!(json["branch_id"].is_string());
        assert_eq!(SUBJECT_CHECK_DATA_ROLE, "cheladb.rpc.check_data_role");

        let ok = serde_json::to_value(CheckDataRoleReply::ok()).unwrap();
        assert_eq!(ok, serde_json::json!({ "ok": true, "error": null, "kind": null }));
        let refused = CheckDataRoleReply::refused("the role x does not exist");
        assert_eq!(
            serde_json::to_value(&refused).unwrap(),
            serde_json::json!({ "ok": false, "error": "the role x does not exist", "kind": "refused" })
        );
        assert_eq!(CheckDataRoleReply::not_ready().kind.as_deref(), Some("not_ready"));
        assert_eq!(CheckDataRoleReply::failed("boom").kind.as_deref(), Some("failed"));
        // A reply without `kind` (the two-field shape) still parses.
        let old: CheckDataRoleReply = serde_json::from_str(r#"{"ok":false,"error":"x"}"#).unwrap();
        assert_eq!(old.kind, None);
    }

    /// Opt-in, against the Compose NATS (`just deps-up`): a stream and consumer of their own,
    /// deleted afterwards.
    mod live {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use super::*;

        async fn js() -> async_nats::jetstream::Context {
            let client = connect(&NatsConfig::from_env().unwrap())
                .await
                .expect("NATS reachable (just deps-up)");
            async_nats::jetstream::new(client)
        }

        /// A work-queue stream `CHELADB_TEST_<rand>` on subject `cheladb.test.<rand>`.
        async fn test_stream(js: &async_nats::jetstream::Context) -> (String, String) {
            let tag = Uuid::new_v4().simple().to_string();
            let (stream, subject) = (format!("CHELADB_TEST_{tag}"), format!("cheladb.test.{tag}"));
            js.create_stream(async_nats::jetstream::stream::Config {
                name: stream.clone(),
                subjects: vec![subject.clone()],
                retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
                ..Default::default()
            })
            .await
            .unwrap();
            (stream, subject)
        }

        async fn publish_one(js: &async_nats::jetstream::Context, subject: &str) {
            let payload = DatabaseSettingsJob { database_id: DatabaseId::new() };
            let job = JobEnvelope { job_id: Uuid::new_v4(), payload };
            js.publish(subject.to_string(), serde_json::to_vec(&job).unwrap().into())
                .await
                .unwrap()
                .await
                .unwrap();
        }

        /// Runs a consumer on a fresh stream for `run_for`, with `handler` answering each
        /// delivery (by attempt), and returns the attempts it saw.
        async fn attempts_seen(
            ack_wait: Duration,
            run_for: Duration,
            handler: impl Fn(u64) -> Result<(), String> + Send + Sync + 'static,
            slow: Duration,
        ) -> Vec<u64> {
            let js = js().await;
            let (stream, subject) = test_stream(&js).await;
            publish_one(&js, &subject).await;
            let seen = Arc::new(Mutex::new(Vec::new()));
            let handler = Arc::new(handler);
            let task = {
                let (js, stream, subject, seen) =
                    (js.clone(), stream.clone(), subject.clone(), seen.clone());
                tokio::spawn(async move {
                    let spec = ConsumerSpec {
                        stream: &stream,
                        durable: "test",
                        subject: &subject,
                        ack_wait,
                        nak_delay: Duration::from_millis(100),
                    };
                    consume(js, spec, move |_job: JobEnvelope<DatabaseSettingsJob>, attempt| {
                        let (seen, handler) = (seen.clone(), handler.clone());
                        async move {
                            seen.lock().unwrap().push(attempt);
                            tokio::time::sleep(slow).await;
                            handler(attempt)
                        }
                    })
                    .await
                })
            };
            tokio::time::sleep(run_for).await;
            task.abort();
            js.delete_stream(&stream).await.unwrap();
            seen.lock().unwrap().clone()
        }

        #[tokio::test]
        #[ignore]
        async fn each_redelivery_counts_its_attempt() {
            let seen = attempts_seen(
                Duration::from_secs(30),
                Duration::from_secs(3),
                |attempt| if attempt < 3 { Err("not yet".into()) } else { Ok(()) },
                Duration::ZERO,
            )
            .await;
            assert_eq!(seen, vec![1, 2, 3]);
        }

        #[tokio::test]
        #[ignore]
        async fn a_job_running_past_its_ack_wait_is_not_delivered_again() {
            let seen = attempts_seen(
                Duration::from_secs(1),
                Duration::from_secs(6),
                |_| Ok(()),
                Duration::from_secs(3),
            )
            .await;
            assert_eq!(seen, vec![1], "the running job keeps its delivery");
        }
    }

    #[test]
    fn branch_jobs_back_off_longer_than_other_jobs() {
        let spec = ConsumerSpec::branch();
        assert_eq!((spec.durable, spec.subject), (CONSUMER_BRANCH, SUBJECT_BRANCH));
        assert_eq!(spec.nak_delay, BRANCH_REDELIVERY_DELAY);
        assert!(BRANCH_REDELIVERY_DELAY > REDELIVERY_DELAY);
        assert_eq!(spec.ack_wait, ACK_WAIT);
    }

    #[test]
    fn stream_subjects_include_delete() {
        assert!(control_subjects().contains(&SUBJECT_DELETE.to_string()));
        assert_eq!(
            missing_subjects(&["cheladb.provision".into()]),
            vec![
                SUBJECT_RECONCILE.to_string(),
                SUBJECT_DELETE.to_string(),
                SUBJECT_BRANCH.to_string(),
                SUBJECT_DATABASE_SETTINGS.to_string(),
            ]
        );
        assert!(missing_subjects(&control_subjects()).is_empty());
    }

    #[test]
    fn stream_carries_the_branch_and_settings_subjects() {
        assert_eq!(SUBJECT_BRANCH, "cheladb.branch");
        assert_eq!(CONSUMER_BRANCH, "cheladb-worker-branch");
        assert_eq!(SUBJECT_DATABASE_SETTINGS, "cheladb.database_settings");
        let subjects = control_subjects();
        assert!(subjects.contains(&SUBJECT_BRANCH.to_string()));
        assert!(subjects.contains(&SUBJECT_DATABASE_SETTINGS.to_string()));
        // An older stream gets exactly the new ones.
        let old = vec![
            SUBJECT_PROVISION.to_string(),
            SUBJECT_RECONCILE.to_string(),
            SUBJECT_DELETE.to_string(),
        ];
        assert_eq!(
            missing_subjects(&old),
            vec![SUBJECT_BRANCH.to_string(), SUBJECT_DATABASE_SETTINGS.to_string()]
        );
    }

    #[test]
    fn point_in_time_is_snake_case_tagged() {
        let at = PointInTime::Lsn("0/16B3748".into());
        assert_eq!(serde_json::to_value(&at).unwrap(), serde_json::json!({ "lsn": "0/16B3748" }));
        let time: DateTime<Utc> = "2026-09-30T12:00:00Z".parse().unwrap();
        assert_eq!(
            serde_json::to_value(PointInTime::Timestamp(time)).unwrap(),
            serde_json::json!({ "timestamp": "2026-09-30T12:00:00Z" })
        );
    }

    #[test]
    fn branch_jobs_round_trip() {
        let branch_id = BranchId::new();
        let jobs = vec![
            BranchJob::Create { branch_id, at: None },
            BranchJob::Create { branch_id, at: Some(PointInTime::Lsn("0/1".into())) },
            BranchJob::ResetToParent { branch_id },
            BranchJob::Restore {
                branch_id,
                backup_id: BranchId::new(),
                backup_name: "dev-before-restore-20260930T120000Z".into(),
                at: PointInTime::Timestamp(Utc::now()),
            },
            BranchJob::Delete { branch_id },
        ];
        for job in jobs {
            let envelope = JobEnvelope { job_id: Uuid::new_v4(), payload: job.clone() };
            let back: JobEnvelope<BranchJob> =
                serde_json::from_slice(&serde_json::to_vec(&envelope).unwrap()).unwrap();
            assert_eq!(back.payload, job);
        }
    }

    #[test]
    fn reset_request_carries_the_branch() {
        let (database_id, branch_id) = (DatabaseId::new(), BranchId::new());
        let json = serde_json::to_value(ResetPasswordRequest { database_id, branch_id }).unwrap();
        assert_eq!(json["branch_id"], serde_json::json!(branch_id));
        assert_eq!(json["database_id"], serde_json::json!(database_id));
    }

    #[test]
    fn reset_request_without_a_branch_means_main() {
        let database_id = DatabaseId::new();
        let back: ResetPasswordRequest =
            serde_json::from_value(serde_json::json!({ "database_id": database_id })).unwrap();
        assert_eq!(back.database_id, database_id);
        assert_eq!(back.branch_id, BranchId::from(database_id));
    }
}
