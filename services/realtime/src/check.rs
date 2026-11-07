//! Checks as a viewer's check identity (spec §4; R2, R3, R6 as superseded by P6/S11, R7/P7, R12,
//! R15, W2, W3, W4, W6, W20, W27, S10, S12, S26, S30).
//!
//! **Invariant:** a viewer receives a row's key only if, at the moment of the check, that
//! viewer's own console read would return that row; anything uncertain degrades to keyless
//! `changed`, never to keys.
//!
//! - Viewers are grouped by check identity: the role (`None` = the owner; claims are always
//!   `{}`). N viewers of one role cost one check.
//! - A named role's check runs on the `authenticator` pool in the console's exact session
//!   (`chelabase_query::console_session` with `RunAs::Role { role, claims: {}, oid: None }`,
//!   `statement_timeout_ms` [`STATEMENT_TIMEOUT_MS`], no plugin settings) and also runs
//!   `OWNER_LEVEL_SQL` (an owner-level role is refused); the owner's goes to
//!   `/internal/realtime/visible` ([`crate::owner`]): realtime never holds owner credentials.
//! - Every check carries the probe (a real `LIMIT 0` read of the key columns plus
//!   `row_security_active`, W2): `probe_ok = false` → `changed` (or `forbidden` at a join).
//! - Key texts are bound as `text[]` per key column and cast in the session; keys whose
//!   relation isn't [`chelabase_query::realtime::keys_supported`] or whose text isn't
//!   [`chelabase_query::realtime::key_text_allowed`] are never sent: the relation's window is
//!   `changed`.
//! - Visibility (P7): every xid of the window must be visible to the check statement's
//!   snapshot; polled every [`VISIBILITY_POLL`] up to [`VISIBILITY_WAIT`], the [`Limiter`]
//!   permit released between polls; still not visible → `changed`.
//! - [`Limiter`]: at most [`CONCURRENCY`] checks in flight per (branch, database); a group that
//!   finds no free permit gets `changed` at once (R15).
//! - [`VisibilityCache`] (R12): (group, relation, key) → visible, for presence; it lives at most
//!   one merge window and is flushed for the whole database on every window of that database.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use chelabase_query::realtime::Relation;
use tokio::time::Instant;
use uuid::Uuid;

use crate::partition::Parent;
use crate::source::CheckIdentity;

pub const CONCURRENCY: usize = 8;
pub const STATEMENT_TIMEOUT_MS: i64 = 2000;
pub const VISIBILITY_WAIT: Duration = Duration::from_secs(2);
pub const VISIBILITY_POLL: Duration = Duration::from_millis(50);
/// S30: a window still unresolved this long after it closed is sent as keyless `changed`.
pub const WINDOW_DEADLINE: Duration = Duration::from_secs(5);

/// A check group: the role (`None` = the owner).
pub type GroupKey = Option<String>;

pub fn group_key(id: &CheckIdentity) -> GroupKey {
    id.role.clone()
}

/// The database a check runs in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CheckTarget {
    pub database_id: Uuid,
    pub branch_id: Uuid,
    pub pg_database: String,
}

/// One check: the relation, the window's xids (all must be visible; empty = no wait), the
/// key texts (`None` = probe only), and the keys to print in the check's own session and
/// transaction (`convert`: a window's every key, so the check's rows and the conversion can't
/// disagree; empty for presence and view checks). `bucket` (object storage m17, a storage
/// channel's): the keys are object ids, and only the objects of that bucket the session sees
/// (and only while it sees the bucket) are returned
/// ([`chelabase_query::realtime::check_in_bucket`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRequest {
    pub rel_oid: u32,
    pub xids: Vec<u64>,
    pub keys: Option<Vec<Vec<String>>>,
    pub convert: Vec<Vec<String>>,
    pub bucket: Option<Uuid>,
}

/// `/visible`'s shape: the probe, whether every xid was visible, and the key strings of the
/// requested rows the session sees (empty when not visible); `converted`: each of the request's
/// `convert` keys in that session's form, in order (`None`: that key can't be converted), or
/// empty when the probe failed or the conversion did (a count that differs is no conversion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckAnswer {
    pub probe_ok: bool,
    pub bypass: bool,
    pub visible: bool,
    pub keys: Vec<String>,
    pub converted: Vec<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    /// An owner-level role (R3).
    #[error("the role is owner-level")]
    OwnerLevel,
    #[error("the check timed out")]
    Timeout,
    #[error("the check failed: {0}")]
    Failed(String),
    /// The compute, the pool or the orchestrator isn't reachable.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

/// A joined table as the catalog describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// OID, names and primary key (empty `key` = no primary key).
    pub relation: Relation,
    /// `pg_class.relkind`.
    pub relkind: char,
    /// Has `pg_inherits` children and isn't partitioned.
    pub inheritance_parent: bool,
}

/// What the compute says about itself (S5, S26), read on the check pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputeFacts {
    /// `pg_is_in_recovery()`.
    pub read_only: bool,
    /// `current_setting('max_replication_slots')`.
    pub max_replication_slots: u32,
    /// `rt_%` slots in `pg_replication_slots`.
    pub rt_slots: u32,
    /// The OID of the database (`pg_database.oid` of `pg_database`).
    pub db_oid: u32,
    /// The database's `chelabase.schema_version` OID ([`crate::schema::VERSION_OID_SQL`]).
    pub version_oid: Option<u32>,
}

/// Every catalog read and check the change source makes. `Err(Unavailable)` from
/// [`Checker::compute`] means the connection route doesn't answer 200 (S25 waits on it).
#[async_trait]
pub trait Checker: Send + Sync + 'static {
    async fn compute(&self, target: &CheckTarget) -> Result<ComputeFacts, CheckError>;
    /// (schema, table) → the relation, as `id` sees the catalog (`None`: no such relation).
    async fn resolve(
        &self,
        target: &CheckTarget,
        id: &CheckIdentity,
        schema: &str,
        table: &str,
    ) -> Result<Option<Resolved>, CheckError>;
    /// The relation's partition ancestors and inheritance parents ([`crate::partition::PARENTS_SQL`]).
    async fn parents(&self, target: &CheckTarget, rel_oid: u32) -> Result<Vec<Parent>, CheckError>;
    /// The probe, the visibility of `req.xids` and the visible keys, in one session as `id`.
    async fn check(
        &self,
        target: &CheckTarget,
        id: &CheckIdentity,
        req: &CheckRequest,
    ) -> Result<CheckAnswer, CheckError>;
    /// Each key's texts as `id`'s session prints them (the key string, S11), without reading
    /// the table (deletes, hidden updates); `None` for a key that can't be converted.
    async fn convert(
        &self,
        target: &CheckTarget,
        id: &CheckIdentity,
        rel_oid: u32,
        keys: &[Vec<String>],
    ) -> Result<Vec<Option<String>>, CheckError>;
}

/// The check concurrency bound of one (branch, database).
pub struct Limiter {
    permits: Arc<Semaphore>,
}

/// A check in flight; dropping it frees the permit.
#[derive(Debug)]
pub struct CheckPermit {
    _permit: OwnedSemaphorePermit,
}

impl Limiter {
    pub fn new(bound: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            permits: Arc::new(Semaphore::new(bound)),
        })
    }
    /// A permit at once, or `None` (overflow → `changed`).
    pub fn try_permit(&self) -> Option<CheckPermit> {
        self.permits
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|p| CheckPermit { _permit: p })
    }
    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

type CacheKey = (GroupKey, u32, String);
/// The flush count, and each key's answer with when it was given.
type CacheEntries = (u64, HashMap<CacheKey, (Instant, bool)>);

/// R12's cache, one per (branch, database).
pub struct VisibilityCache {
    ttl: Duration,
    /// The entries, and how many flushes there were (a check that began before a flush never
    /// writes its answers after it: [`VisibilityCache::put_since`]).
    entries: Mutex<CacheEntries>,
}

impl VisibilityCache {
    pub fn new(ttl: Duration) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            ttl,
            entries: Mutex::new((0, HashMap::new())),
        })
    }
    /// The flush count now (see [`VisibilityCache::put_since`]).
    pub fn generation(&self) -> u64 {
        self.entries.lock().unwrap().0
    }
    /// [`VisibilityCache::put`] unless the cache was flushed since `generation` (a window came
    /// in while the check ran: its answer may predate it).
    pub fn put_since(
        &self,
        generation: u64,
        now: Instant,
        group: &GroupKey,
        rel_oid: u32,
        key: &str,
        visible: bool,
    ) {
        if self.entries.lock().unwrap().0 == generation {
            self.put(now, group, rel_oid, key, visible);
        }
    }
    pub fn get(&self, now: Instant, group: &GroupKey, rel_oid: u32, key: &str) -> Option<bool> {
        let entries = self.entries.lock().unwrap();
        let (at, visible) = entries.1.get(&(group.clone(), rel_oid, key.to_string()))?;
        (now < *at + self.ttl).then_some(*visible)
    }
    pub fn put(&self, now: Instant, group: &GroupKey, rel_oid: u32, key: &str, visible: bool) {
        let mut entries = self.entries.lock().unwrap();
        // Expired entries go first, so the cache never holds more than a window's worth.
        let ttl = self.ttl;
        entries.1.retain(|_, (at, _)| now < *at + ttl);
        entries
            .1
            .insert((group.clone(), rel_oid, key.to_string()), (now, visible));
    }
    /// Every entry of the database.
    pub fn flush(&self) {
        let mut entries = self.entries.lock().unwrap();
        entries.0 += 1;
        entries.1.clear();
    }
}

// --- the production checker ---------------------------------------------------------------

/// What returns a pooled `authenticator` connection to a clean session (rest's): every
/// transaction sends it first, and again after it ends.
pub const RESET_SQL: &str = "SET ROLE NONE; RESET ALL";

const REQUEST_ID: &str = "realtime-check";

/// The compute's facts in one statement, as `authenticator` (S5, S26).
const COMPUTE_SQL: &str = "SELECT pg_catalog.pg_is_in_recovery(), \
     pg_catalog.current_setting('max_replication_slots')::pg_catalog.int4, \
     (SELECT pg_catalog.count(*)::pg_catalog.int4 FROM pg_catalog.pg_replication_slots s \
       WHERE s.slot_name OPERATOR(pg_catalog.~~) 'rt\\_%'), \
     (SELECT d.oid FROM pg_catalog.pg_database d \
       WHERE d.datname OPERATOR(pg_catalog.=) pg_catalog.current_database()), \
     pg_catalog.to_regclass('chelabase.schema_version')::pg_catalog.oid";

/// (schema, table) → OID, `relkind`, legacy inheritance parent; by name, never `search_path`.
const RESOLVE_SQL: &str = "SELECT c.oid, c.relkind::pg_catalog.text, \
     (c.relkind OPERATOR(pg_catalog.<>) 'p' AND EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i \
       WHERE i.inhparent OPERATOR(pg_catalog.=) c.oid)) \
     FROM pg_catalog.pg_class c \
     JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
     WHERE n.nspname OPERATOR(pg_catalog.=) $1::pg_catalog.text::pg_catalog.name \
       AND c.relname OPERATOR(pg_catalog.=) $2::pg_catalog.text::pg_catalog.name";

/// Whether every xid of `$1` (decimal xid8 texts) is visible to the statement's snapshot.
const XIDS_VISIBLE_SQL: &str = "SELECT pg_catalog.bool_and(pg_catalog.pg_visible_in_snapshot(\
     x::pg_catalog.xid8, pg_catalog.pg_current_snapshot())) \
     FROM pg_catalog.unnest($1::pg_catalog.text[]) AS x";

fn failure(f: chelabase_query::QueryFailure) -> CheckError {
    match (f.status.as_u16(), f.sqlstate.as_deref()) {
        (_, Some("57014")) => CheckError::Timeout,
        (503 | 504, _) => CheckError::Unavailable(f.message),
        _ => CheckError::Failed(f.message),
    }
}

fn pg_failure(e: tokio_postgres::Error) -> chelabase_query::QueryFailure {
    chelabase_query::QueryFailure::from_pg(&e, REQUEST_ID)
}

/// The session settings of every check: the console's, with [`STATEMENT_TIMEOUT_MS`].
fn settings() -> chelabase_query::RunSettings {
    chelabase_query::RunSettings {
        max_rows: 1000,
        statement_timeout_ms: STATEMENT_TIMEOUT_MS,
        default_schema: "public".into(),
        reset_sql: RESET_SQL,
    }
}

/// The checks on computes: named roles on the `authenticator` pools in the console's session,
/// the owner through the orchestrator's `/visible` (realtime holds no owner credentials).
pub struct PgChecker {
    pools: Arc<crate::pool::CheckPools>,
    owner: crate::owner::OwnerClient,
}

impl PgChecker {
    pub fn new(pools: Arc<crate::pool::CheckPools>, owner: crate::owner::OwnerClient) -> Self {
        Self { pools, owner }
    }

    /// `f` in a clean transaction as `authenticator` (catalog reads).
    async fn as_authenticator<T>(
        &self,
        target: &CheckTarget,
        f: impl AsyncFnOnce(&tokio_postgres::Client) -> Result<T, chelabase_query::QueryFailure>,
    ) -> Result<T, CheckError> {
        let client = self.pools.get(target).await?;
        chelabase_query::engine::with_transaction(&*client, RESET_SQL, REQUEST_ID, f)
            .await
            .map_err(failure)
    }

    /// `f` in the console's session as `role` (`SET ROLE`, claims `{}`), after the owner-level
    /// refusal (R3).
    async fn as_role<T>(
        &self,
        target: &CheckTarget,
        role: &str,
        f: impl AsyncFnOnce(&tokio_postgres::Client) -> Result<T, chelabase_query::QueryFailure>,
    ) -> Result<T, CheckError> {
        let client = self.pools.get(target).await?;
        let claims = serde_json::json!({});
        let run_as = chelabase_query::RunAs::Role {
            role,
            claims: &claims,
            oid: None,
        };
        let out = chelabase_query::engine::console_session(
            &*client,
            &target.pg_database,
            run_as,
            &settings(),
            REQUEST_ID,
            async |c, _version| {
                let level = c
                    .query_typed_opt(
                        chelabase_query::roles::OWNER_LEVEL_SQL,
                        &[(&role, tokio_postgres::types::Type::TEXT)],
                    )
                    .await
                    .map_err(pg_failure)?
                    .map(|r| r.try_get::<_, Option<bool>>(0))
                    .transpose()
                    .map_err(pg_failure)?;
                match level {
                    Some(Some(false)) => Ok(Some(f(c).await?)),
                    // Owner-level, or a role that isn't there (fails closed).
                    Some(Some(true)) | Some(None) => Ok(None),
                    None => Err(chelabase_query::QueryFailure::invalid_query(
                        "the role doesn't exist",
                    )),
                }
            },
        )
        .await
        .map_err(failure)?;
        out.ok_or(CheckError::OwnerLevel)
    }

    /// The owner's check through `/visible`: each earlier xid polled probe-only (a committed
    /// xid visible to one snapshot is visible to every later one), the last with the keys and
    /// the keys to convert (`convert_keys`: printed in that same owner session).
    async fn owner_check(
        &self,
        target: &CheckTarget,
        token: &str,
        req: &CheckRequest,
    ) -> Result<CheckAnswer, CheckError> {
        let mut xids: Vec<u64> = Vec::with_capacity(req.xids.len());
        for x in &req.xids {
            if !xids.contains(x) {
                xids.push(*x);
            }
        }
        let last = xids.pop();
        for x in xids {
            let a = self
                .owner
                .visible(token, target, req.rel_oid, Some(x), None)
                .await?;
            if !a.probe_ok || !a.visible {
                return Ok(CheckAnswer {
                    keys: Vec::new(),
                    converted: Vec::new(),
                    ..a
                });
            }
        }
        self.owner
            .check(token, target, req.rel_oid, last, req.keys.as_deref(), &req.convert, req.bucket)
            .await
    }
}

/// The probe, visibility and keys in one role session (see [`Checker::check`]).
async fn role_check(
    c: &tokio_postgres::Client,
    req: &CheckRequest,
) -> Result<CheckAnswer, chelabase_query::QueryFailure> {
    use chelabase_query::realtime as rt;
    let failed = CheckAnswer {
        probe_ok: false,
        bypass: false,
        visible: true,
        keys: Vec::new(),
        converted: Vec::new(),
    };
    // Any Postgres error aborts the transaction (its COMMIT rolls back): the probe failed.
    let Ok(Some(rel)) = rt::load_relation(c, req.rel_oid).await else {
        return Ok(failed);
    };
    let Ok(probe) = rt::probe(c, &rel).await else {
        return Ok(failed);
    };
    let answer = |visible: bool, keys: Vec<String>| CheckAnswer {
        probe_ok: true,
        bypass: probe.bypass,
        visible,
        keys,
        converted: Vec::new(),
    };
    let mut xids: Vec<String> = Vec::new();
    for x in &req.xids {
        let x = x.to_string();
        if !xids.contains(&x) {
            xids.push(x);
        }
    }
    let last = req.keys.as_ref().and_then(|_| xids.pop());
    if !xids.is_empty() {
        let row = c
            .query_typed_one(XIDS_VISIBLE_SQL, &[(&xids, tokio_postgres::types::Type::TEXT_ARRAY)])
            .await;
        match row.and_then(|r| r.try_get::<_, Option<bool>>(0)) {
            Ok(Some(true)) => {}
            Ok(_) => return Ok(answer(false, Vec::new())),
            Err(_) => return Ok(failed),
        }
    }
    let mut out = match &req.keys {
        None => answer(true, Vec::new()),
        Some(keys) => {
            if !rt::keys_supported(&rel)
                || keys.iter().any(|k| {
                    k.len() != rel.key.len()
                        || k.iter().zip(&rel.key).any(|(t, col)| !rt::key_text_allowed(col, t))
                })
            {
                return Ok(failed);
            }
            let last = last.and_then(|x| x.parse::<u64>().ok());
            let checked = match req.bucket {
                None => rt::check(c, &rel, last, keys).await.map(Some),
                Some(bucket) => rt::check_in_bucket(c, &rel, last, keys, bucket).await,
            };
            match checked {
                Ok(Some(checked)) => answer(checked.visible, checked.keys),
                // Not `storage.objects` (a bucket on another relation), or an error.
                Ok(None) | Err(_) => return Ok(failed),
            }
        }
    };
    // The window's keys in this same session and transaction, once its rows are visible. A
    // failed conversion aborts the transaction (nothing was written) and converts nothing.
    if out.visible && !req.convert.is_empty() {
        out.converted = convert_with(c, &rel, &req.convert).await.unwrap_or_default();
    }
    Ok(out)
}

/// Each key's session key string, `None` for a key the relation can't take (P6).
async fn role_convert(
    c: &tokio_postgres::Client,
    rel_oid: u32,
    keys: &[Vec<String>],
) -> Result<Vec<Option<String>>, chelabase_query::QueryFailure> {
    use chelabase_query::realtime as rt;
    let rel = rt::load_relation(c, rel_oid)
        .await
        .map_err(pg_failure)?
        .ok_or_else(|| chelabase_query::QueryFailure::invalid_query("the relation is gone"))?;
    convert_with(c, &rel, keys).await
}

/// [`role_convert`] for a relation already read.
async fn convert_with(
    c: &tokio_postgres::Client,
    rel: &chelabase_query::realtime::Relation,
    keys: &[Vec<String>],
) -> Result<Vec<Option<String>>, chelabase_query::QueryFailure> {
    use chelabase_query::realtime as rt;
    if !rt::keys_supported(rel) {
        return Ok(vec![None; keys.len()]);
    }
    let ok = |k: &Vec<String>| {
        k.len() == rel.key.len() && k.iter().zip(&rel.key).all(|(t, col)| rt::key_text_allowed(col, t))
    };
    let valid: Vec<Vec<String>> = keys.iter().filter(|k| ok(k)).cloned().collect();
    let mut converted = rt::convert(c, rel, &valid).await.map_err(pg_failure)?.into_iter();
    Ok(keys
        .iter()
        .map(|k| {
            if ok(k) {
                converted.next().filter(|s| !s.is_empty())
            } else {
                None
            }
        })
        .collect())
}

#[async_trait]
impl Checker for PgChecker {
    async fn compute(&self, target: &CheckTarget) -> Result<ComputeFacts, CheckError> {
        // S25: the route itself must answer (a stopped compute's doesn't).
        self.pools.lookup().fetch(target.branch_id).await?;
        let row = self
            .as_authenticator(target, async |c| {
                c.query_one(COMPUTE_SQL, &[]).await.map_err(pg_failure)
            })
            .await?;
        let get = || -> Result<ComputeFacts, tokio_postgres::Error> {
            Ok(ComputeFacts {
                read_only: row.try_get(0)?,
                max_replication_slots: u32::try_from(row.try_get::<_, i32>(1)?).unwrap_or(0),
                rt_slots: u32::try_from(row.try_get::<_, i32>(2)?).unwrap_or(u32::MAX),
                db_oid: row.try_get(3)?,
                version_oid: row.try_get(4)?,
            })
        };
        get().map_err(|e| CheckError::Failed(e.to_string()))
    }

    async fn resolve(
        &self,
        target: &CheckTarget,
        id: &CheckIdentity,
        schema: &str,
        table: &str,
    ) -> Result<Option<Resolved>, CheckError> {
        let (schema, table) = (schema.to_string(), table.to_string());
        let read = async |c: &tokio_postgres::Client| {
            let Some(row) = c
                .query_opt(RESOLVE_SQL, &[&schema, &table])
                .await
                .map_err(pg_failure)?
            else {
                return Ok(None);
            };
            let oid: u32 = row.try_get(0).map_err(pg_failure)?;
            let relkind: String = row.try_get(1).map_err(pg_failure)?;
            let inheritance_parent: bool = row.try_get(2).map_err(pg_failure)?;
            let relation = chelabase_query::realtime::load_relation(c, oid)
                .await
                .map_err(pg_failure)?;
            Ok(relation.map(|relation| Resolved {
                relation,
                relkind: relkind.chars().next().unwrap_or('?'),
                inheritance_parent,
            }))
        };
        match &id.role {
            Some(role) => self.as_role(target, role, read).await,
            // The catalog is readable by everyone: the owner's names resolve the same.
            None => self.as_authenticator(target, read).await,
        }
    }

    async fn parents(&self, target: &CheckTarget, rel_oid: u32) -> Result<Vec<Parent>, CheckError> {
        self.as_authenticator(target, async |c| {
            let rows = c
                .query_typed(
                    crate::partition::PARENTS_SQL,
                    &[(&rel_oid, tokio_postgres::types::Type::OID)],
                )
                .await
                .map_err(pg_failure)?;
            rows.iter()
                .map(|r| {
                    Ok(Parent {
                        oid: r.try_get(0).map_err(pg_failure)?,
                        keyed: r.try_get(1).map_err(pg_failure)?,
                    })
                })
                .collect()
        })
        .await
    }

    async fn check(
        &self,
        target: &CheckTarget,
        id: &CheckIdentity,
        req: &CheckRequest,
    ) -> Result<CheckAnswer, CheckError> {
        match &id.role {
            None => self.owner_check(target, &id.token, req).await,
            Some(role) => self.as_role(target, role, async |c| role_check(c, req).await).await,
        }
    }

    async fn convert(
        &self,
        target: &CheckTarget,
        id: &CheckIdentity,
        rel_oid: u32,
        keys: &[Vec<String>],
    ) -> Result<Vec<Option<String>>, CheckError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        match &id.role {
            None => Ok(self
                .owner
                .convert(&id.token, target, rel_oid, keys)
                .await?
                .into_iter()
                .map(Some)
                .collect()),
            Some(role) => {
                self.as_role(target, role, async |c| role_convert(c, rel_oid, keys).await)
                    .await
            }
        }
    }
}
