//! Storage object events (object-storage m18): what the storage service publishes after each
//! successful object write, and what the worker delivers to a database's webhook endpoints.
//!
//! The storage service publishes each event to the JetStream stream [`STREAM`] on its
//! database's subject ([`subject`]: `chelabase.storage.events.<database_id>`; the cache events'
//! `chelabase.storage.events` has one token less, so neither ever matches the other). Only the
//! orchestrator creates or fixes the stream ([`ensure_stream`], R10). The worker reads it
//! through one durable consumer per enabled endpoint ([`endpoint_consumer`]), filtered to the
//! endpoint's database: each endpoint's position and pending retries are its own.
//!
//! An event never carries object contents or a signed URL.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use crate::NatsError;

/// The object events stream.
pub const STREAM: &str = "CHELABASE_STORAGE_EVENTS";

/// The subjects' common prefix; one more token names the Chelabase database.
const SUBJECT_PREFIX: &str = "chelabase.storage.events";

/// How long the stream keeps an event: the 24 h retry window plus an hour's margin.
pub const RETENTION: std::time::Duration = std::time::Duration::from_secs(25 * 3600);

/// Events the stream keeps for one database (one subject): a busy database drops its own
/// oldest events past this, never another database's pending ones. A tenth of the stream's
/// whole count, so ten such databases fit before the stream-wide limits apply.
pub const MAX_EVENTS_PER_DATABASE: i64 = 100_000;

/// How long a consumer nobody pulls from lives on (an endpoint deleted while no worker ran).
pub const CONSUMER_INACTIVE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// `chelabase.storage.events.<database_id>`: a uuid is one token, so no value widens it.
pub fn subject(database_id: Uuid) -> String {
    format!("{SUBJECT_PREFIX}.{database_id}")
}

/// What happened to an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum ObjectEventType {
    /// A new object (an upload, a completed resumable upload, a copy: a copy never replaces
    /// an existing name).
    #[serde(rename = "object.created")]
    Created,
    /// An existing object replaced by an upsert.
    #[serde(rename = "object.updated")]
    Updated,
    #[serde(rename = "object.deleted")]
    Deleted,
    /// Renamed, or moved to another bucket (`from` names where it was).
    #[serde(rename = "object.moved")]
    Moved,
    /// A test event sent on request (`POST …/webhooks/{id}/test`); never subscribed to.
    #[serde(rename = "webhook.test")]
    Test,
}

impl ObjectEventType {
    pub const ALL: [ObjectEventType; 4] =
        [Self::Created, Self::Updated, Self::Deleted, Self::Moved];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "object.created",
            Self::Updated => "object.updated",
            Self::Deleted => "object.deleted",
            Self::Moved => "object.moved",
            Self::Test => "webhook.test",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

/// Who made the change: the database role the request ran as, and the caller's `sub` claim
/// (an app user's id) when it had one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectActor {
    pub role: String,
    pub sub: Option<String>,
}

/// Where a moved object was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectFrom {
    pub bucket: String,
    pub name: String,
}

/// One object event, as published and as delivered (the webhook body is this, as JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEvent {
    /// Unique per event; every delivery attempt of it carries the same id.
    pub id: Uuid,
    #[serde(rename = "type")]
    pub kind: ObjectEventType,
    pub time: DateTime<Utc>,
    /// The Chelabase database.
    pub database_id: Uuid,
    pub branch_id: Uuid,
    /// The Postgres database on the branch.
    pub pg_database: String,
    pub bucket: String,
    pub name: String,
    /// The object's version (absent for a delete).
    pub version_id: Option<Uuid>,
    pub size: Option<i64>,
    pub content_type: Option<String>,
    pub actor: ObjectActor,
    /// Only on `object.moved`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ObjectFrom>,
}

/// The stream: limits retention (each endpoint's consumer reads independently; nothing is
/// consumed away), [`RETENTION`], bounded in count and bytes with the oldest dropped first,
/// each database also bounded on its own ([`MAX_EVENTS_PER_DATABASE`]), and a duplicate window
/// so a retried publish (same `Nats-Msg-Id`, the event id) is stored once.
pub fn stream_config() -> async_nats::jetstream::stream::Config {
    use async_nats::jetstream::stream::{Config, DiscardPolicy, RetentionPolicy, StorageType};
    Config {
        name: STREAM.to_string(),
        subjects: vec![format!("{SUBJECT_PREFIX}.*")],
        retention: RetentionPolicy::Limits,
        storage: StorageType::File,
        max_age: RETENTION,
        max_messages: 1_000_000,
        max_messages_per_subject: MAX_EVENTS_PER_DATABASE,
        max_bytes: 512 * 1024 * 1024,
        max_message_size: 64 * 1024,
        discard: DiscardPolicy::Old,
        duplicate_window: std::time::Duration::from_secs(120),
        num_replicas: 1,
        ..Default::default()
    }
}

fn drifted(
    have: &async_nats::jetstream::stream::Config,
    want: &async_nats::jetstream::stream::Config,
) -> bool {
    have.subjects != want.subjects
        || have.retention != want.retention
        || have.storage != want.storage
        || have.max_age != want.max_age
        || have.max_messages != want.max_messages
        || have.max_messages_per_subject != want.max_messages_per_subject
        || have.max_bytes != want.max_bytes
        || have.max_message_size != want.max_message_size
        || have.discard != want.discard
        || have.duplicate_window != want.duplicate_window
        || have.num_replicas != want.num_replicas
}

/// Creates the stream, or puts back the settings of one that drifted (the orchestrator, at
/// start).
pub async fn ensure_stream(js: &async_nats::jetstream::Context) -> Result<(), NatsError> {
    let want = stream_config();
    match js.get_stream(STREAM).await {
        Ok(stream) => {
            if drifted(&stream.cached_info().config, &want) {
                js.update_stream(want)
                    .await
                    .map_err(|e| NatsError::JetStream(e.to_string()))?;
                info!(stream = STREAM, "jetstream stream config restored");
            }
        }
        Err(err)
            if matches!(
                err.kind(),
                async_nats::jetstream::context::GetStreamErrorKind::JetStream(e)
                    if e.error_code() == async_nats::jetstream::ErrorCode::STREAM_NOT_FOUND
            ) =>
        {
            js.create_stream(want)
                .await
                .map_err(|e| NatsError::JetStream(e.to_string()))?;
            info!(stream = STREAM, "jetstream stream created");
        }
        Err(err) => return Err(NatsError::JetStream(err.to_string())),
    }
    Ok(())
}

/// Publishes `event` on its database's subject and waits for the stream's ack; the event id is
/// the message id, so a retried publish is stored once.
pub async fn publish(
    js: &async_nats::jetstream::Context,
    event: &ObjectEvent,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(event)?;
    let message = async_nats::jetstream::message::PublishMessage::build()
        .payload(payload.into())
        .message_id(event.id.to_string());
    js.send_publish(subject(event.database_id), message)
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

/// A JetStream context on `client` (what [`publish`] takes).
pub fn jetstream(client: async_nats::Client) -> async_nats::jetstream::Context {
    async_nats::jetstream::new(client)
}

/// The durable consumer name of an endpoint enabled since `since`: a re-enabled endpoint gets a
/// new consumer starting then (the old one, if a worker never deleted it, expires after
/// [`CONSUMER_INACTIVE`]).
pub fn endpoint_consumer_name(endpoint: Uuid, since: DateTime<Utc>) -> String {
    format!("storage-webhook-{endpoint}-{}", since.timestamp_millis())
}

/// An endpoint's consumer: durable, filtered to its database's subject, delivering from the
/// time it was enabled, explicit acks (a failed delivery is NAKed with its backoff, so
/// redelivery is the worker's to time), at most `max_ack_pending` in flight.
pub fn endpoint_consumer(
    endpoint: Uuid,
    database_id: Uuid,
    since: DateTime<Utc>,
) -> async_nats::jetstream::consumer::pull::Config {
    use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, pull};
    let name = endpoint_consumer_name(endpoint, since);
    pull::Config {
        durable_name: Some(name.clone()),
        name: Some(name),
        filter_subject: subject(database_id),
        deliver_policy: DeliverPolicy::ByStartTime {
            start_time: time::OffsetDateTime::from_unix_timestamp_nanos(
                since.timestamp_nanos_opt().unwrap_or_default() as i128,
            )
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH),
        },
        ack_policy: AckPolicy::Explicit,
        // Past the request timeout (10 s) and the signing: a worker that dies mid-delivery
        // has its message redelivered after this.
        ack_wait: std::time::Duration::from_secs(60),
        max_deliver: -1,
        max_ack_pending: 64,
        inactive_threshold: CONSUMER_INACTIVE,
        ..Default::default()
    }
}

/// Core request/reply (not JetStream): send one test event to one endpoint now.
pub const SUBJECT_TEST: &str = "chelabase.rpc.storage_webhooks.test";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRequest {
    pub database_id: Uuid,
    pub webhook_id: Uuid,
}

/// How one delivery attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// A 2xx answer.
    Delivered,
    /// Any other HTTP status.
    HttpError,
    /// No answer within the timeout.
    Timeout,
    /// The connection failed.
    ConnectError,
    /// The URL or its address was refused (not public).
    Refused,
}

impl AttemptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::HttpError => "http_error",
            Self::Timeout => "timeout",
            Self::ConnectError => "connect_error",
            Self::Refused => "refused",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Delivered,
            Self::HttpError,
            Self::Timeout,
            Self::ConnectError,
            Self::Refused,
        ]
        .into_iter()
        .find(|o| o.as_str() == s)
    }
}

/// The test's answer: the attempt (as recorded), or why none was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum TestReply {
    Sent {
        outcome: AttemptOutcome,
        status: Option<u16>,
        duration_ms: u64,
    },
    NotFound,
    Failed {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_subject_is_one_token_per_database_and_not_the_cache_subject() {
        let db = Uuid::nil();
        assert_eq!(
            subject(db),
            "chelabase.storage.events.00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(subject(db).split('.').count(), 4);
        assert_eq!(stream_config().subjects, ["chelabase.storage.events.*"]);
        assert_ne!(crate::SUBJECT_STORAGE_EVENTS.split('.').count(), 4);
        assert!(
            !crate::control_subjects()
                .iter()
                .any(|s| s.starts_with(SUBJECT_PREFIX))
        );
    }

    /// One database can hold only its share of the stream: past that, its own oldest events go,
    /// never another database's. A stream without the cap counts as drifted, so an existing
    /// one is updated at start.
    #[test]
    fn each_database_is_capped_within_the_stream() {
        let want = stream_config();
        assert!(want.max_messages_per_subject > 0);
        assert!(want.max_messages_per_subject * 10 <= want.max_messages);
        let mut have = want.clone();
        assert!(!drifted(&have, &want));
        have.max_messages_per_subject = -1;
        assert!(drifted(&have, &want), "an uncapped stream is updated");
    }

    #[test]
    fn the_event_has_its_wire_shape() {
        let e = ObjectEvent {
            id: Uuid::nil(),
            kind: ObjectEventType::Moved,
            time: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            database_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            pg_database: "app".into(),
            bucket: "avatars".into(),
            name: "b.png".into(),
            version_id: Some(Uuid::nil()),
            size: Some(3),
            content_type: Some("image/png".into()),
            actor: ObjectActor {
                role: "authenticated".into(),
                sub: Some("u1".into()),
            },
            from: Some(ObjectFrom {
                bucket: "avatars".into(),
                name: "a.png".into(),
            }),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "object.moved");
        assert_eq!(v["time"], "2023-11-14T22:13:20Z");
        assert_eq!(v["actor"], json!({ "role": "authenticated", "sub": "u1" }));
        assert_eq!(v["from"], json!({ "bucket": "avatars", "name": "a.png" }));
        assert_eq!(serde_json::from_value::<ObjectEvent>(v).unwrap(), e);
        let created = ObjectEvent {
            kind: ObjectEventType::Created,
            from: None,
            ..e
        };
        assert!(
            serde_json::to_value(&created)
                .unwrap()
                .get("from")
                .is_none()
        );
        for t in ObjectEventType::ALL {
            assert_eq!(ObjectEventType::parse(t.as_str()), Some(t));
            assert_eq!(serde_json::to_value(t).unwrap(), t.as_str());
        }
        assert_eq!(ObjectEventType::parse("object.read"), None);
        assert_eq!(
            ObjectEventType::parse("webhook.test"),
            None,
            "not subscribable"
        );
        assert_eq!(
            serde_json::to_value(ObjectEventType::Test).unwrap(),
            "webhook.test"
        );
    }

    #[test]
    fn the_stream_is_bounded_and_keeps_events_past_the_retry_window() {
        use async_nats::jetstream::stream::{DiscardPolicy, RetentionPolicy};
        let c = stream_config();
        assert_eq!(c.name, STREAM);
        assert_eq!(c.retention, RetentionPolicy::Limits);
        assert_eq!(c.discard, DiscardPolicy::Old);
        assert!(c.max_age > std::time::Duration::from_secs(24 * 3600));
        assert!(c.max_bytes > 0 && c.max_messages > 0 && c.max_message_size > 0);
        assert!(c.duplicate_window > std::time::Duration::ZERO);
        assert!(!drifted(&c, &stream_config()));
    }

    #[test]
    fn an_endpoint_consumer_is_filtered_to_its_database_from_its_enable_time() {
        use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};
        let (endpoint, db) = (Uuid::new_v4(), Uuid::new_v4());
        let since = DateTime::from_timestamp(1_700_000_000, 5_000_000).unwrap();
        let c = endpoint_consumer(endpoint, db, since);
        assert_eq!(c.filter_subject, subject(db));
        assert_eq!(
            c.durable_name.as_deref(),
            Some(endpoint_consumer_name(endpoint, since).as_str())
        );
        assert_eq!(c.name, c.durable_name);
        assert!(!c.durable_name.as_deref().unwrap().contains(['.', '*', '>']));
        assert_eq!(c.ack_policy, AckPolicy::Explicit);
        assert_eq!(c.max_deliver, -1);
        let DeliverPolicy::ByStartTime { start_time } = c.deliver_policy else {
            panic!("{:?}", c.deliver_policy)
        };
        assert_eq!(start_time.unix_timestamp_nanos(), 1_700_000_000_005_000_000);
        // A re-enable is a new consumer.
        assert_ne!(
            endpoint_consumer_name(endpoint, since),
            endpoint_consumer_name(endpoint, since + chrono::Duration::seconds(1))
        );
    }

    #[test]
    fn the_test_reply_has_its_wire_shape() {
        let r = TestReply::Sent {
            outcome: AttemptOutcome::HttpError,
            status: Some(500),
            duration_ms: 12,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "result": "sent", "outcome": "http_error", "status": 500, "duration_ms": 12 })
        );
        assert_eq!(
            SUBJECT_TEST.split('.').take(2).collect::<Vec<_>>(),
            ["chelabase", "rpc"]
        );
    }
}
