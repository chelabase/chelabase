//! Replication slots (spec §3.2, §3.3; R17, W7, W11, W21, S5, S26, P14).
//!
//! - The per-compute cap (S5) is a set of index keys `<branch_id>.<n>`, `n` in `0..cap`, in
//!   `CHELABASE_RT_SLOTS`: a holder takes the lowest `n` whose `create` succeeds, renews it with
//!   its lease and deletes it at its own revision on release; no free index is `no_slot`.
//!   `cap = min(CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE, live max_replication_slots / 2)`. The
//!   `rt_%` count in `pg_replication_slots` is a second check after taking an index: over the cap,
//!   give the index back and report `no_slot`.
//! - Slot creation gets [`CREATE_TIMEOUT`] (a running or prepared transaction blocks the
//!   consistent point, W7): on timeout the connection is closed (the temporary slot goes with
//!   it), watchers get `live: false, reason: "waiting"`, and it's retried after
//!   [`retry_delay`].
//! - Postgres errors map to status reasons with [`classify`].

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio_postgres::types::PgLsn;
use uuid::Uuid;

use crate::lease::{KvError, KvStore};
use crate::protocol::StatusReason;

pub const CREATE_TIMEOUT: Duration = Duration::from_secs(10);
/// The first retry after a failed or timed-out creation; later ones back off to [`RETRY_MAX`].
pub const RETRY: Duration = Duration::from_secs(30);
pub const RETRY_MAX: Duration = Duration::from_secs(300);

/// The delay before retry number `attempt` (1-based): [`RETRY`], doubling, at most
/// [`RETRY_MAX`].
pub fn retry_delay(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    RETRY.saturating_mul(1u32 << doublings).min(RETRY_MAX)
}

/// `min(max_per_compute, max_replication_slots / 2)`.
pub fn cap(max_per_compute: u32, max_replication_slots: u32) -> u32 {
    max_per_compute.min(max_replication_slots / 2)
}

/// `<branch_id>.<n>`.
pub fn index_key(branch_id: Uuid, n: u32) -> String {
    format!("{}.{n}", branch_id.hyphenated())
}

/// Whether creating one more slot would pass `cap`: `existing` is the compute's `rt_%` slots
/// counted before ours is created (`existing >= cap`).
pub fn over_cap(existing: u32, cap: u32) -> bool {
    existing >= cap
}

/// A held slot index.
#[derive(Debug)]
pub struct SlotIndex {
    key: String,
    n: u32,
    revision: u64,
    holder: Bytes,
}

impl SlotIndex {
    pub fn n(&self) -> u32 {
        self.n
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Renewed with the lease (a new revision).
    pub async fn renew(&mut self, kv: &dyn KvStore) -> Result<(), KvError> {
        self.revision = kv
            .update(&self.key, self.holder.clone(), self.revision)
            .await?;
        Ok(())
    }
    /// Deleted at its own revision.
    pub async fn release(self, kv: &dyn KvStore) -> Result<(), KvError> {
        kv.delete_expect(&self.key, self.revision).await
    }
}

/// The lowest free index of `branch_id` below `cap` (`None`: every index is taken).
pub async fn take_index(
    kv: &dyn KvStore,
    branch_id: Uuid,
    cap: u32,
    holder: &str,
) -> Result<Option<SlotIndex>, KvError> {
    let value = Bytes::from(holder.to_string());
    for n in 0..cap {
        let key = index_key(branch_id, n);
        match kv.create(&key, value.clone()).await {
            Ok(revision) => {
                return Ok(Some(SlotIndex {
                    key,
                    n,
                    revision,
                    holder: value,
                }));
            }
            Err(KvError::Exists) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Why a slot isn't streaming.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SlotError {
    /// The compute's `max_replication_slots`/`max_wal_senders`, or realtime's cap.
    #[error("no free replication slot")]
    NoSlot,
    /// No `CONNECT` for `chelabase_realtime`, or the database doesn't accept connections (W21).
    #[error("no CONNECT on the database")]
    NoConnect,
    #[error("the compute is read-only")]
    ReadOnly,
    /// The plugin refuses the database (SQL_ASCII).
    #[error("the database's encoding isn't supported")]
    Unsupported,
    /// Creation timed out (W7).
    #[error("waiting for running transactions")]
    Waiting,
    #[error("unavailable: {0}")]
    Unavailable(String),
}

impl SlotError {
    /// The `status` reason watchers get.
    pub fn reason(&self) -> StatusReason {
        match self {
            SlotError::NoSlot => StatusReason::NoSlot,
            SlotError::NoConnect => StatusReason::NoConnect,
            SlotError::ReadOnly => StatusReason::ReadOnly,
            SlotError::Unsupported => StatusReason::Unsupported,
            SlotError::Waiting => StatusReason::Waiting,
            SlotError::Unavailable(_) => StatusReason::Unavailable,
        }
    }
}

/// A Postgres error (SQLSTATE, message) of the replication session as a [`SlotError`]:
/// `53400` (all replication slots in use) and `53300` (too many connections / wal senders) →
/// `NoSlot`; `42501` "permission denied for database" and `55000` "not currently accepting
/// connections" → `NoConnect`; `0A000` from `chelabase_keys` (encoding) → `Unsupported`;
/// `25006`/"recovery" → `ReadOnly`; anything else → `Unavailable`.
pub fn classify(code: Option<&str>, message: &str) -> SlotError {
    let lower = message.to_ascii_lowercase();
    match code {
        Some("53400" | "53300") => SlotError::NoSlot,
        Some("42501") if lower.contains("permission denied for database") => SlotError::NoConnect,
        Some("55000") if lower.contains("not currently accepting connections") => {
            SlotError::NoConnect
        }
        Some("0A000") if lower.contains("encoding") => SlotError::Unsupported,
        Some("25006") => SlotError::ReadOnly,
        _ if lower.contains("recovery") => SlotError::ReadOnly,
        _ => SlotError::Unavailable(message.to_string()),
    }
}

/// What the walsender sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalMessage {
    Data { wal_end: PgLsn, data: Bytes },
    Keepalive { wal_end: PgLsn, reply: bool },
}

/// A streaming slot. Dropping it closes the replication connection (the temporary slot goes
/// with it).
#[async_trait]
pub trait WalStream: Send {
    /// The next message; `None` when the stream ended (a compute restart, a parse error).
    async fn next(&mut self) -> Option<Result<WalMessage, SlotError>>;
    /// A standby status update with `flush` as written, flushed and applied. It must not stop
    /// the stream from being read (T2 note).
    async fn confirm(&mut self, flush: PgLsn) -> Result<(), SlotError>;
}

/// Where a database's slot is created.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SlotTarget {
    pub branch_id: Uuid,
    pub pg_database: String,
    pub db_oid: u32,
}

/// Opens replication sessions: connect as `chelabase_realtime`, `CREATE_REPLICATION_SLOT`, then
/// `START_REPLICATION` from the slot's consistent point. The caller bounds the whole call with
/// [`CREATE_TIMEOUT`]; dropping the future closes the connection.
#[async_trait]
pub trait Replicator: Send + Sync + 'static {
    async fn start(&self, target: &SlotTarget, slot: &str)
    -> Result<Box<dyn WalStream>, SlotError>;
}

// --- the production replicator ------------------------------------------------------------

/// A standby status update that the connection doesn't take within this drops the slot.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// A Postgres error of the replication session as a [`SlotError`].
fn slot_error(e: &tokio_postgres::Error) -> SlotError {
    match e.as_db_error() {
        Some(db) => classify(Some(db.code().code()), db.message()),
        None => SlotError::Unavailable(e.to_string()),
    }
}

/// The task driving a replication connection; dropping it closes the connection (the
/// temporary slot goes with it).
struct ConnTask(tokio::task::JoinHandle<()>);

impl Drop for ConnTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A streaming `chelabase_keys` slot on a compute.
struct PgWalStream {
    stream: std::pin::Pin<Box<tokio_postgres::replication::ReplicationStream>>,
    _client: tokio_postgres::Client,
    _conn: ConnTask,
}

#[async_trait]
impl WalStream for PgWalStream {
    async fn next(&mut self) -> Option<Result<WalMessage, SlotError>> {
        use futures_util::StreamExt;
        use tokio_postgres::replication::ReplicationMessage;
        loop {
            return match self.stream.next().await? {
                Ok(ReplicationMessage::XLogData(body)) => Some(Ok(WalMessage::Data {
                    wal_end: PgLsn::from(body.wal_end()),
                    data: body.data().clone(),
                })),
                Ok(ReplicationMessage::PrimaryKeepAlive(k)) => Some(Ok(WalMessage::Keepalive {
                    wal_end: PgLsn::from(k.wal_end()),
                    reply: k.reply() == 1,
                })),
                Ok(_) => continue,
                // The stream ends after its first error (T2): a fresh slot follows.
                Err(e) => Some(Err(slot_error(&e))),
            };
        }
    }
    async fn confirm(&mut self, flush: PgLsn) -> Result<(), SlotError> {
        let update = self.stream.as_mut().standby_status_update(
            flush,
            flush,
            flush,
            std::time::SystemTime::now(),
            0,
        );
        match tokio::time::timeout(CONFIRM_TIMEOUT, update).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(slot_error(&e)),
            Err(_) => Err(SlotError::Unavailable(
                "the standby status update wasn't taken".into(),
            )),
        }
    }
}

/// Replication sessions as `chelabase_realtime` to the branch's compute (the connection
/// route's address, TLS verified as its certificate name).
pub struct PgReplicator {
    lookup: std::sync::Arc<crate::pool::ConnLookup>,
    tls: Option<chelabase_query::server_tls::ServerTls>,
    password: String,
}

impl PgReplicator {
    pub fn new(
        lookup: std::sync::Arc<crate::pool::ConnLookup>,
        tls: Option<chelabase_query::server_tls::ServerTls>,
        password: String,
    ) -> Self {
        Self {
            lookup,
            tls,
            password,
        }
    }
}

impl std::fmt::Debug for PgReplicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgReplicator").finish_non_exhaustive()
    }
}

#[async_trait]
impl Replicator for PgReplicator {
    async fn start(
        &self,
        target: &SlotTarget,
        slot: &str,
    ) -> Result<Box<dyn WalStream>, SlotError> {
        use crate::decoder::{
            ReplicationTarget, create_slot_command, replication_config, start_replication_command,
        };
        let info = self
            .lookup
            .get(target.branch_id)
            .await
            .map_err(|e| SlotError::Unavailable(e.to_string()))?;
        let config = replication_config(&ReplicationTarget {
            host: info.host.clone(),
            port: info.port,
            server_name: info.server_name.clone(),
            database: target.pg_database.clone(),
            password: self.password.clone(),
        });
        let (client, conn) = match (&info.server_name, &self.tls) {
            (Some(_), Some(tls)) => {
                let (client, connection) = config
                    .connect(tls.connector())
                    .await
                    .map_err(|e| slot_error(&e))?;
                (client, ConnTask(tokio::spawn(async move {
                    let _ = connection.await;
                })))
            }
            (None, None) => {
                let (client, connection) = config
                    .connect(tokio_postgres::NoTls)
                    .await
                    .map_err(|e| slot_error(&e))?;
                (client, ConnTask(tokio::spawn(async move {
                    let _ = connection.await;
                })))
            }
            _ => {
                return Err(SlotError::Unavailable(
                    "the compute's TLS can't be verified (CA and certificate name)".into(),
                ));
            }
        };
        let rows = client
            .simple_query(&create_slot_command(slot))
            .await
            .map_err(|e| slot_error(&e))?;
        let consistent_point = rows
            .iter()
            .find_map(|m| match m {
                tokio_postgres::SimpleQueryMessage::Row(row) => row
                    .get("consistent_point")
                    .and_then(|t| t.parse::<PgLsn>().ok()),
                _ => None,
            })
            .ok_or_else(|| SlotError::Unavailable("no consistent point".into()))?;
        let duplex = client
            .copy_both_simple::<Bytes>(&start_replication_command(slot, consistent_point))
            .await
            .map_err(|e| slot_error(&e))?;
        Ok(Box::new(PgWalStream {
            stream: Box::pin(tokio_postgres::replication::ReplicationStream::new(duplex)),
            _client: client,
            _conn: conn,
        }))
    }
}
