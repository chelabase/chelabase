//! Leases (spec §1, §3.3; R17, W12, S4, S25, S29): one key per (branch, Postgres database) in
//! `CHELABASE_RT_LEASES`; its holder runs that database's decoder.
//!
//! - S4: realtime creates both buckets at startup with `history 1`, `max_age 10 s`, memory
//!   storage and `limit_markers 5 s` (an expired revision leaves a delete marker, so watchers see
//!   it). No per-key TTL: every revision expires 10 s after it's written; a renewal is a new
//!   revision.
//! - Create with `create` (create-if-absent), renew with `update(rev)` every [`RENEW_EVERY`],
//!   release with `delete_expect_revision(rev)` (never a blind delete).
//! - Fencing (W12, S29): a holder stops decoding at its local monotonic deadline, the start of its
//!   last successful write + [`TTL`] − [`FENCE_MARGIN`], whatever an in-flight renewal does; a
//!   fenced holder stops renewing and lets the key expire.
//! - JetStream calls time out after [`JS_TIMEOUT`] (below the renewal period).
//! - A holder stops [`IDLE_AFTER`] after the last watching heartbeat, counting from when it took
//!   the lease (S29); watchers heartbeat every [`HEARTBEAT`].

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_nats::jetstream::kv::{CreateErrorKind, Operation, UpdateErrorKind};
use async_trait::async_trait;
use bytes::Bytes;
use chelabase_nats::rt::{LEASES_BUCKET, SLOTS_BUCKET};
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

use crate::bus::subject_matches;

pub const TTL: Duration = Duration::from_secs(10);
pub const RENEW_EVERY: Duration = Duration::from_secs(3);
pub const FENCE_MARGIN: Duration = Duration::from_secs(3);
pub const JS_TIMEOUT: Duration = Duration::from_secs(2);
pub const MARKER_TTL: Duration = Duration::from_secs(5);
pub const IDLE_AFTER: Duration = Duration::from_secs(30);
pub const HEARTBEAT: Duration = Duration::from_secs(5);

/// `<branch_id>.<db_oid>` (canonical hyphenated UUID).
pub fn lease_key(branch_id: Uuid, db_oid: u32) -> String {
    format!("{}.{db_oid}", branch_id.hyphenated())
}

/// S4's bucket settings.
pub fn bucket_config(bucket: &str) -> async_nats::jetstream::kv::Config {
    async_nats::jetstream::kv::Config {
        bucket: bucket.to_string(),
        history: 1,
        max_age: TTL,
        storage: async_nats::jetstream::stream::StorageType::Memory,
        limit_markers: Some(MARKER_TTL),
        ..Default::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KvError {
    /// `create` of a key that has a live revision.
    #[error("the key exists")]
    Exists,
    /// `update`/`delete_expect` at a revision that isn't the key's last.
    #[error("wrong revision")]
    WrongRevision,
    #[error("kv unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvEvent {
    Put {
        key: String,
        revision: u64,
    },
    /// A delete, a purge or an expiry (delete marker).
    Deleted {
        key: String,
    },
}

pub type KvWatch = Pin<Box<dyn Stream<Item = KvEvent> + Send>>;

/// The two KV operations leases and slot indexes need.
#[async_trait]
pub trait KvStore: Send + Sync + 'static {
    /// Create-if-absent: the new revision.
    async fn create(&self, key: &str, value: Bytes) -> Result<u64, KvError>;
    /// Compare-and-set at `revision`: the new revision.
    async fn update(&self, key: &str, value: Bytes, revision: u64) -> Result<u64, KvError>;
    /// Delete at `revision` only.
    async fn delete_expect(&self, key: &str, revision: u64) -> Result<(), KvError>;
    /// The key's live value and revision.
    async fn get(&self, key: &str) -> Result<Option<(Bytes, u64)>, KvError>;
    /// Changes of keys matching `key` (NATS wildcards) from now on.
    async fn watch(&self, key: &str) -> Result<KvWatch, KvError>;
}

/// Both buckets, created (or updated to S4's settings) at startup.
pub struct Buckets {
    pub leases: NatsKv,
    pub slots: NatsKv,
}

pub async fn ensure_buckets(js: &async_nats::jetstream::Context) -> Result<Buckets, KvError> {
    let mut stores = Vec::with_capacity(2);
    for bucket in [LEASES_BUCKET, SLOTS_BUCKET] {
        let store = tokio::time::timeout(
            JS_TIMEOUT * 5,
            js.create_or_update_key_value(bucket_config(bucket)),
        )
        .await
        .map_err(|_| KvError::Unavailable(format!("creating {bucket} timed out")))?
        .map_err(|e| KvError::Unavailable(format!("{bucket}: {e}")))?;
        stores.push(NatsKv::new(store));
    }
    let slots = stores.pop().expect("two buckets");
    let leases = stores.pop().expect("two buckets");
    Ok(Buckets { leases, slots })
}

/// A JetStream KV bucket.
pub struct NatsKv {
    store: async_nats::jetstream::kv::Store,
}

impl NatsKv {
    pub fn new(store: async_nats::jetstream::kv::Store) -> Self {
        Self { store }
    }
}

/// `f` within [`JS_TIMEOUT`].
async fn bounded<T>(
    f: impl std::future::Future<Output = Result<T, KvError>>,
) -> Result<T, KvError> {
    tokio::time::timeout(JS_TIMEOUT, f)
        .await
        .map_err(|_| KvError::Unavailable("JetStream timed out".into()))?
}

#[async_trait]
impl KvStore for NatsKv {
    async fn create(&self, key: &str, value: Bytes) -> Result<u64, KvError> {
        bounded(async {
            self.store
                .create(key, value)
                .await
                .map_err(|e| match e.kind() {
                    CreateErrorKind::AlreadyExists => KvError::Exists,
                    _ => KvError::Unavailable(e.to_string()),
                })
        })
        .await
    }
    async fn update(&self, key: &str, value: Bytes, revision: u64) -> Result<u64, KvError> {
        bounded(async {
            self.store
                .update(key, value, revision)
                .await
                .map_err(|e| match e.kind() {
                    UpdateErrorKind::WrongLastRevision => KvError::WrongRevision,
                    _ => KvError::Unavailable(e.to_string()),
                })
        })
        .await
    }
    async fn delete_expect(&self, key: &str, revision: u64) -> Result<(), KvError> {
        bounded(async {
            self.store
                .delete_expect_revision(key, Some(revision))
                .await
                .map_err(|e| match e.kind() {
                    UpdateErrorKind::WrongLastRevision => KvError::WrongRevision,
                    _ => KvError::Unavailable(e.to_string()),
                })
        })
        .await
    }
    async fn get(&self, key: &str) -> Result<Option<(Bytes, u64)>, KvError> {
        bounded(async {
            match self.store.entry(key).await {
                Ok(Some(e)) if e.operation == Operation::Put => Ok(Some((e.value, e.revision))),
                Ok(_) => Ok(None),
                Err(e) => Err(KvError::Unavailable(e.to_string())),
            }
        })
        .await
    }
    async fn watch(&self, key: &str) -> Result<KvWatch, KvError> {
        let watch = bounded(async {
            self.store
                .watch(key)
                .await
                .map_err(|e| KvError::Unavailable(e.to_string()))
        })
        .await?;
        Ok(Box::pin(watch.filter_map(|entry| async move {
            let entry = entry.ok()?;
            Some(match entry.operation {
                Operation::Put => KvEvent::Put {
                    key: entry.key,
                    revision: entry.revision,
                },
                Operation::Delete | Operation::Purge => KvEvent::Deleted { key: entry.key },
            })
        })))
    }
}

struct MemEntry {
    value: Bytes,
    revision: u64,
    written: Instant,
}

#[derive(Default)]
struct MemState {
    entries: HashMap<String, MemEntry>,
    revision: u64,
    watchers: Vec<(String, mpsc::UnboundedSender<KvEvent>)>,
}

impl MemState {
    fn notify(&mut self, ev: &KvEvent) {
        let key = match ev {
            KvEvent::Put { key, .. } | KvEvent::Deleted { key } => key.clone(),
        };
        self.watchers
            .retain(|(pattern, tx)| !subject_matches(pattern, &key) || tx.send(ev.clone()).is_ok());
    }
    /// Drops `key` when its revision is past `max_age` at `now` (a watcher sees the marker).
    fn expire(&mut self, key: &str, now: Instant, max_age: Duration) {
        if self
            .entries
            .get(key)
            .is_some_and(|e| now >= e.written + max_age)
        {
            self.entries.remove(key);
            self.notify(&KvEvent::Deleted {
                key: key.to_string(),
            });
        }
    }
    fn live(&mut self, key: &str, now: Instant, max_age: Duration) -> Option<&MemEntry> {
        self.expire(key, now, max_age);
        self.entries.get(key)
    }
}

/// An in-process bucket for tests with the S4 semantics on tokio's clock: every revision
/// expires `max_age` after it's written and then reads as absent, and watchers get
/// [`KvEvent::Deleted`] for it (the delete marker). While [`MemoryKv::set_down`] is on, every
/// call fails with `Unavailable`.
pub struct MemoryKv {
    max_age: Duration,
    state: Mutex<MemState>,
    down: AtomicBool,
    me: Weak<MemoryKv>,
}

impl MemoryKv {
    pub fn new(max_age: Duration) -> std::sync::Arc<Self> {
        Arc::new_cyclic(|me| Self {
            max_age,
            state: Mutex::new(MemState::default()),
            down: AtomicBool::new(false),
            me: me.clone(),
        })
    }
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
    fn up(&self) -> Result<(), KvError> {
        if self.down.load(Ordering::SeqCst) {
            Err(KvError::Unavailable("disconnected".into()))
        } else {
            Ok(())
        }
    }
    /// Writes a new revision of `key` and schedules its expiry.
    fn write(&self, state: &mut MemState, key: &str, value: Bytes) -> u64 {
        state.revision += 1;
        let revision = state.revision;
        let written = Instant::now();
        state.entries.insert(
            key.to_string(),
            MemEntry {
                value,
                revision,
                written,
            },
        );
        state.notify(&KvEvent::Put {
            key: key.to_string(),
            revision,
        });
        let me = self.me.clone();
        let key = key.to_string();
        let max_age = self.max_age;
        tokio::spawn(async move {
            tokio::time::sleep_until(written + max_age).await;
            if let Some(kv) = me.upgrade() {
                let mut s = kv.state.lock().unwrap();
                if s.entries.get(&key).is_some_and(|e| e.revision == revision) {
                    s.expire(&key, Instant::now(), max_age);
                }
            }
        });
        revision
    }
}

#[async_trait]
impl KvStore for MemoryKv {
    async fn create(&self, key: &str, value: Bytes) -> Result<u64, KvError> {
        self.up()?;
        let mut s = self.state.lock().unwrap();
        if s.live(key, Instant::now(), self.max_age).is_some() {
            return Err(KvError::Exists);
        }
        Ok(self.write(&mut s, key, value))
    }
    async fn update(&self, key: &str, value: Bytes, revision: u64) -> Result<u64, KvError> {
        self.up()?;
        let mut s = self.state.lock().unwrap();
        let current = s
            .live(key, Instant::now(), self.max_age)
            .map(|e| e.revision);
        if current != Some(revision) {
            return Err(KvError::WrongRevision);
        }
        Ok(self.write(&mut s, key, value))
    }
    async fn delete_expect(&self, key: &str, revision: u64) -> Result<(), KvError> {
        self.up()?;
        let mut s = self.state.lock().unwrap();
        let current = s
            .live(key, Instant::now(), self.max_age)
            .map(|e| e.revision);
        if current != Some(revision) {
            return Err(KvError::WrongRevision);
        }
        s.entries.remove(key);
        s.notify(&KvEvent::Deleted {
            key: key.to_string(),
        });
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Option<(Bytes, u64)>, KvError> {
        self.up()?;
        let mut s = self.state.lock().unwrap();
        Ok(s.live(key, Instant::now(), self.max_age)
            .map(|e| (e.value.clone(), e.revision)))
    }
    async fn watch(&self, key: &str) -> Result<KvWatch, KvError> {
        self.up()?;
        let (tx, rx) = mpsc::unbounded_channel();
        self.state
            .lock()
            .unwrap()
            .watchers
            .push((key.to_string(), tx));
        Ok(Box::pin(futures_util::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|ev| (ev, rx)) },
        )))
    }
}

/// A held lease.
#[derive(Debug)]
pub struct Lease {
    key: String,
    revision: u64,
    deadline: Instant,
    holder: Bytes,
}

impl Lease {
    /// Create-if-absent with `holder` (the instance id) as the value: `None` when another
    /// holder has it.
    pub async fn acquire(
        kv: &dyn KvStore,
        key: &str,
        holder: &str,
    ) -> Result<Option<Lease>, KvError> {
        let started = Instant::now();
        let value = Bytes::from(holder.to_string());
        match kv.create(key, value.clone()).await {
            Ok(revision) => Ok(Some(Lease {
                key: key.to_string(),
                revision,
                deadline: started + TTL - FENCE_MARGIN,
                holder: value,
            })),
            Err(KvError::Exists) => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// CAS renewal; on success the deadline moves to this call's start + TTL − margin, on
    /// failure it stays.
    pub async fn renew(&mut self, kv: &dyn KvStore) -> Result<(), KvError> {
        let started = Instant::now();
        let revision = kv
            .update(&self.key, self.holder.clone(), self.revision)
            .await?;
        self.revision = revision;
        self.deadline = started + TTL - FENCE_MARGIN;
        Ok(())
    }
    /// CAS delete at the lease's own revision.
    pub async fn release(self, kv: &dyn KvStore) -> Result<(), KvError> {
        kv.delete_expect(&self.key, self.revision).await
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    /// Past the local deadline: stop decoding.
    pub fn fenced(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}
