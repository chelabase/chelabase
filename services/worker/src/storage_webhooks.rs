//! Storage webhooks (object-storage m18): delivers each storage object event to every matching
//! endpoint of its database, signed, at least once.
//!
//! Each enabled endpoint has its own durable consumer on the object events stream
//! (`chelabase_nats::storage_objects`), filtered to its database's subject and starting when
//! the endpoint was (re)enabled, so its position and its pending retries are its own. [`run`]
//! keeps one task per enabled endpoint, polling the metadata every [`POLL`]: a new endpoint
//! starts, a changed one (URL, filters, rotated secret) is picked up, a disabled or deleted
//! one stops and its consumer is deleted.
//!
//! Each message is one attempt ([`Delivery::process`]): the event is checked against the
//! endpoint (its own database, type, bucket and prefix), sent as a signed `POST`
//! ([`signature`]) through the public-only client (`chelabase_http::outbound`: no private
//! address, no redirect, 10 s), and recorded. A failure is NAKed with exponential backoff and
//! jitter ([`backoff`]); an event older than 24 h is dropped and counted instead; an endpoint
//! failing for 3 days running is disabled ([`decide`]).
//!
//! Signing: `Chelabase-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">`, keyed
//! with the endpoint's secret (its UTF-8 bytes, `whsec_…`). The body is the event as JSON; every
//! attempt of an event has the same `id` (also in `Chelabase-Event-Id`).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chelabase_http::outbound::{Outbound, PublicClient, refused_by_guard};
use chelabase_metadata::{
    DeliveryTarget, MetadataClient, MetadataError, StorageWebhook, StorageWebhookAttempt,
};
use chelabase_nats::storage_objects::{
    self, AttemptOutcome, ObjectActor, ObjectEvent, ObjectEventType, TestReply, TestRequest,
};
use chelabase_secrets::{SecretKey, open_webhook_secret};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tracing::{info, warn};
use uuid::Uuid;

pub const SIGNATURE_HEADER: &str = "chelabase-signature";
pub const EVENT_ID_HEADER: &str = "chelabase-event-id";
pub const EVENT_TYPE_HEADER: &str = "chelabase-event-type";
/// Each request's whole time.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// An event is retried for this long after it happened, then dropped.
pub const MAX_AGE: chrono::TimeDelta = chelabase_metadata::STORAGE_WEBHOOK_RETRY_WINDOW;
/// An endpoint failing for this long running is disabled (a run ends at a success, or after a
/// quiet spell longer than [`MAX_AGE`]: see [`chelabase_metadata::FailureRun`]).
pub const DISABLE_AFTER: chrono::TimeDelta = chrono::TimeDelta::hours(72);
/// The first retry's delay; each next one doubles, up to [`BACKOFF_CAP`].
pub const BACKOFF_BASE: Duration = Duration::from_secs(5);
pub const BACKOFF_CAP: Duration = Duration::from_secs(3600);
/// How often the endpoints are read again.
pub const POLL: Duration = Duration::from_secs(5);
/// Deliveries in flight per endpoint.
const CONCURRENCY: usize = 4;

type HmacSha256 = Hmac<Sha256>;

/// The `Chelabase-Signature` value for `body` sent at `t` (unix seconds).
pub fn signature(secret: &str, t: i64, body: &[u8]) -> String {
    format!(
        "t={t},v1={}",
        hex::encode(mac(secret, t, body).finalize().into_bytes())
    )
}

fn mac(secret: &str, t: i64, body: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC takes any key size");
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// Why a signature doesn't verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// No `t=` and `v1=` in the header.
    Malformed,
    /// Older (or newer) than the tolerance.
    Expired,
    /// No `v1` matches.
    Mismatch,
}

/// Checks a `Chelabase-Signature` header against `body` (what a receiver does): the timestamp
/// within `tolerance_secs` of `now`, and a `v1` that matches, compared in constant time.
pub fn verify(
    secret: &str,
    header: &str,
    body: &[u8],
    now: i64,
    tolerance_secs: i64,
) -> Result<(), VerifyError> {
    let mut t = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => t = v.parse::<i64>().ok(),
            Some(("v1", v)) => signatures.extend(hex::decode(v).ok()),
            _ => {}
        }
    }
    let (Some(t), false) = (t, signatures.is_empty()) else {
        return Err(VerifyError::Malformed);
    };
    if (now - t).abs() > tolerance_secs {
        return Err(VerifyError::Expired);
    }
    let expected = mac(secret, t, body);
    if signatures
        .iter()
        .any(|s| expected.clone().verify_slice(s).is_ok())
    {
        Ok(())
    } else {
        Err(VerifyError::Mismatch)
    }
}

/// Whether `event` is one `webhook` asked for: its type, and its bucket and name prefix when
/// set (a move matches on where the object went or where it came from).
pub fn matches(webhook: &StorageWebhook, event: &ObjectEvent) -> bool {
    if !webhook.events.iter().any(|e| e == event.kind.as_str())
        || event.kind == ObjectEventType::Test
    {
        return false;
    }
    let at = |bucket: &str, name: &str| {
        webhook.bucket.as_deref().is_none_or(|b| b == bucket)
            && webhook
                .prefix
                .as_deref()
                .is_none_or(|p| name.starts_with(p))
    };
    at(&event.bucket, &event.name) || event.from.as_ref().is_some_and(|f| at(&f.bucket, &f.name))
}

/// The delay before retry `attempt` (1 for the first retry): [`BACKOFF_BASE`] doubled per
/// attempt, capped at [`BACKOFF_CAP`], times `1 + jitter` (`jitter` in -0.2..=0.2).
pub fn backoff(attempt: u64, jitter: f64) -> Duration {
    let doublings = attempt.saturating_sub(1).min(30) as u32;
    let base = BACKOFF_BASE
        .saturating_mul(1u32 << doublings.min(31))
        .min(BACKOFF_CAP);
    base.mul_f64(1.0 + jitter.clamp(-0.2, 0.2))
}

/// What to do with a message after an attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Next {
    /// Delivered, or nothing to deliver: acknowledge.
    Ack,
    /// Failed: redeliver after the delay.
    Retry(Duration),
    /// Older than [`MAX_AGE`]: acknowledged unsent, and counted.
    Dropped,
    /// The endpoint failed for [`DISABLE_AFTER`]: it is disabled; acknowledge.
    Disabled,
}

/// After attempt number `attempt` (1 = the first) of an event from `event_time`: delivered is
/// done; a failure disables an endpoint failing since [`DISABLE_AFTER`] ago, drops an event
/// [`MAX_AGE`] old, and otherwise retries after [`backoff`].
pub fn decide(
    delivered: bool,
    attempt: u64,
    event_time: DateTime<Utc>,
    now: DateTime<Utc>,
    failing_since: Option<DateTime<Utc>>,
    jitter: f64,
) -> Next {
    if delivered {
        return Next::Ack;
    }
    if failing_since.is_some_and(|since| now - since >= DISABLE_AFTER) {
        return Next::Disabled;
    }
    if now - event_time >= MAX_AGE {
        return Next::Dropped;
    }
    Next::Retry(backoff(attempt, jitter))
}

type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T, MetadataError>> + Send + 'a>>;

/// Where attempts go (the metadata; a fake in tests).
pub trait AttemptStore: Send + Sync + 'static {
    /// Records an attempt; the endpoint's `failing_since` afterwards.
    fn record<'a>(
        &'a self,
        webhook: Uuid,
        attempt: &'a StorageWebhookAttempt,
    ) -> Fut<'a, Option<DateTime<Utc>>>;
    /// Disables the endpoint if it is still failing since `since`.
    fn disable_failing(&self, webhook: Uuid, since: DateTime<Utc>) -> Fut<'_, bool>;
}

impl AttemptStore for MetadataClient {
    fn record<'a>(
        &'a self,
        webhook: Uuid,
        attempt: &'a StorageWebhookAttempt,
    ) -> Fut<'a, Option<DateTime<Utc>>> {
        Box::pin(self.record_storage_webhook_attempt(webhook, attempt))
    }

    fn disable_failing(&self, webhook: Uuid, since: DateTime<Utc>) -> Fut<'_, bool> {
        Box::pin(self.disable_failing_storage_webhook(webhook, since))
    }
}

/// One attempt's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    pub outcome: AttemptOutcome,
    pub status: Option<u16>,
    pub duration_ms: u64,
}

/// Sends `event` to `url`, signed with `secret` at `now`. Never follows a redirect (a 3xx is a
/// failure) and never reaches a non-public address.
pub async fn send(
    client: &PublicClient,
    url: &str,
    secret: &str,
    event: &ObjectEvent,
    now: DateTime<Utc>,
) -> Sent {
    let started = Instant::now();
    let elapsed = |started: Instant| started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let body = match serde_json::to_vec(event) {
        Ok(body) => body,
        Err(_) => {
            return Sent {
                outcome: AttemptOutcome::ConnectError,
                status: None,
                duration_ms: 0,
            };
        }
    };
    let request = match client.post(url) {
        Ok(request) => request,
        Err(refused) => {
            warn!(webhook_url_host = %host_of(url), "webhook refused: {}", refused.log_detail());
            return Sent {
                outcome: AttemptOutcome::Refused,
                status: None,
                duration_ms: 0,
            };
        }
    };
    let response = request
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, "Chelabase-Webhooks/1")
        .header(SIGNATURE_HEADER, signature(secret, now.timestamp(), &body))
        .header(EVENT_ID_HEADER, event.id.to_string())
        .header(EVENT_TYPE_HEADER, event.kind.as_str())
        .body(body)
        .send()
        .await;
    match response {
        Ok(r) => {
            let status = r.status();
            Sent {
                outcome: if status.is_success() {
                    AttemptOutcome::Delivered
                } else {
                    AttemptOutcome::HttpError
                },
                status: Some(status.as_u16()),
                duration_ms: elapsed(started),
            }
        }
        Err(e) => Sent {
            outcome: if let Some(refused) = refused_by_guard(&e) {
                warn!(webhook_url_host = %host_of(url), "webhook refused: {}", refused.log_detail());
                AttemptOutcome::Refused
            } else if e.is_timeout() {
                AttemptOutcome::Timeout
            } else {
                AttemptOutcome::ConnectError
            },
            status: None,
            duration_ms: elapsed(started),
        },
    }
}

/// A URL's host for logs (never its path or query, which may hold a token).
fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// Delivers messages for the endpoints: the client, where attempts go, and the count of events
/// dropped after [`MAX_AGE`].
pub struct Delivery<S> {
    pub store: S,
    pub client: PublicClient,
    pub dropped: AtomicU64,
}

impl<S: AttemptStore> Delivery<S> {
    pub fn new(store: S, outbound: &Outbound) -> reqwest::Result<Self> {
        Ok(Self {
            store,
            client: outbound.client(TIMEOUT)?,
            dropped: AtomicU64::new(0),
        })
    }

    /// One delivery attempt (number `attempt`, 1 = the first) of the message `payload` to
    /// `target`, at `now`.
    pub async fn process(
        &self,
        target: &StorageWebhook,
        secret: &str,
        payload: &[u8],
        attempt: u64,
        now: DateTime<Utc>,
        jitter: f64,
    ) -> Next {
        let Ok(event) = serde_json::from_slice::<ObjectEvent>(payload) else {
            warn!(webhook = %target.id, "storage webhook: an unreadable event skipped");
            return Next::Ack;
        };
        // Only the endpoint's own database (its consumer's filter says so too).
        if event.database_id != target.database_id || !matches(target, &event) {
            return Next::Ack;
        }
        if now - event.time >= MAX_AGE {
            return self.drop_event(target, &event);
        }
        let sent = send(&self.client, &target.url, secret, &event, now).await;
        let record = StorageWebhookAttempt {
            event_id: event.id,
            event_type: event.kind.as_str().to_string(),
            attempt: attempt.min(i32::MAX as u64) as i32,
            outcome: sent.outcome.as_str().to_string(),
            status: sent.status.map(i32::from),
            duration_ms: sent.duration_ms.min(i32::MAX as u64) as i32,
            attempted_at: now,
        };
        let failing_since = match self.store.record(target.id, &record).await {
            Ok(f) => f,
            Err(e) => {
                warn!(webhook = %target.id, "storage webhook: recording an attempt: {e}");
                None
            }
        };
        let delivered = sent.outcome == AttemptOutcome::Delivered;
        match decide(delivered, attempt, event.time, now, failing_since, jitter) {
            Next::Disabled => {
                let since = failing_since.expect("Disabled only with a failure run");
                match self.store.disable_failing(target.id, since).await {
                    Ok(_) => {
                        warn!(webhook = %target.id, %since, "storage webhook disabled: failing for 3 days")
                    }
                    Err(e) => warn!(webhook = %target.id, "storage webhook: disabling: {e}"),
                }
                Next::Disabled
            }
            Next::Dropped => self.drop_event(target, &event),
            next => next,
        }
    }

    /// Events dropped since the process started.
    pub fn dropped_total(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn drop_event(&self, target: &StorageWebhook, event: &ObjectEvent) -> Next {
        let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        warn!(webhook = %target.id, event = %event.id, dropped_total = total, "storage webhook event dropped after 24 h of retries");
        Next::Dropped
    }
}

/// The totals line (#154; its target, message and field names are documented on the
/// self-hosting storage settings page, so log alerts can match them): `dropped_total` events
/// given up on since the worker started (each endpoint's count; an event dropped for two
/// endpoints counts twice), `dropped_since_last` of them since the previous line.
pub fn log_dropped_totals(total: u64, since_last: u64) {
    warn!(
        target: "worker::storage_webhooks",
        dropped_total = total,
        dropped_since_last = since_last,
        "storage webhook events dropped"
    );
}

/// A test event for `webhook`: type `webhook.test`, now, as the database's own; no object and
/// no branch (`branch_id` is the nil uuid).
pub fn test_event(webhook: &StorageWebhook, now: DateTime<Utc>) -> ObjectEvent {
    ObjectEvent {
        id: Uuid::new_v4(),
        kind: ObjectEventType::Test,
        time: now,
        database_id: webhook.database_id,
        branch_id: Uuid::nil(),
        pg_database: String::new(),
        bucket: webhook.bucket.clone().unwrap_or_default(),
        name: webhook.prefix.clone().unwrap_or_default(),
        version_id: None,
        size: None,
        content_type: None,
        actor: ObjectActor {
            role: String::new(),
            sub: None,
        },
        from: None,
    }
}

/// Answers a test request: one signed attempt now, not retried, not counted toward the
/// endpoint's failures.
pub async fn test_reply(
    metadata: &MetadataClient,
    key: &SecretKey,
    client: &PublicClient,
    request: &TestRequest,
) -> TestReply {
    let target = match metadata
        .storage_webhook_target(request.database_id, request.webhook_id)
        .await
    {
        Ok(Some(target)) => target,
        Ok(None) => return TestReply::NotFound,
        Err(e) => {
            return TestReply::Failed {
                message: format!("reading the endpoint: {e}"),
            };
        }
    };
    let Ok(secret) = open_webhook_secret(key, target.webhook.id, &target.secret_enc) else {
        return TestReply::Failed {
            message: "the endpoint's secret doesn't open".into(),
        };
    };
    let now = Utc::now();
    let sent = send(
        client,
        &target.webhook.url,
        &secret,
        &test_event(&target.webhook, now),
        now,
    )
    .await;
    TestReply::Sent {
        outcome: sent.outcome,
        status: sent.status,
        duration_ms: sent.duration_ms,
    }
}

/// An endpoint as a task delivers to it: the target and its opened secret.
struct Live {
    webhook: StorageWebhook,
    secret: String,
}

struct Running {
    task: tokio::task::JoinHandle<()>,
    live: tokio::sync::watch::Sender<Arc<Live>>,
}

/// Runs the endpoint tasks for as long as the process runs (see the module docs).
pub async fn run(
    metadata: MetadataClient,
    key: SecretKey,
    js: async_nats::jetstream::Context,
    outbound: Outbound,
) {
    let delivery = match Delivery::new(metadata.clone(), &outbound) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            warn!("storage webhooks don't run: building the client: {e}");
            return;
        }
    };
    tokio::spawn({
        let delivery = delivery.clone();
        async move {
            storage_objects::report_loss_totals(
                storage_objects::LOSS_TOTALS_EVERY,
                move || delivery.dropped_total(),
                log_dropped_totals,
            )
            .await
        }
    });
    let mut running: HashMap<String, Running> = HashMap::new();
    loop {
        match metadata.storage_webhook_targets().await {
            Ok(targets) => reconcile(&key, &js, &delivery, &mut running, targets).await,
            Err(e) => warn!("storage webhooks: reading the endpoints: {e}"),
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn reconcile(
    key: &SecretKey,
    js: &async_nats::jetstream::Context,
    delivery: &Arc<Delivery<MetadataClient>>,
    running: &mut HashMap<String, Running>,
    targets: Vec<DeliveryTarget>,
) {
    let mut wanted = HashMap::new();
    let mut held = HashSet::new();
    for t in targets {
        let name = storage_objects::endpoint_consumer_name(t.webhook.id, t.webhook.enabled_since);
        let Ok(secret) = open_webhook_secret(key, t.webhook.id, &t.secret_enc) else {
            warn!(webhook = %t.webhook.id, "storage webhook paused: its secret doesn't open (its consumer and pending retries are kept)");
            held.insert(name);
            continue;
        };
        wanted.insert(
            name,
            Arc::new(Live {
                webhook: t.webhook,
                secret,
            }),
        );
    }
    for (name, delete) in to_stop(running.keys(), &wanted, &held) {
        if let Some(r) = running.remove(&name) {
            r.task.abort();
        }
        if !delete {
            continue;
        }
        if let Err(e) = js
            .delete_consumer_from_stream(&name, storage_objects::STREAM)
            .await
        {
            warn!(consumer = %name, "storage webhook consumer not deleted (it expires unused): {e}");
        }
    }
    for (name, live) in wanted {
        match running.get(&name) {
            Some(r) if !r.task.is_finished() => {
                r.live.send_replace(live);
            }
            _ => {
                let (tx, rx) = tokio::sync::watch::channel(live);
                let task = tokio::spawn(endpoint_loop(js.clone(), delivery.clone(), rx));
                running.insert(name, Running { task, live: tx });
            }
        }
    }
}

/// The running endpoints to stop: every one no longer wanted, each with whether its consumer
/// is deleted too. A held endpoint (still enabled, its secret not opening) keeps its consumer,
/// so its position and pending retries survive until the secret opens again.
fn to_stop<'a, T>(
    running: impl Iterator<Item = &'a String>,
    wanted: &HashMap<String, T>,
    held: &HashSet<String>,
) -> Vec<(String, bool)> {
    let mut stop: Vec<(String, bool)> = running
        .filter(|n| !wanted.contains_key(*n))
        .map(|n| (n.clone(), !held.contains(n)))
        .collect();
    stop.sort();
    stop
}

/// One endpoint's consumer, consumed until the task is stopped (again after [`POLL`] when it
/// fails: the stream not there yet, NATS away).
async fn endpoint_loop(
    js: async_nats::jetstream::Context,
    delivery: Arc<Delivery<MetadataClient>>,
    live: tokio::sync::watch::Receiver<Arc<Live>>,
) {
    loop {
        if let Err(e) = consume(&js, &delivery, &live).await {
            let id = live.borrow().webhook.id;
            warn!(webhook = %id, "storage webhook consumer: {e}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn consume(
    js: &async_nats::jetstream::Context,
    delivery: &Delivery<MetadataClient>,
    live: &tokio::sync::watch::Receiver<Arc<Live>>,
) -> anyhow::Result<()> {
    let config = {
        let l = live.borrow();
        storage_objects::endpoint_consumer(
            l.webhook.id,
            l.webhook.database_id,
            l.webhook.enabled_since,
        )
    };
    let consumer = js
        .create_consumer_on_stream(config, storage_objects::STREAM)
        .await
        .map_err(|e| anyhow::anyhow!("creating the consumer: {e}"))?;
    let messages = consumer
        .messages()
        .await
        .map_err(|e| anyhow::anyhow!("pulling: {e}"))?;
    info!(webhook = %live.borrow().webhook.id, "storage webhook delivering");
    messages
        .for_each_concurrent(CONCURRENCY, |message| async move {
            let message = match message {
                Ok(m) => m,
                Err(e) => {
                    warn!("storage webhook message: {e}");
                    return;
                }
            };
            let attempt = message
                .info()
                .map(|i| i.delivered.max(1) as u64)
                .unwrap_or(1);
            let current = live.borrow().clone();
            let jitter = (rand::random::<f64>() - 0.5) * 0.4;
            let next = delivery
                .process(
                    &current.webhook,
                    &current.secret,
                    &message.payload,
                    attempt,
                    Utc::now(),
                    jitter,
                )
                .await;
            let ack = match next {
                Next::Retry(delay) => {
                    message
                        .ack_with(async_nats::jetstream::AckKind::Nak(Some(delay)))
                        .await
                }
                Next::Ack | Next::Dropped | Next::Disabled => message.ack().await,
            };
            if let Err(e) = ack {
                warn!(webhook = %current.webhook.id, "storage webhook ack: {e}");
            }
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_nats::storage_objects::ObjectFrom;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const SECRET: &str = "whsec_0123456789abcdef";

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    // --- signing (AC 4) ---

    #[test]
    fn a_signature_round_trips_and_tampering_fails() {
        let body = br#"{"id":"x","type":"object.created"}"#;
        let header = signature(SECRET, 1_700_000_000, body);
        assert!(header.starts_with("t=1700000000,v1="), "{header}");
        assert_eq!(header.len(), "t=1700000000,v1=".len() + 64);
        assert_eq!(verify(SECRET, &header, body, 1_700_000_000, 300), Ok(()));
        assert_eq!(verify(SECRET, &header, body, 1_700_000_299, 300), Ok(()));
        // HMAC-SHA256 over "<t>.<body>", hex, keyed with the secret's bytes.
        let mut mac = HmacSha256::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(b"1700000000.");
        mac.update(body);
        assert_eq!(
            header,
            format!(
                "t=1700000000,v1={}",
                hex::encode(mac.finalize().into_bytes())
            )
        );

        assert_eq!(
            verify(
                SECRET,
                &header,
                br#"{"id":"y","type":"object.created"}"#,
                1_700_000_000,
                300
            ),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify("whsec_other", &header, body, 1_700_000_000, 300),
            Err(VerifyError::Mismatch)
        );
        let flipped = header.replace("t=1700000000", "t=1700000001");
        assert_eq!(
            verify(SECRET, &flipped, body, 1_700_000_001, 300),
            Err(VerifyError::Mismatch)
        );
        let mut bad_sig = header.clone();
        let last = bad_sig.pop().unwrap();
        bad_sig.push(if last == '0' { '1' } else { '0' });
        assert_eq!(
            verify(SECRET, &bad_sig, body, 1_700_000_000, 300),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(SECRET, &header, body, 1_700_000_301, 300),
            Err(VerifyError::Expired)
        );
        assert_eq!(
            verify(SECRET, &header, body, 1_699_999_699, 300),
            Err(VerifyError::Expired)
        );
        for bad in ["", "t=1700000000", "v1=abc", "t=x,v1=00", "nonsense"] {
            assert_eq!(
                verify(SECRET, bad, body, 1_700_000_000, 300),
                Err(VerifyError::Malformed),
                "{bad}"
            );
        }
        // Several v1 values (a receiver mid-rotation): any match passes.
        let both = format!("{header},v1={}", "0".repeat(64));
        assert_eq!(verify(SECRET, &both, body, 1_700_000_000, 300), Ok(()));
    }

    // --- filters ---

    fn webhook(events: &[&str], bucket: Option<&str>, prefix: Option<&str>) -> StorageWebhook {
        StorageWebhook {
            id: Uuid::new_v4(),
            database_id: Uuid::nil(),
            url: "https://example.com/hook".into(),
            events: events.iter().map(|s| s.to_string()).collect(),
            bucket: bucket.map(Into::into),
            prefix: prefix.map(Into::into),
            enabled: true,
            enabled_since: at(0),
            disabled_reason: None,
            failing_since: None,
            consecutive_failures: 0,
            last_attempt_at: None,
            created_at: at(0),
            updated_at: at(0),
        }
    }

    fn event(kind: ObjectEventType, bucket: &str, name: &str, time: DateTime<Utc>) -> ObjectEvent {
        ObjectEvent {
            id: Uuid::new_v4(),
            kind,
            time,
            database_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            pg_database: "app".into(),
            bucket: bucket.into(),
            name: name.into(),
            version_id: Some(Uuid::new_v4()),
            size: Some(3),
            content_type: Some("image/png".into()),
            actor: ObjectActor {
                role: "authenticated".into(),
                sub: None,
            },
            from: None,
        }
    }

    #[test]
    fn filters_match_type_bucket_and_prefix() {
        let created = event(ObjectEventType::Created, "avatars", "u/1.png", at(0));
        assert!(matches(
            &webhook(&["object.created"], Some("avatars"), None),
            &created
        ));
        assert!(!matches(
            &webhook(&["object.created"], Some("docs"), None),
            &created
        ));
        assert!(!matches(
            &webhook(&["object.deleted"], None, None),
            &created
        ));
        assert!(matches(
            &webhook(&["object.created"], None, Some("u/")),
            &created
        ));
        assert!(!matches(
            &webhook(&["object.created"], None, Some("v/")),
            &created
        ));
        assert!(
            !matches(
                &webhook(&["object.created"], Some("Avatars"), None),
                &created
            ),
            "exact bucket name"
        );
        let mut moved = event(ObjectEventType::Moved, "archive", "old/1.png", at(0));
        moved.from = Some(ObjectFrom {
            bucket: "avatars".into(),
            name: "u/1.png".into(),
        });
        assert!(
            matches(
                &webhook(&["object.moved"], Some("avatars"), Some("u/")),
                &moved
            ),
            "from where it was"
        );
        assert!(
            matches(
                &webhook(&["object.moved"], Some("archive"), Some("old/")),
                &moved
            ),
            "to where it went"
        );
        assert!(!matches(
            &webhook(&["object.moved"], Some("avatars"), Some("old/")),
            &moved
        ));
        // A test event never matches a subscription.
        let test = test_event(&webhook(&["object.created"], None, None), at(0));
        assert!(!matches(&webhook(&["object.created"], None, None), &test));
    }

    // --- the delivery state machine, with a fake clock (AC 2, 7) ---

    #[test]
    fn backoff_doubles_with_bounded_jitter_and_a_cap() {
        assert_eq!(backoff(1, 0.0), Duration::from_secs(5));
        assert_eq!(backoff(2, 0.0), Duration::from_secs(10));
        assert_eq!(backoff(3, 0.0), Duration::from_secs(20));
        assert_eq!(backoff(40, 0.0), BACKOFF_CAP);
        assert_eq!(backoff(1, 0.2), Duration::from_secs(6));
        assert_eq!(backoff(1, -0.2), Duration::from_secs(4));
        assert_eq!(backoff(1, 9.0), Duration::from_secs(6), "jitter is clamped");
        assert_eq!(backoff(0, 0.0), Duration::from_secs(5));
        // Increasing whatever the jitter, until the cap.
        for n in 1..9 {
            assert!(backoff(n, 0.2) < backoff(n + 1, -0.2), "{n}");
        }
    }

    #[test]
    fn decide_retries_then_drops_after_a_day_and_disables_after_three() {
        let t0 = at(1_700_000_000);
        let h = chrono::TimeDelta::hours;
        assert_eq!(decide(true, 1, t0, t0, None, 0.0), Next::Ack);
        assert_eq!(
            decide(true, 9, t0, t0 + h(30), Some(t0 - h(100)), 0.0),
            Next::Ack,
            "delivered wins"
        );
        assert_eq!(
            decide(false, 1, t0, t0, Some(t0), 0.0),
            Next::Retry(Duration::from_secs(5))
        );
        assert_eq!(
            decide(false, 3, t0, t0 + h(1), Some(t0), 0.0),
            Next::Retry(Duration::from_secs(20))
        );
        assert_eq!(
            decide(
                false,
                3,
                t0,
                t0 + h(24) - chrono::TimeDelta::seconds(1),
                Some(t0),
                0.0
            ),
            Next::Retry(Duration::from_secs(20))
        );
        assert_eq!(
            decide(false, 30, t0, t0 + h(24), Some(t0), 0.0),
            Next::Dropped
        );
        assert_eq!(
            decide(
                false,
                1,
                t0,
                t0,
                Some(t0 - h(72) + chrono::TimeDelta::seconds(1)),
                0.0
            ),
            Next::Retry(Duration::from_secs(5))
        );
        assert_eq!(
            decide(false, 1, t0, t0, Some(t0 - h(72)), 0.0),
            Next::Disabled
        );
        assert_eq!(
            decide(false, 1, t0, t0 + h(25), Some(t0 - h(80)), 0.0),
            Next::Disabled,
            "disable before drop"
        );
    }

    /// The metadata's attempt bookkeeping, in memory: the same failure-run rule
    /// ([`chelabase_metadata::FailureRun`]).
    struct FakeStore {
        attempts: Mutex<Vec<(Uuid, StorageWebhookAttempt)>>,
        run: Mutex<(chelabase_metadata::FailureRun, Option<DateTime<Utc>>)>,
        disabled: Mutex<Vec<(Uuid, DateTime<Utc>)>>,
    }

    impl Default for FakeStore {
        fn default() -> Self {
            Self {
                attempts: Mutex::default(),
                run: Mutex::new((
                    chelabase_metadata::FailureRun {
                        failing_since: None,
                        consecutive_failures: 0,
                    },
                    None,
                )),
                disabled: Mutex::default(),
            }
        }
    }

    impl FakeStore {
        /// Failing since `since`, the last attempt at `last`.
        fn failing(since: DateTime<Utc>, last: DateTime<Utc>) -> Arc<Self> {
            let store = Self::default();
            *store.run.lock().unwrap() = (
                chelabase_metadata::FailureRun {
                    failing_since: Some(since),
                    consecutive_failures: 1,
                },
                Some(last),
            );
            Arc::new(store)
        }
    }

    impl AttemptStore for Arc<FakeStore> {
        fn record<'a>(
            &'a self,
            webhook: Uuid,
            attempt: &'a StorageWebhookAttempt,
        ) -> Fut<'a, Option<DateTime<Utc>>> {
            let mut run = self.run.lock().unwrap();
            let next = run
                .0
                .after(run.1, attempt.outcome == "delivered", attempt.attempted_at);
            // As the store: the last attempt never moves back.
            *run = (next, run.1.max(Some(attempt.attempted_at)));
            self.attempts
                .lock()
                .unwrap()
                .push((webhook, attempt.clone()));
            let out = next.failing_since;
            Box::pin(std::future::ready(Ok(out)))
        }

        fn disable_failing(&self, webhook: Uuid, since: DateTime<Utc>) -> Fut<'_, bool> {
            self.disabled.lock().unwrap().push((webhook, since));
            Box::pin(std::future::ready(Ok(true)))
        }
    }

    /// A local endpoint answering `statuses` in turn (the last repeats), keeping each request.
    async fn endpoint(statuses: &[u16]) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (log, statuses) = (seen.clone(), statuses.to_vec());
        tokio::spawn(async move {
            let mut n = 0;
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 65536];
                let mut len = 0;
                // Read the head and the body (content-length).
                while let Ok(k) = sock.read(&mut buf[len..]).await {
                    if k == 0 {
                        break;
                    }
                    len += k;
                    let text = String::from_utf8_lossy(&buf[..len]).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let cl = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if len >= end + 4 + cl {
                            break;
                        }
                    }
                }
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..len]).to_string());
                let status = statuses[n.min(statuses.len() - 1)];
                n += 1;
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (port, seen)
    }

    fn local(store: Arc<FakeStore>) -> Delivery<Arc<FakeStore>> {
        let outbound = Outbound::new().allow_http(true).allow_hosts(["127.0.0.1"]);
        Delivery::new(store, &outbound).unwrap()
    }

    fn header_of(request: &str, name: &str) -> Option<String> {
        request.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }

    fn body_of(request: &str) -> &str {
        &request[request.find("\r\n\r\n").unwrap() + 4..]
    }

    /// AC 2: 500 three times then 200: four attempts of the same event id, each signed, with
    /// increasing backoff between them, then acknowledged.
    #[tokio::test]
    async fn a_failing_endpoint_gets_the_same_event_until_it_answers() {
        let (port, seen) = endpoint(&[500, 500, 500, 200]).await;
        let store = Arc::new(FakeStore::default());
        let d = local(store.clone());
        let mut w = webhook(&["object.created"], Some("avatars"), None);
        w.url = format!("http://127.0.0.1:{port}/hook");
        let t0 = at(Utc::now().timestamp());
        let e = event(ObjectEventType::Created, "avatars", "a.png", t0);
        let payload = serde_json::to_vec(&e).unwrap();
        let mut delays = Vec::new();
        for attempt in 1..=4u64 {
            let now = t0 + chrono::TimeDelta::seconds(attempt as i64 * 10);
            match d.process(&w, SECRET, &payload, attempt, now, 0.0).await {
                Next::Retry(delay) => delays.push(delay),
                Next::Ack => assert_eq!(attempt, 4),
                other => panic!("attempt {attempt}: {other:?}"),
            }
        }
        assert_eq!(delays.len(), 3);
        assert!(delays.windows(2).all(|p| p[0] < p[1]), "{delays:?}");
        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 4);
        for (i, r) in requests.iter().enumerate() {
            assert!(r.starts_with("POST /hook "), "{r}");
            assert_eq!(header_of(r, EVENT_ID_HEADER), Some(e.id.to_string()));
            assert_eq!(
                header_of(r, EVENT_TYPE_HEADER).as_deref(),
                Some("object.created")
            );
            assert_eq!(
                header_of(r, "content-type").as_deref(),
                Some("application/json")
            );
            let body = body_of(r);
            let sent: ObjectEvent = serde_json::from_str(body).unwrap();
            assert_eq!(sent, e);
            let now = (t0 + chrono::TimeDelta::seconds((i as i64 + 1) * 10)).timestamp();
            assert_eq!(
                verify(
                    SECRET,
                    &header_of(r, SIGNATURE_HEADER).unwrap(),
                    body.as_bytes(),
                    now,
                    300
                ),
                Ok(())
            );
        }
        let attempts = store.attempts.lock().unwrap();
        let recorded: Vec<_> = attempts
            .iter()
            .map(|(_, a)| (a.attempt, a.outcome.as_str(), a.status))
            .collect();
        assert_eq!(
            recorded,
            [
                (1, "http_error", Some(500)),
                (2, "http_error", Some(500)),
                (3, "http_error", Some(500)),
                (4, "delivered", Some(200))
            ]
        );
        assert!(attempts.iter().all(|(_, a)| a.event_id == e.id));
    }

    /// Nothing is sent for another database's event, an unmatched one, or one past the retry
    /// window (dropped and counted).
    #[tokio::test]
    async fn unmatched_foreign_and_stale_events_are_not_sent() {
        let (port, seen) = endpoint(&[200]).await;
        let store = Arc::new(FakeStore::default());
        let d = local(store.clone());
        let mut w = webhook(&["object.created"], Some("avatars"), None);
        w.url = format!("http://127.0.0.1:{port}/hook");
        let now = Utc::now();
        let other_bucket = event(ObjectEventType::Created, "docs", "a.png", now);
        let mut foreign = event(ObjectEventType::Created, "avatars", "a.png", now);
        foreign.database_id = Uuid::new_v4();
        let stale = event(ObjectEventType::Created, "avatars", "a.png", now - MAX_AGE);
        for (e, want) in [
            (&other_bucket, Next::Ack),
            (&foreign, Next::Ack),
            (&stale, Next::Dropped),
        ] {
            let payload = serde_json::to_vec(e).unwrap();
            assert_eq!(d.process(&w, SECRET, &payload, 1, now, 0.0).await, want);
        }
        assert_eq!(
            d.process(&w, SECRET, b"not json", 1, now, 0.0).await,
            Next::Ack
        );
        assert!(seen.lock().unwrap().is_empty());
        assert!(store.attempts.lock().unwrap().is_empty());
        assert_eq!(d.dropped.load(Ordering::SeqCst), 1);
    }

    /// What `f` logs, as the default text format writes it (no colours).
    fn logged(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(crate::DEFAULT_LOG_FILTER)
            .with_writer({
                let buf = buf.clone();
                move || buf.clone()
            })
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    /// #154: each event dropped after the retry window adds one to the dropped total, and the
    /// totals line carries it (and its growth) under a stable target, message and field
    /// names that the default log filter keeps.
    #[tokio::test]
    async fn a_dropped_event_is_counted_in_the_totals_line() {
        let d = local(Arc::new(FakeStore::default()));
        let w = webhook(&["object.created"], None, None);
        let now = Utc::now();
        let stale = event(ObjectEventType::Created, "a", "b", now - MAX_AGE);
        let payload = serde_json::to_vec(&stale).unwrap();
        for _ in 0..3 {
            assert_eq!(
                d.process(&w, SECRET, &payload, 9, now, 0.0).await,
                Next::Dropped
            );
        }
        assert_eq!(d.dropped_total(), 3);
        let line = logged(|| log_dropped_totals(d.dropped_total(), 2));
        assert!(
            line.contains(" WARN worker::storage_webhooks: storage webhook events dropped"),
            "{line}"
        );
        assert!(
            line.contains("dropped_total=3 dropped_since_last=2"),
            "{line}"
        );
    }

    /// AC 7: an endpoint failing since three days ago is disabled at its next failure.
    #[tokio::test]
    async fn an_endpoint_failing_for_three_days_is_disabled() {
        let (port, _) = endpoint(&[503]).await;
        let now = Utc::now();
        let store = FakeStore::failing(now - DISABLE_AFTER, now - BACKOFF_CAP);
        let d = local(store.clone());
        let mut w = webhook(&["object.created"], None, None);
        w.url = format!("http://127.0.0.1:{port}/hook");
        let payload = serde_json::to_vec(&event(ObjectEventType::Created, "a", "b", now)).unwrap();
        assert_eq!(
            d.process(&w, SECRET, &payload, 1, now, 0.0).await,
            Next::Disabled
        );
        assert_eq!(
            *store.disabled.lock().unwrap(),
            [(w.id, now - DISABLE_AFTER)]
        );
    }

    /// An endpoint that failed one event for a day (dropped), then had three quiet days, is
    /// not disabled at its first failure back: that failure starts a new run.
    #[tokio::test]
    async fn a_failure_after_a_quiet_spell_starts_a_new_run_not_a_disable() {
        let (port, _) = endpoint(&[502]).await;
        let now = Utc::now();
        let h = chrono::TimeDelta::hours;
        let store = FakeStore::failing(now - h(96), now - h(72));
        let d = local(store.clone());
        let mut w = webhook(&["object.created"], None, None);
        w.url = format!("http://127.0.0.1:{port}/hook");
        let payload = serde_json::to_vec(&event(ObjectEventType::Created, "a", "b", now)).unwrap();
        assert_eq!(
            d.process(&w, SECRET, &payload, 1, now, 0.0).await,
            Next::Retry(BACKOFF_BASE)
        );
        assert!(store.disabled.lock().unwrap().is_empty());
        assert_eq!(store.run.lock().unwrap().0.failing_since, Some(now));
    }

    /// Reconciling stops what is no longer wanted; only a held endpoint (its secret not
    /// opening) keeps its consumer and so its pending retries.
    #[test]
    fn a_held_endpoint_keeps_its_consumer() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let running = names(&["a", "b", "c"]);
        let wanted: HashMap<String, ()> = [("a".to_string(), ())].into();
        let held: HashSet<String> = ["b".to_string()].into();
        assert_eq!(
            to_stop(running.iter(), &wanted, &held),
            [("b".to_string(), false), ("c".to_string(), true)]
        );
    }

    /// A test event names its database and no branch (it comes from none).
    #[test]
    fn a_test_event_has_no_branch() {
        let mut w = webhook(&["object.created"], None, None);
        w.database_id = Uuid::new_v4();
        let e = test_event(&w, at(0));
        assert_eq!((e.database_id, e.branch_id), (w.database_id, Uuid::nil()));
    }

    /// AC 3 at delivery: a private target (registered before, or allowed then) is refused with
    /// nothing sent, recorded as `refused`, and retried like any failure.
    #[tokio::test]
    async fn a_private_target_is_refused_at_delivery() {
        let (port, seen) = endpoint(&[200]).await;
        let store = Arc::new(FakeStore::default());
        let d = Delivery::new(store.clone(), &Outbound::new().allow_http(true)).unwrap();
        let now = Utc::now();
        let payload = serde_json::to_vec(&event(ObjectEventType::Created, "a", "b", now)).unwrap();
        for url in [
            format!("http://127.0.0.1:{port}/"),
            format!("http://localhost:{port}/"),
        ] {
            let mut w = webhook(&["object.created"], None, None);
            w.url = url.clone();
            assert!(
                matches!(
                    d.process(&w, SECRET, &payload, 1, now, 0.0).await,
                    Next::Retry(_)
                ),
                "{url}"
            );
        }
        assert!(seen.lock().unwrap().is_empty());
        let attempts = store.attempts.lock().unwrap();
        assert!(
            attempts
                .iter()
                .all(|(_, a)| a.outcome == "refused" && a.status.is_none()),
            "{attempts:?}"
        );
    }
}
