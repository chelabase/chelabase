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

/// The longest object name, in bytes of UTF-8 (the storage service refuses a longer one). JSON
/// can double it (`"` and `\` are escaped; control characters are refused).
pub const MAX_NAME_BYTES: usize = 1024;

/// The longest bucket name: `[a-z0-9._-]`, nothing JSON escapes.
pub const MAX_BUCKET_BYTES: usize = 63;

/// The longest `Content-Type` an upload may declare (the storage service answers 400 above
/// it). Its grammar is RFC 9110 tokens and spaces around `;` (never a tab), so JSON escapes
/// nothing in it and the byte count is the JSON length; an event carries a stored type longer
/// than this (written with SQL) as absent.
pub const MAX_CONTENT_TYPE_BYTES: usize = 255;

/// The longest `actor.sub` an event carries, counted as JSON writes it (escapes included): the
/// claim comes from the caller's token, so a longer one is left out rather than cut, since a
/// cut `sub` would name someone else.
pub const MAX_SUB_BYTES: usize = 255;

/// A Postgres name's longest (`NAMEDATALEN` − 1): the role and the Postgres database. Such a
/// name may hold any character but NUL, so JSON can write each byte as six (`\u0001`).
pub const PG_NAME_BYTES: usize = 63;

/// Events the stream keeps for one database (one subject). Past this, the database's own
/// oldest events are dropped, pending or not: about 1.1 events a second sustained over
/// [`RETENTION`] while an endpoint is down. Webhooks don't count those drops; a function's
/// storage trigger does, through its watcher ([`fn_trigger_watcher`], edge functions m14).
pub const MAX_EVENTS_PER_DATABASE: i64 = 100_000;

/// The largest event the stream stores, JSON payload and `Nats-Msg-Id` header together. The
/// stream refuses a bigger one (`max_message_size`). The worst case the field bounds allow (a
/// [`MAX_NAME_BYTES`] name of `"`, which JSON doubles, twice in a move; every other field at
/// its bound) is under 6 KiB, so every write the API accepts is stored.
pub const MAX_EVENT_BYTES: i64 = 6 * 1024;

/// What JetStream's file store keeps for each message beside its payload and headers: the
/// record header (22 bytes), the header length (4), the checksum (8) and the subject.
pub const STORED_OVERHEAD_BYTES: i64 = 22 + 4 + 8 + (SUBJECT_PREFIX.len() + 1 + 36) as i64;

/// The most one database at its cap can take of the stream: [`MAX_EVENTS_PER_DATABASE`]
/// events of [`MAX_EVENT_BYTES`] each (about 595 MiB).
pub const DATABASE_BYTES_AT_CAP: i64 =
    (MAX_EVENT_BYTES + STORED_OVERHEAD_BYTES) * MAX_EVENTS_PER_DATABASE;

/// Room left for every other database's traffic while one is fully backed up: about 6.8
/// million ordinary events (about 630 bytes stored each), or about six more databases backed
/// up with worst-case events.
pub const OTHER_TRAFFIC_BYTES: i64 = 4 * 1024 * 1024 * 1024;

/// The stream's byte limit (about 4.6 GiB): one database at its cap plus everyone else's
/// traffic, so one busy database never pushes out another's pending events. JetStream reserves
/// this much of the NATS server's file store when the stream is created; the disk fills only
/// as events arrive.
pub const STREAM_MAX_BYTES: i64 = DATABASE_BYTES_AT_CAP + OTHER_TRAFFIC_BYTES;

/// The smallest event the stream can hold, JSON payload and `Nats-Msg-Id` header together:
/// one-byte names, nothing optional.
pub const MIN_EVENT_BYTES: i64 = 380;

/// Events the whole stream keeps: what [`STREAM_MAX_BYTES`] holds of the smallest events
/// (about 10.4 million), so the count never binds before the bytes and the byte budget is what
/// every database shares. It costs no disk: JetStream reserves by bytes.
pub const MAX_EVENTS: i64 = STREAM_MAX_BYTES / (MIN_EVENT_BYTES + STORED_OVERHEAD_BYTES);

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
/// consumed away), [`RETENTION`], bounded in bytes ([`STREAM_MAX_BYTES`]) with the oldest
/// dropped first (and in count, [`MAX_EVENTS`], which never binds before the bytes), each database also bounded on its own
/// ([`MAX_EVENTS_PER_DATABASE`]: beyond it that database's oldest events drop, uncounted),
/// each event bounded ([`MAX_EVENT_BYTES`]), and a duplicate window so a retried publish (same
/// `Nats-Msg-Id`, the event id) is stored once.
pub fn stream_config() -> async_nats::jetstream::stream::Config {
    use async_nats::jetstream::stream::{Config, DiscardPolicy, RetentionPolicy, StorageType};
    Config {
        name: STREAM.to_string(),
        subjects: vec![format!("{SUBJECT_PREFIX}.*")],
        retention: RetentionPolicy::Limits,
        storage: StorageType::File,
        max_age: RETENTION,
        max_messages: MAX_EVENTS,
        max_messages_per_subject: MAX_EVENTS_PER_DATABASE,
        max_bytes: STREAM_MAX_BYTES,
        max_message_size: MAX_EVENT_BYTES as i32,
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

/// Edge functions m14: the durable consumer name of a storage trigger enabled since `since`
/// (`fn-trigger-<id>-<enabled_since_ms>`): a re-enabled trigger gets a new consumer starting
/// then, so nothing from the disabled window is replayed.
pub fn fn_trigger_consumer_name(trigger: Uuid, since: DateTime<Utc>) -> String {
    format!("fn-trigger-{trigger}-{}", since.timestamp_millis())
}

/// A storage trigger's consumer: [`endpoint_consumer`]'s shape (durable, filtered to the
/// database, from the enable time, explicit acks, never given up on, orphans expiring after
/// [`CONSUMER_INACTIVE`]) under the trigger's own name, with `ack_wait` longer than one
/// invocation may take.
pub fn fn_trigger_consumer(
    trigger: Uuid,
    database_id: Uuid,
    since: DateTime<Utc>,
    ack_wait: std::time::Duration,
) -> async_nats::jetstream::consumer::pull::Config {
    let name = fn_trigger_consumer_name(trigger, since);
    async_nats::jetstream::consumer::pull::Config {
        durable_name: Some(name.clone()),
        name: Some(name),
        ack_wait,
        ..endpoint_consumer(trigger, database_id, since)
    }
}

/// How long a watcher nobody pulls from lives on.
pub const WATCHER_INACTIVE: std::time::Duration = std::time::Duration::from_secs(300);

/// A storage trigger's watcher: an ephemeral consumer (a new name per start) on the same
/// subject from stream sequence `from` (at least 1), never acked. The worker reads it at the
/// head of the subject to see every event arrive (and whether it is the trigger's), so it can
/// count the ones the per-database cap ([`MAX_EVENTS_PER_DATABASE`]) evicts before the
/// trigger's consumer reaches them.
pub fn fn_trigger_watcher(
    trigger: Uuid,
    database_id: Uuid,
    from: u64,
) -> async_nats::jetstream::consumer::pull::Config {
    use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, pull};
    pull::Config {
        name: Some(format!("fn-watch-{trigger}-{}", Uuid::new_v4().simple())),
        filter_subject: subject(database_id),
        deliver_policy: DeliverPolicy::ByStartSequence {
            start_sequence: from.max(1),
        },
        ack_policy: AckPolicy::None,
        inactive_threshold: WATCHER_INACTIVE,
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
        let mut have = want.clone();
        have.max_bytes = 512 * 1024 * 1024;
        assert!(
            drifted(&have, &want),
            "a stream with the old byte limit is updated"
        );
        let mut have = want.clone();
        have.max_messages = 1_000_000;
        assert!(
            drifted(&have, &want),
            "a stream with the old event count limit is updated"
        );
        let mut have = want.clone();
        have.max_message_size = 64 * 1024;
        assert!(
            drifted(&have, &want),
            "a stream with the old event size limit is updated"
        );
    }

    /// The worst-case event, built from the field bounds: a [`MAX_NAME_BYTES`] name of `"`
    /// (JSON doubles it) twice in a move, [`MAX_BUCKET_BYTES`] buckets, a
    /// [`MAX_CONTENT_TYPE_BYTES`] type, a `sub` of [`MAX_SUB_BYTES`] as written, Postgres names
    /// of [`PG_NAME_BYTES`] control characters (six bytes each in JSON), the longest time and
    /// size. Growing any bound grows this event.
    fn worst_event() -> ObjectEvent {
        let quotes = "\"".repeat(MAX_NAME_BYTES);
        ObjectEvent {
            id: Uuid::max(),
            kind: ObjectEventType::Moved,
            time: DateTime::from_timestamp(253_402_300_799, 999_999_999).unwrap(),
            database_id: Uuid::max(),
            branch_id: Uuid::max(),
            pg_database: "\u{1}".repeat(PG_NAME_BYTES),
            bucket: "b".repeat(MAX_BUCKET_BYTES),
            name: quotes.clone(),
            version_id: Some(Uuid::max()),
            size: Some(i64::MIN),
            content_type: Some("t".repeat(MAX_CONTENT_TYPE_BYTES)),
            actor: ObjectActor {
                role: "\u{1}".repeat(PG_NAME_BYTES),
                sub: Some("s".repeat(MAX_SUB_BYTES)),
            },
            from: Some(ObjectFrom {
                bucket: "b".repeat(MAX_BUCKET_BYTES),
                name: quotes,
            }),
        }
    }

    /// The smallest event: one-byte names, a delete with nothing optional, a whole-second
    /// time.
    fn least_event() -> ObjectEvent {
        ObjectEvent {
            id: Uuid::nil(),
            kind: ObjectEventType::Moved,
            time: DateTime::from_timestamp(0, 0).unwrap(),
            database_id: Uuid::nil(),
            branch_id: Uuid::nil(),
            pg_database: "a".into(),
            bucket: "b".into(),
            name: "c".into(),
            version_id: None,
            size: None,
            content_type: None,
            actor: ObjectActor {
                role: "r".into(),
                sub: None,
            },
            from: None,
        }
    }

    /// What the stream counts of `e`: its JSON and the `Nats-Msg-Id` header block.
    fn wire_bytes(e: &ObjectEvent) -> i64 {
        let header = format!("NATS/1.0\r\nNats-Msg-Id: {}\r\n\r\n", e.id).len();
        (serde_json::to_vec(e).unwrap().len() + header) as i64
    }

    /// Re-audit B4 (round 3): the worst-case event the bounds allow is stored.
    #[test]
    fn the_worst_case_event_fits_the_event_size_limit() {
        let worst = wire_bytes(&worst_event());
        assert!(
            worst <= stream_config().max_message_size as i64,
            "the worst-case event ({worst} B) is stored"
        );
    }

    /// Re-audit S2 (round 3): the stream's event count never binds before its bytes, even
    /// with every event as small as an event can be, so the byte budget is what everyone
    /// shares.
    #[test]
    fn the_event_count_never_binds_before_the_bytes() {
        let c = stream_config();
        let least = ObjectEventType::ALL
            .map(|kind| {
                wire_bytes(&ObjectEvent {
                    kind,
                    ..least_event()
                })
            })
            .into_iter()
            .min()
            .unwrap();
        assert!(MIN_EVENT_BYTES <= least, "the smallest event is {least} B");
        let stored = least + STORED_OVERHEAD_BYTES;
        assert!(
            c.max_messages.saturating_mul(stored) >= c.max_bytes,
            "{} events of {stored} B fill only {} of {} B",
            c.max_messages,
            c.max_messages * stored,
            c.max_bytes
        );
    }

    /// The stream's bytes hold one database at its cap with worst-case events and still leave
    /// room for every other database's ordinary traffic.
    #[test]
    fn one_database_at_its_cap_fits_the_stream_with_room_for_the_rest() {
        let c = stream_config();
        let worst = worst_event();
        let stored =
            |e: &ObjectEvent| wire_bytes(e) + 22 + 4 + 8 + subject(e.database_id).len() as i64;
        assert_eq!(stored(&worst) - wire_bytes(&worst), STORED_OVERHEAD_BYTES);
        let at_cap = stored(&worst) * MAX_EVENTS_PER_DATABASE;
        assert!(at_cap <= DATABASE_BYTES_AT_CAP);
        let ordinary = ObjectEvent {
            name: "avatars/user-1/photo.png".into(),
            pg_database: "app".into(),
            bucket: "avatars".into(),
            actor: ObjectActor {
                role: "authenticated".into(),
                sub: Some(Uuid::max().to_string()),
            },
            content_type: Some("image/png".into()),
            from: None,
            kind: ObjectEventType::Created,
            ..worst
        };
        // About 6.8 million ordinary events: over sixty databases' worth at their cap.
        let others = OTHER_TRAFFIC_BYTES / stored(&ordinary);
        assert!(
            others >= 60 * MAX_EVENTS_PER_DATABASE,
            "room for {others} ordinary events"
        );
        assert!(
            at_cap + OTHER_TRAFFIC_BYTES <= c.max_bytes,
            "one database at its cap ({at_cap} B) plus everyone else's traffic fit in \
             max_bytes ({} B)",
            c.max_bytes
        );
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

    /// Edge functions m14: a storage trigger's consumer is its own (not an endpoint's), filtered
    /// to its database from its enable time, expires like an endpoint's when orphaned, and
    /// waits longer than an invocation for an ack.
    #[test]
    fn a_function_trigger_consumer_starts_at_its_enable_time() {
        use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};
        let (trigger, db) = (Uuid::new_v4(), Uuid::new_v4());
        let since = DateTime::from_timestamp(1_700_000_000, 7_000_000).unwrap();
        let ack_wait = std::time::Duration::from_secs(90);
        let c = fn_trigger_consumer(trigger, db, since, ack_wait);
        assert_eq!(
            c.durable_name.as_deref(),
            Some(format!("fn-trigger-{trigger}-1700000000007").as_str())
        );
        assert_eq!(c.name, c.durable_name);
        assert_eq!(
            fn_trigger_consumer_name(trigger, since),
            format!("fn-trigger-{trigger}-1700000000007")
        );
        assert_ne!(c.durable_name, endpoint_consumer(trigger, db, since).durable_name);
        assert_eq!(c.filter_subject, subject(db));
        assert_eq!(c.ack_policy, AckPolicy::Explicit);
        assert_eq!(c.ack_wait, ack_wait);
        assert_eq!(c.max_deliver, -1);
        assert_eq!(c.inactive_threshold, endpoint_consumer(trigger, db, since).inactive_threshold);
        let DeliverPolicy::ByStartTime { start_time } = c.deliver_policy else {
            panic!("{:?}", c.deliver_policy)
        };
        assert_eq!(start_time.unix_timestamp_nanos(), 1_700_000_000_007_000_000);
        assert_ne!(
            fn_trigger_consumer_name(trigger, since),
            fn_trigger_consumer_name(trigger, since + chrono::Duration::milliseconds(1)),
            "a re-enable is a new consumer"
        );
    }

    /// Its watcher sees every event of the database from a stream sequence on, never acked, and
    /// is gone soon after nobody pulls from it.
    #[test]
    fn a_function_trigger_watcher_reads_headers_from_a_sequence() {
        use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};
        let (trigger, db) = (Uuid::new_v4(), Uuid::new_v4());
        let w = fn_trigger_watcher(trigger, db, 42);
        assert!(w.durable_name.is_none());
        let name = w.name.clone().unwrap();
        assert!(name.starts_with(&format!("fn-watch-{trigger}-")), "{name}");
        assert!(!name.contains(['.', '*', '>']));
        assert_ne!(name, fn_trigger_watcher(trigger, db, 42).name.unwrap(), "one per start");
        assert_eq!(w.filter_subject, subject(db));
        assert!(!w.headers_only, "the payload says whether it is the trigger's");
        assert_eq!(w.ack_policy, AckPolicy::None);
        assert_eq!(w.deliver_policy, DeliverPolicy::ByStartSequence { start_sequence: 42 });
        assert!(w.inactive_threshold <= std::time::Duration::from_secs(600));
        assert_eq!(
            fn_trigger_watcher(trigger, db, 0).deliver_policy,
            DeliverPolicy::ByStartSequence { start_sequence: 1 }
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
