//! NATS cache invalidation (m9): storage drops what it cached the moment the control plane
//! says it changed, instead of waiting out its cache lifetimes (which stay as the fallback, so
//! with NATS down storage keeps serving on them).
//!
//! It listens as its own NATS user (`storage`, subscribe-only) to:
//! - `chelabase.rt.project.*`: a branch's status changed (a reset or restore ends with one), it
//!   was deleted, or one of its Postgres databases was created or dropped: the branch's
//!   connection info, pools and schema checks go;
//! - `chelabase.storage.events` ([`chelabase_nats::StorageEvent`]): a data key revoked, rotated
//!   or its database list changed (its signed URLs' liveness goes), or a branch's connection
//!   budget changed (as a branch event above);
//! - `chelabase.rt.access.*`: a member's access changed (removed, suspended, a grant): every
//!   cached key liveness goes, since a console URL lives on its member's access.
//!
//! m18: after each object write commits, storage publishes an object event
//! ([`ObjectEvents::emit`], `chelabase_nats::storage_objects`) on the same connection, in the
//! background and retried; one that still fails is logged and counted, never failing the
//! request. Changes made with SQL directly on `storage.objects` publish nothing.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chelabase_auth::internal::DataIdentity;
use chelabase_nats::storage_objects::{
    MAX_CONTENT_TYPE_BYTES, MAX_SUB_BYTES, ObjectActor, ObjectEvent, ObjectEventType, ObjectFrom,
};

use chelabase_nats::rt::{ACCESS, PROJECT, ProjectEvent};
use chelabase_nats::{NatsConfig, SUBJECT_STORAGE_EVENTS, StorageEvent};
use chelabase_pgsession::ConnSource;
use futures_util::StreamExt;
use uuid::Uuid;

use crate::app::AppState;

/// What an event makes storage drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drop {
    /// The branch's connection info, pools and schema checks.
    Branch(Uuid),
    /// One key's cached liveness.
    Key(Uuid),
    /// Every key's cached liveness.
    AllKeys,
}

/// What a message on `subject` makes storage drop (`None`: nothing, or unreadable).
pub fn classify(subject: &str, payload: &[u8]) -> Option<Drop> {
    if subject == SUBJECT_STORAGE_EVENTS {
        return match serde_json::from_slice(payload).ok()? {
            StorageEvent::Key { key_id } => Some(Drop::Key(key_id)),
            StorageEvent::Branch { branch_id } => Some(Drop::Branch(branch_id)),
        };
    }
    if subject.starts_with(&format!("{ACCESS}.")) {
        return Some(Drop::AllKeys);
    }
    if subject.starts_with(&format!("{PROJECT}.")) {
        return match serde_json::from_slice(payload).ok()? {
            ProjectEvent::BranchStatus { branch_id, .. }
            | ProjectEvent::BranchDeleted { branch_id, .. }
            | ProjectEvent::PgDatabaseCreated { branch_id, .. }
            | ProjectEvent::PgDatabaseDropped { branch_id, .. } => Some(Drop::Branch(branch_id)),
            ProjectEvent::BranchCreated { .. }
            | ProjectEvent::DatabaseCreated { .. }
            | ProjectEvent::DatabaseDeleted { .. } => None,
        };
    }
    None
}

/// Drops what `drop` names.
pub async fn apply<S: ConnSource, M: ConnSource>(state: &AppState<S, M>, drop: Drop) {
    match drop {
        Drop::Branch(branch) => state.engine.invalidate(branch).await,
        Drop::Key(key_id) => state.urls.forget_key(key_id),
        Drop::AllKeys => state.urls.forget_all(),
    }
}

/// Where object events go: the stream (through NATS), or a fake in tests.
pub trait EventSink: Send + Sync + 'static {
    fn publish<'a>(&'a self, event: &'a ObjectEvent) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// The stream, on storage's latest NATS connection (until there is one every publish fails:
/// the event is missed and counted).
#[derive(Default)]
pub struct NatsSink {
    js: RwLock<Option<chelabase_nats::JetStreamContext>>,
}

impl EventSink for NatsSink {
    fn publish<'a>(&'a self, event: &'a ObjectEvent) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let js = self.js.read().ok().and_then(|js| js.clone()).ok_or("NATS isn't connected")?;
            chelabase_nats::storage_objects::publish(&js, event).await.map_err(|e| e.to_string())
        })
    }
}

/// What a committed write changed, for its event.
#[derive(Debug, Clone)]
pub struct ObjectChange {
    pub kind: ObjectEventType,
    pub bucket: String,
    pub name: String,
    pub version_id: Option<Uuid>,
    pub size: Option<i64>,
    pub content_type: Option<String>,
    pub from: Option<ObjectFrom>,
}

/// The event of `change`, made by `data` in `at`, now. Every field stays within its bound
/// (`chelabase_nats::storage_objects`), so the stream stores the event: the API refuses a
/// longer name, bucket or declared type, and Postgres bounds the role and database names.
/// The two strings that can still be longer are left out, not cut, since a cut value would
/// name a different user or type: a `sub` claim past [`MAX_SUB_BYTES`] as JSON writes it (it
/// comes from the caller's token), and a stored type past [`MAX_CONTENT_TYPE_BYTES`] (one
/// written with SQL, then copied, moved or deleted through the API). Bucket names come from
/// the API's grammar; a bucket made with SQL outside it is SQL's change, like any other.
pub fn event_of(data: &DataIdentity, at: &crate::engine::Located, change: ObjectChange) -> ObjectEvent {
    ObjectEvent {
        id: Uuid::new_v4(),
        kind: change.kind,
        time: chrono::Utc::now(),
        database_id: at.database_id,
        branch_id: data.branch_id,
        pg_database: at.database.clone(),
        bucket: change.bucket,
        name: change.name,
        version_id: change.version_id,
        size: change.size,
        content_type: change.content_type.filter(|t| json_len(t) <= MAX_CONTENT_TYPE_BYTES),
        actor: ObjectActor {
            role: data.role.clone(),
            sub: data
                .claims
                .get("sub")
                .and_then(|s| s.as_str())
                .filter(|s| json_len(s) <= MAX_SUB_BYTES)
                .map(str::to_string),
        },
        from: change.from,
    }
}

/// `s`'s bytes as JSON writes it, escapes included, without the quotes.
fn json_len(s: &str) -> usize {
    serde_json::to_string(s).map_or(usize::MAX, |j| j.len() - 2)
}

impl<S: ConnSource, M: ConnSource> AppState<S, M> {
    /// Publishes the event of a write that committed (in the background).
    pub(crate) fn emit(&self, data: &DataIdentity, at: &crate::engine::Located, change: ObjectChange) {
        self.events.emit(event_of(data, at, change));
    }
}

/// Publishes object events after their writes commit (see the module docs).
pub struct ObjectEvents {
    sink: Option<Arc<dyn EventSink>>,
    nats: Option<Arc<NatsSink>>,
    /// The pauses between tries: one try more than pauses.
    retries: Vec<Duration>,
    /// Events that couldn't be published.
    pub missed: AtomicU64,
}

impl ObjectEvents {
    /// Through NATS, once [`spawn`] connects.
    pub fn nats() -> Arc<Self> {
        let nats = Arc::new(NatsSink::default());
        Arc::new(Self {
            sink: Some(nats.clone()),
            nats: Some(nats),
            retries: vec![Duration::from_millis(100), Duration::from_millis(500), Duration::from_secs(2)],
            missed: AtomicU64::new(0),
        })
    }

    /// To `sink`, pausing `retries` between tries.
    pub fn with_sink(sink: Arc<dyn EventSink>, retries: Vec<Duration>) -> Arc<Self> {
        Arc::new(Self { sink: Some(sink), nats: None, retries, missed: AtomicU64::new(0) })
    }

    /// Nowhere (unit tests that don't look at events).
    pub fn none() -> Arc<Self> {
        Arc::new(Self { sink: None, nats: None, retries: Vec::new(), missed: AtomicU64::new(0) })
    }

    fn connected(&self, js: chelabase_nats::JetStreamContext) {
        if let Some(nats) = &self.nats
            && let Ok(mut slot) = nats.js.write()
        {
            *slot = Some(js);
        }
    }

    /// Publishes `event` in the background (the request never waits for it).
    pub fn emit(self: &Arc<Self>, event: ObjectEvent) {
        if self.sink.is_none() {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.send(&event).await;
        });
    }

    /// Publishes `event`, trying again after each pause; false (logged and counted) when every
    /// try failed.
    pub async fn send(&self, event: &ObjectEvent) -> bool {
        let Some(sink) = &self.sink else {
            return true;
        };
        let mut pauses = self.retries.iter();
        loop {
            match sink.publish(event).await {
                Ok(()) => return true,
                Err(e) => match pauses.next() {
                    Some(pause) => tokio::time::sleep(*pause).await,
                    None => {
                        let missed = self.missed.fetch_add(1, Ordering::Relaxed) + 1;
                        tracing::warn!(
                            event = %event.id,
                            database = %event.database_id,
                            missed_total = missed,
                            "an object event was not published (no webhook gets it): {e}"
                        );
                        return false;
                    }
                },
            }
        }
    }
}

/// How long to wait before connecting again after NATS refused or couldn't be reached.
const RETRY: Duration = Duration::from_secs(5);

/// Listens in the background for as long as the process runs: connects (again every
/// [`RETRY`] until NATS answers; meanwhile the cache lifetimes apply), subscribes and applies
/// each event. The client reconnects on its own once connected.
pub fn spawn<S: ConnSource, M: ConnSource>(state: Arc<AppState<S, M>>, config: NatsConfig) {
    tokio::spawn(async move {
        loop {
            match listen(&state, &config).await {
                Ok(()) => tracing::warn!("the NATS subscriptions ended; subscribing again"),
                Err(e) => {
                    tracing::warn!("NATS cache events unavailable (the cache lifetimes apply): {e}")
                }
            }
            tokio::time::sleep(RETRY).await;
        }
    });
}

async fn listen<S: ConnSource, M: ConnSource>(
    state: &AppState<S, M>,
    config: &NatsConfig,
) -> anyhow::Result<()> {
    let client = chelabase_nats::connect(config).await?;
    state.events.connected(chelabase_nats::storage_objects::jetstream(client.clone()));
    let subjects = [
        format!("{PROJECT}.*"),
        format!("{ACCESS}.*"),
        SUBJECT_STORAGE_EVENTS.to_string(),
    ];
    let mut subscriptions = Vec::new();
    for subject in subjects {
        subscriptions.push(client.subscribe(subject).await?);
    }
    tracing::info!("listening for NATS cache events");
    let mut messages = futures_util::stream::select_all(subscriptions);
    while let Some(message) = messages.next().await {
        if let Some(drop) = classify(message.subject.as_str(), &message.payload) {
            tracing::debug!(?drop, "NATS cache event");
            apply(state, drop).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// A sink that fails its first `fail` publishes, keeping what it published.
    struct Flaky {
        fail: Mutex<usize>,
        tries: AtomicU64,
        published: Mutex<Vec<ObjectEvent>>,
    }

    impl Flaky {
        fn new(fail: usize) -> Arc<Self> {
            Arc::new(Self { fail: Mutex::new(fail), tries: AtomicU64::new(0), published: Mutex::new(Vec::new()) })
        }
    }

    impl EventSink for Flaky {
        fn publish<'a>(&'a self, event: &'a ObjectEvent) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            self.tries.fetch_add(1, Ordering::SeqCst);
            let mut fail = self.fail.lock().unwrap();
            let out = if *fail > 0 {
                *fail -= 1;
                Err("nats down".to_string())
            } else {
                self.published.lock().unwrap().push(event.clone());
                Ok(())
            };
            Box::pin(std::future::ready(out))
        }
    }

    fn data(claims: serde_json::Value) -> DataIdentity {
        serde_json::from_value(json!({
            "branch_id": Uuid::from_u128(2), "key_id": Uuid::from_u128(3), "kind": "secret",
            "role": "app_writer", "claims": claims
        }))
        .unwrap()
    }

    fn located() -> crate::engine::Located {
        crate::engine::Located { database: "app".into(), database_id: Uuid::from_u128(1) }
    }

    fn change(kind: ObjectEventType) -> ObjectChange {
        ObjectChange {
            kind,
            bucket: "avatars".into(),
            name: "a.png".into(),
            version_id: Some(Uuid::from_u128(9)),
            size: Some(3),
            content_type: Some("image/png".into()),
            from: None,
        }
    }

    /// An event names its database, branch, Postgres database, object and actor (role and a
    /// string `sub`, nothing else of the claims).
    #[test]
    fn an_event_carries_the_write_and_its_actor() {
        let e = event_of(&data(json!({ "sub": "user-1", "email": "a@b.c" })), &located(), change(ObjectEventType::Created));
        assert_eq!((e.database_id, e.branch_id), (Uuid::from_u128(1), Uuid::from_u128(2)));
        assert_eq!((e.pg_database.as_str(), e.bucket.as_str(), e.name.as_str()), ("app", "avatars", "a.png"));
        assert_eq!((e.version_id, e.size, e.content_type.as_deref()), (Some(Uuid::from_u128(9)), Some(3), Some("image/png")));
        assert_eq!(e.actor, ObjectActor { role: "app_writer".into(), sub: Some("user-1".into()) });
        assert!(!serde_json::to_string(&e).unwrap().contains("a@b.c"));
        assert!((chrono::Utc::now() - e.time).num_seconds().abs() < 5);
        let anon = event_of(&data(json!({ "sub": 7 })), &located(), change(ObjectEventType::Deleted));
        assert_eq!(anon.actor.sub, None);
        assert_ne!(anon.id, e.id);
    }

    /// What the stream stores of `e`: its JSON and the `Nats-Msg-Id` header block.
    fn wire_bytes(e: &ObjectEvent) -> usize {
        let header = format!("NATS/1.0\r\nNats-Msg-Id: {}\r\n\r\n", e.id).len();
        serde_json::to_vec(e).unwrap().len() + header
    }

    /// Re-audit B4 (round 3): every write the API accepts makes an event the stream stores.
    /// The audit's case (a 255-byte type, a worst-case name moved, a long `sub`) fits
    /// `max_message_size`; a `sub` or stored type past its bound is left out, never cut.
    #[test]
    fn an_accepted_write_makes_an_event_the_stream_stores() {
        use chelabase_nats::storage_objects::{MAX_EVENT_BYTES, MAX_SUB_BYTES, PG_NAME_BYTES};
        use crate::names::{MAX_CONTENT_TYPE_BYTES, MAX_NAME_BYTES, check_content_type, check_object_name};
        let ct = format!("image/png; x={}", "a".repeat(MAX_CONTENT_TYPE_BYTES - "image/png; x=".len()));
        check_content_type(&ct).expect("storage accepts the type");
        let name = "\"".repeat(MAX_NAME_BYTES);
        check_object_name(&name).expect("storage accepts the name");
        let worst_change = || ObjectChange {
            kind: ObjectEventType::Moved,
            bucket: "b".repeat(63),
            name: name.clone(),
            version_id: Some(Uuid::max()),
            size: Some(i64::MIN),
            content_type: Some(ct.clone()),
            from: Some(ObjectFrom { bucket: "b".repeat(63), name: name.clone() }),
        };
        let worst_at = crate::engine::Located { database: "\u{1}".repeat(PG_NAME_BYTES), database_id: Uuid::max() };
        let worst_data = |sub: &str| {
            let mut d = data(json!({ "sub": sub }));
            d.role = "\u{1}".repeat(PG_NAME_BYTES);
            d
        };
        for (sub, kept) in [
            ("a".repeat(MAX_SUB_BYTES), true),
            ("\"".repeat(MAX_SUB_BYTES / 2), true),
            ("\"".repeat(MAX_SUB_BYTES), false),
            ("\u{1}".repeat(MAX_SUB_BYTES), false),
            ("a".repeat(MAX_SUB_BYTES + 1), false),
            ("a".repeat(6000), false),
        ] {
            let e = event_of(&worst_data(&sub), &worst_at, worst_change());
            let n = wire_bytes(&e);
            assert!(n as i64 <= MAX_EVENT_BYTES, "event {n} B > max_message_size {MAX_EVENT_BYTES} B: refused, never delivered");
            assert_eq!(e.actor.sub.as_deref(), kept.then_some(sub.as_str()), "{} bytes of sub", sub.len());
            assert_eq!(e.content_type.as_deref(), Some(ct.as_str()));
        }
        // A stored type past the bound (written with SQL, then copied, moved or deleted
        // through the API) is left out.
        let mut sql_typed = worst_change();
        sql_typed.content_type = Some(format!("image/png; x={}", "a".repeat(6000)));
        let e = event_of(&worst_data("u"), &worst_at, sql_typed);
        assert!(wire_bytes(&e) as i64 <= MAX_EVENT_BYTES);
        assert_eq!(e.content_type, None);
    }

    /// A publish that fails is tried again; one that keeps failing is counted and never
    /// panics or errors; the next event publishes once NATS is back (AC 6).
    #[tokio::test]
    async fn a_failed_publish_is_retried_then_counted_and_later_events_resume() {
        let sink = Flaky::new(2);
        let events = ObjectEvents::with_sink(sink.clone(), vec![Duration::ZERO; 3]);
        let first = event_of(&data(json!({})), &located(), change(ObjectEventType::Created));
        assert!(events.send(&first).await);
        assert_eq!(sink.tries.load(Ordering::SeqCst), 3);
        assert_eq!(*sink.published.lock().unwrap(), std::slice::from_ref(&first));
        assert_eq!(events.missed.load(Ordering::SeqCst), 0);

        let down = Flaky::new(usize::MAX);
        let events = ObjectEvents::with_sink(down.clone(), vec![Duration::ZERO; 3]);
        assert!(!events.send(&first).await);
        assert_eq!(down.tries.load(Ordering::SeqCst), 4, "one try more than pauses");
        assert_eq!(events.missed.load(Ordering::SeqCst), 1);
        *down.fail.lock().unwrap() = 0;
        let later = event_of(&data(json!({})), &located(), change(ObjectEventType::Updated));
        assert!(events.send(&later).await);
        assert_eq!(*down.published.lock().unwrap(), [later]);
        assert_eq!(events.missed.load(Ordering::SeqCst), 1);

        // Without a connection (NATS never reached), an event is missed and counted.
        let unconnected = ObjectEvents::nats();
        let unconnected = ObjectEvents { retries: vec![Duration::ZERO], ..Arc::into_inner(unconnected).unwrap() };
        assert!(!unconnected.send(&first).await);
        assert_eq!(unconnected.missed.load(Ordering::SeqCst), 1);
    }

    /// Each subject and event maps to what it drops; anything else drops nothing.
    #[test]
    fn events_map_to_what_they_drop() {
        let (b, d, k) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let project = format!("{PROJECT}.{}", Uuid::new_v4());
        let ev = |v: serde_json::Value| serde_json::to_vec(&v).unwrap();
        for (subject, payload, want) in [
            (
                project.clone(),
                ev(
                    json!({ "event": "branch.status", "database_id": d, "branch_id": b, "status": "ready" }),
                ),
                Some(Drop::Branch(b)),
            ),
            (
                project.clone(),
                ev(json!({ "event": "branch.deleted", "database_id": d, "branch_id": b })),
                Some(Drop::Branch(b)),
            ),
            (
                project.clone(),
                ev(
                    json!({ "event": "pg_database.dropped", "database_id": d, "branch_id": b, "name": "app" }),
                ),
                Some(Drop::Branch(b)),
            ),
            (
                project.clone(),
                ev(
                    json!({ "event": "pg_database.created", "database_id": d, "branch_id": b, "name": "app" }),
                ),
                Some(Drop::Branch(b)),
            ),
            (
                project.clone(),
                ev(json!({ "event": "branch.created", "database_id": d, "branch_id": b })),
                None,
            ),
            (
                project.clone(),
                ev(json!({ "event": "database.deleted", "database_id": d })),
                None,
            ),
            (project.clone(), b"not json".to_vec(), None),
            (
                SUBJECT_STORAGE_EVENTS.to_string(),
                ev(json!({ "event": "key", "key_id": k })),
                Some(Drop::Key(k)),
            ),
            (
                SUBJECT_STORAGE_EVENTS.to_string(),
                ev(json!({ "event": "branch", "branch_id": b })),
                Some(Drop::Branch(b)),
            ),
            (
                SUBJECT_STORAGE_EVENTS.to_string(),
                ev(json!({ "event": "other" })),
                None,
            ),
            (
                format!("{ACCESS}.{}", Uuid::new_v4()),
                ev(json!({ "tenant_id": d, "member_sub": null })),
                Some(Drop::AllKeys),
            ),
            ("chelabase.rt.rows.x".to_string(), ev(json!({})), None),
        ] {
            assert_eq!(
                classify(&subject, &payload),
                want,
                "{subject} {}",
                String::from_utf8_lossy(&payload)
            );
        }
    }
}
