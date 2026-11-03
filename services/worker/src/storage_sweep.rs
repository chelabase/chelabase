//! The storage sweep (object-storage m8): deletes the blobs no branch of a Chelabase database
//! can reach any more, and the leftovers of failed uploads.
//!
//! Blobs live at `{database_id}/{datoid}/{version_id}` (m5). Every branch shares the prefix
//! and a sibling branch can reuse an OID for another database, so a version is kept while any
//! database of any live branch (backups included) has it in `storage.objects` or
//! `storage.upload_tickets`: the union over the whole Chelabase database, never per OID.
//!
//! A pass, per Chelabase database ([`sweep_database`]):
//!
//! 1. The store's own clock, from a probe object (never the worker's).
//! 2. Every live branch's references, in every database it has; dead tickets (expired, or
//!    another timeline's) deleted on the branches with a compute. Any branch that isn't
//!    `ready`, has a restore or reset in flight, or can't be read stops the pass: nothing is
//!    deleted (fail closed), and the report says why.
//! 3. The blobs, the marks and the open multipart uploads are listed; then the branches are
//!    read again. A branch made, reset or restored meanwhile shows as a changed timeline set
//!    and stops the pass.
//! 4. A blob nobody references, older than the grace period by the store's clock, is first
//!    marked (an empty object under `_sweep/{database_id}/marks/`, whose own time is the
//!    store's). It is deleted only once its mark is older than the grace period plus the
//!    longest history a branch can be restored from.
//!
//! The store's clock is checked against every compute's `clock_timestamp()` (at most
//! [`MAX_SKEW`] apart) and against the last pass (it may not run ahead of the databases' clock
//! by more than twice): a leaping store clock would otherwise cut every mark's wait short.
//! A dry run still writes the clock probe, so it needs write access to the bucket.
//!
//! Why marks: a point-in-time restore or a branch made at a past point can bring back rows a
//! pass never saw. Once a blob is older than the grace period no upload can reference it any
//! more (tickets live minutes; grace must exceed the longest upload), and a move keeps its
//! version on the same branch, so only a new timeline can bring a reference back. The pass
//! records the timelines it saw (`_sweep/{database_id}/timelines`). A timeline it hasn't
//! recorded holds what a recorded ancestor held at its branch point P (Neon's ancestry, walked
//! through unrecorded timelines), plus fresh uploads: it voids the marks made after P (less
//! [`MAX_SKEW`]), and every mark when no recorded ancestor or no time for P is found. A branch
//! made at head voids nothing older than an hour. With the set unchanged since a mark, the
//! blob was unreferenced on every branch from the mark on, so no restore target within the
//! window can need it once the mark is older than the window.
//!
//! A database that is `deleted` with no live branch (so no backup) loses its whole prefix. Its
//! delete first writes a purge marker (`_sweep/purge/{database_id}`), which the periodic job
//! queues again until the purge removes it.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use chelabase_metadata::{
    BranchKind, BranchRecord, BranchStatus, DatabaseStatus, NewStaticEndpoint,
};
use chelabase_neon::{ComputeBackend, NeonId, SqlOpts};
use chelabase_storage::{ObjectStore, PutOptions, StorageError};
use chelabase_types::{BranchId, DatabaseId};
use tracing::{info, warn};
use uuid::Uuid;

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::provision::{new_compute_token, new_endpoint_id, sha256_hex};

/// `CHELABASE_STORAGE_SWEEP_GRACE_SECS`'s default and its floor: 8 days. Two things bound it
/// from below, and any change must keep both:
///
/// - Signed download URLs (m6) pin a version and live up to
///   [`chelabase_storage::MAX_SIGNED_URL_TTL`] (7 days), after the row that named the version
///   is gone (an overwrite, a delete). The grace, and so every mark's wait, is never below that
///   TTL plus a day.
/// - A blob streams to its version key before its row exists (an upload has run past 18
///   minutes), and an open multipart upload older than the grace is aborted: the grace stays
///   far above the longest upload. So does a multipart complete (m7), which claims (deletes)
///   its `upload_sessions` row before S3's CompleteMultipartUpload and the object's row: for
///   that window its upload and its version key are named by nothing, and only the grace
///   keeps them.
/// - With no `pitr_interval` set, Neon keeps up to 7 days of history
///   ([`NEON_DEFAULT_PITR`]); the window is taken as that then, and the grace is above it too.
///   Neon accepts any point back to its applied GC cutoff, which trails the interval by one GC
///   period; the grace absorbs that lag (a GC stalled for longer than the grace isn't).
pub const DEFAULT_GRACE: Duration = Duration::from_secs(8 * 86_400);

/// The shortest grace the setting accepts (see [`DEFAULT_GRACE`]).
pub const MIN_GRACE: Duration = DEFAULT_GRACE;

const _: () =
    assert!(MIN_GRACE.as_secs() >= chelabase_storage::MAX_SIGNED_URL_TTL.as_secs() + 86_400);
const _: () = assert!(MIN_GRACE.as_secs() >= NEON_DEFAULT_PITR.as_secs());

/// Neon's `DEFAULT_PITR_INTERVAL`: the history a tenant without `pitr_interval` keeps.
pub const NEON_DEFAULT_PITR: Duration = Duration::from_secs(7 * 86_400);

/// How far the store's clock and a database's may disagree before a pass is refused; also the
/// margin taken off a new timeline's branch point.
pub const MAX_SKEW: Duration = Duration::from_secs(3600);

/// After this many failed deliveries a sweep job is dropped (logged and counted); the next
/// period queues the database again.
pub const SWEEP_ATTEMPTS: u64 = 3;

/// `CHELABASE_STORAGE_SWEEP_SECS`'s default: every database is swept every 6 hours.
pub const DEFAULT_EVERY: Duration = Duration::from_secs(6 * 3600);

/// How long one SQL read of a branch may take.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The sweep's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepSettings {
    /// `CHELABASE_STORAGE_SWEEP_GRACE_SECS` (default and floor 8 days, at most 60 days).
    pub grace: Duration,
    /// `CHELABASE_STORAGE_SWEEP_SECS` (default 6 h, 60 s to 7 days).
    pub every: Duration,
    /// `CHELABASE_STORAGE_SWEEP_DRY_RUN`: count, delete nothing.
    pub dry_run: bool,
}

impl Default for SweepSettings {
    fn default() -> Self {
        Self {
            grace: DEFAULT_GRACE,
            every: DEFAULT_EVERY,
            dry_run: false,
        }
    }
}

impl SweepSettings {
    /// From the environment; blank is the default.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let secs =
            |name: &str, default: Duration, min: u64, max: u64| -> Result<Duration, String> {
                match get(name)
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                {
                    None => Ok(default),
                    Some(v) => match v.parse::<u64>() {
                        Ok(n) if (min..=max).contains(&n) => Ok(Duration::from_secs(n)),
                        _ => Err(format!(
                            "{name}={v:?}: expected whole seconds from {min} to {max}"
                        )),
                    },
                }
            };
        let dry_run = match get("CHELABASE_STORAGE_SWEEP_DRY_RUN")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("" | "false" | "0" | "no" | "off") => false,
            Some("true" | "1" | "yes" | "on") => true,
            Some(other) => {
                return Err(format!(
                    "CHELABASE_STORAGE_SWEEP_DRY_RUN={other:?}: expected true or false"
                ));
            }
        };
        Ok(Self {
            grace: secs(
                "CHELABASE_STORAGE_SWEEP_GRACE_SECS",
                DEFAULT_GRACE,
                MIN_GRACE.as_secs(),
                60 * 86_400,
            )?,
            every: secs(
                "CHELABASE_STORAGE_SWEEP_SECS",
                DEFAULT_EVERY,
                60,
                7 * 86_400,
            )?,
            dry_run,
        })
    }
}

/// One live branch, as a pass sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepBranch {
    pub id: BranchId,
    pub kind: BranchKind,
    pub status: BranchStatus,
    pub timeline: Option<String>,
    pub pending_timeline: Option<String>,
    pub endpoint_id: Option<String>,
}

impl From<&BranchRecord> for SweepBranch {
    fn from(b: &BranchRecord) -> Self {
        Self {
            id: b.id,
            kind: b.kind,
            status: b.status,
            timeline: b.timeline_id.clone(),
            pending_timeline: b.pending_timeline_id.clone(),
            endpoint_id: b.endpoint_id.clone(),
        }
    }
}

/// A Chelabase database, as a pass sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepDatabase {
    pub status: DatabaseStatus,
    /// Every branch that isn't deleted, backups included.
    pub branches: Vec<SweepBranch>,
    /// The longest history a branch can be restored from.
    pub retention: Duration,
}

/// What one branch's read found, over all its databases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchRead {
    /// Versions in `storage.objects`, `upload_tickets` and `upload_sessions`.
    pub versions: HashSet<Uuid>,
    /// Dead tickets and expired sessions deleted (only with `expire`).
    pub deleted_rows: u64,
    /// `neon.timeline_id` as each database's read reported it.
    pub timelines: BTreeSet<String>,
    /// The compute's `clock_timestamp()`.
    pub clock: Option<SystemTime>,
    /// Upload ids of unexpired upload sessions: never aborted.
    pub live_uploads: HashSet<String>,
}

/// Why a read failed: `Refused` is a state that holds until someone acts (a closed database,
/// a compute on the wrong timeline, a damaged schema), warned and counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Failed(String),
    Refused(String),
}

impl From<String> for ReadError {
    fn from(e: String) -> Self {
        Self::Failed(e)
    }
}

impl From<&str> for ReadError {
    fn from(e: &str) -> Self {
        Self::Failed(e.into())
    }
}

/// What a pass reads; the worker's ports in production, a model in the tests.
pub trait SweepSource: Send + Sync {
    /// The database and its live branches; `None` when there's no such row.
    fn database(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Option<SweepDatabase>, String>> + Send;
    /// What `branch` references in all its databases. With `expire`, its dead tickets and
    /// its expired sessions whose upload isn't in `open_uploads` are deleted first.
    fn references(
        &self,
        db: DatabaseId,
        branch: &SweepBranch,
        expire: bool,
        open_uploads: &[String],
    ) -> impl Future<Output = Result<BranchRead, ReadError>> + Send;
    /// When a timeline not in `recorded` branched off a recorded one (through unrecorded
    /// ones); `None` when there's no such ancestor or no time for the point.
    fn branch_point(
        &self,
        db: DatabaseId,
        timeline: &str,
        recorded: &BTreeSet<String>,
    ) -> impl Future<Output = Result<Option<SystemTime>, String>> + Send;
}

/// How one pass went. In a dry run the counts say what a real pass would have done.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub dry_run: bool,
    pub blobs: usize,
    pub referenced: usize,
    /// Unreferenced, younger than the grace period.
    pub young: usize,
    /// Unreferenced and old enough: marked this pass.
    pub marked: usize,
    /// Marked earlier, the mark not old enough yet.
    pub waiting: usize,
    pub deleted: usize,
    pub marks_cleared: usize,
    pub uploads_aborted: usize,
    /// Dead tickets and expired sessions deleted.
    pub tickets_deleted: u64,
    /// Keys under the prefix that aren't `{datoid}/{version}`: never touched.
    pub unknown_keys: usize,
    /// The whole prefix went (a deleted database).
    pub purged: bool,
    /// Why the pass stopped before deleting anything.
    pub blocked: Option<String>,
    /// The branch whose status stopped it, with that status.
    pub blocked_branch: Option<(BranchId, BranchStatus)>,
    /// The stop holds until someone acts (a branch's status, a closed database, a clock, a
    /// damaged record): warned and counted.
    pub refused: bool,
}

fn blob_prefix(db: DatabaseId) -> String {
    format!("{db}/")
}

fn sweep_prefix(db: DatabaseId) -> String {
    format!("_sweep/{db}/")
}

fn mark_prefix(db: DatabaseId) -> String {
    format!("_sweep/{db}/marks/")
}

/// Where a database's delete leaves its purge pending: outside `_sweep/{db}/`, so the periodic
/// job lists every pending purge at once.
pub const PURGE_PREFIX: &str = "_sweep/purge/";

fn purge_marker(db: DatabaseId) -> String {
    format!("{PURGE_PREFIX}{db}")
}

fn age(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
}

fn apart(a: SystemTime, b: SystemTime) -> Duration {
    a.duration_since(b)
        .or_else(|_| b.duration_since(a))
        .unwrap_or_default()
}

fn secs(t: SystemTime) -> f64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn store_err(what: &str) -> impl Fn(StorageError) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// The store's clock: the time it gives a probe object written now.
async fn store_now(store: &dyn ObjectStore, db: DatabaseId) -> Result<SystemTime, String> {
    let key = format!("{}clock", sweep_prefix(db));
    store
        .put(&key, Bytes::new(), PutOptions::default())
        .await
        .map_err(store_err("writing the clock probe"))?;
    store
        .list_dated(&key)
        .await
        .map_err(store_err("reading the clock probe"))?
        .into_iter()
        .find(|o| o.key == key)
        .map(|o| o.modified)
        .ok_or_else(|| "the clock probe isn't listed".to_string())
}

/// Every timeline a branch has or is switching to.
fn timelines(branches: &[SweepBranch]) -> BTreeSet<String> {
    branches
        .iter()
        .flat_map(|b| [b.timeline.clone(), b.pending_timeline.clone()])
        .flatten()
        .collect()
}

/// Why a pass couldn't read every branch: the reason, the branch and status when that was
/// it, and whether it's a refusal (warned and counted).
struct Refusal {
    why: String,
    branch: Option<(BranchId, BranchStatus)>,
    refused: bool,
}

impl Refusal {
    fn block(self, mut report: SweepReport) -> Result<SweepReport, String> {
        report.blocked = Some(self.why);
        report.blocked_branch = self.branch;
        report.refused = self.refused;
        Ok(report)
    }
}

/// What every branch's read found.
#[derive(Default)]
struct Found {
    refs: HashSet<Uuid>,
    deleted_rows: u64,
    clocks: Vec<SystemTime>,
    live_uploads: HashSet<String>,
}

/// The references of `branches` (backups only with `backups`), or why they can't all be read.
async fn enumerate<S: SweepSource>(
    source: &S,
    db: DatabaseId,
    branches: &[SweepBranch],
    expire: bool,
    backups: bool,
    open_uploads: &[String],
) -> Result<Found, Refusal> {
    let mut found = Found::default();
    for b in branches {
        let status = |why: String| Refusal {
            why,
            branch: Some((b.id, b.status)),
            refused: true,
        };
        if b.status != BranchStatus::Ready {
            return Err(status(format!("branch {} is {}", b.id, b.status.as_str())));
        }
        if b.pending_timeline.is_some() {
            return Err(status(format!(
                "branch {} is {} with a reset or restore in flight",
                b.id,
                b.status.as_str()
            )));
        }
        if b.kind == BranchKind::Backup && !backups {
            continue;
        }
        let expire = expire && b.kind != BranchKind::Backup;
        let read = source
            .references(db, b, expire, open_uploads)
            .await
            .map_err(|e| match e {
                ReadError::Failed(e) => Refusal {
                    why: format!("branch {}: {e}", b.id),
                    branch: None,
                    refused: false,
                },
                ReadError::Refused(e) => Refusal {
                    why: format!("branch {}: {e}", b.id),
                    branch: None,
                    refused: true,
                },
            })?;
        // The compute must serve the branch's own timeline (a stale one would read old state).
        let expected: BTreeSet<String> = b.timeline.iter().cloned().collect();
        if read.timelines != expected {
            return Err(Refusal {
                why: format!(
                    "branch {}'s compute serves timeline {:?}, not {:?}",
                    b.id, read.timelines, b.timeline
                ),
                branch: None,
                refused: true,
            });
        }
        found.refs.extend(read.versions);
        found.deleted_rows += read.deleted_rows;
        found.clocks.extend(read.clock);
        found.live_uploads.extend(read.live_uploads);
    }
    Ok(found)
}

/// The last pass's clocks (`_sweep/{db}/last-pass`): the store's and the databases', in
/// seconds since the epoch.
#[derive(serde::Serialize, serde::Deserialize)]
struct LastPass {
    store: f64,
    databases: f64,
}

/// Whether the store's clock can be trusted this pass: near every database's, and not run
/// ahead of them since the last pass by more than twice (plus [`MAX_SKEW`]).
fn clock_refusal(
    now: SystemTime,
    clocks: &[SystemTime],
    last: Option<&LastPass>,
) -> Option<String> {
    let Some(databases) = clocks.iter().max().copied() else {
        return Some("no database clock was read".into());
    };
    if let Some(c) = clocks.iter().find(|c| apart(now, **c) > MAX_SKEW) {
        return Some(format!(
            "the store's clock ({:.0}) and a database's ({:.0}) are more than {} s apart",
            secs(now),
            secs(*c),
            MAX_SKEW.as_secs()
        ));
    }
    let last = last?;
    let store_ran = (secs(now) - last.store).max(0.0);
    let databases_ran = (secs(databases) - last.databases).max(0.0);
    (store_ran > 2.0 * databases_ran + MAX_SKEW.as_secs_f64()).then(|| {
        format!(
            "the store's clock ran {store_ran:.0} s since the last pass, the databases' {databases_ran:.0} s"
        )
    })
}

/// One pass over `db` (see the module docs). `Err` is the store or metadata failing
/// mid-pass; a pass that can't read every branch is `Ok` with [`SweepReport::blocked`].
pub async fn sweep_database<S: SweepSource>(
    source: &S,
    store: &dyn ObjectStore,
    db: DatabaseId,
    grace: Duration,
    dry_run: bool,
) -> Result<SweepReport, String> {
    let report = SweepReport {
        dry_run,
        ..SweepReport::default()
    };
    let blocked = |why: String| Refusal {
        why,
        branch: None,
        refused: false,
    };
    let refused = |why: String| Refusal {
        why,
        branch: None,
        refused: true,
    };
    let now = store_now(store, db).await?;
    let Some(first) = source.database(db).await? else {
        return blocked("no such database".into()).block(report);
    };
    match first.status {
        DatabaseStatus::Ready => {}
        DatabaseStatus::Deleted if first.branches.is_empty() => {
            return purge(store, db, report).await;
        }
        DatabaseStatus::Deleted => {
            let why = format!("deleted, but {} branch(es) remain", first.branches.len());
            return blocked(why).block(report);
        }
        other => return blocked(format!("the database is {}", other.as_str())).block(report),
    }
    // Listed first: an expired session's row is deleted only once its upload is gone.
    let uploads = store
        .list_uploads(&blob_prefix(db))
        .await
        .map_err(store_err("listing the open uploads"))?;
    let open: Vec<String> = uploads.iter().map(|u| u.upload_id.clone()).collect();
    let mut found = match enumerate(source, db, &first.branches, !dry_run, true, &open).await {
        Ok(found) => found,
        Err(refusal) => return refusal.block(report),
    };
    let mut report = report;
    report.tickets_deleted = found.deleted_rows;
    let last_key = format!("{}last-pass", sweep_prefix(db));
    let last: Option<LastPass> = match read_json(store, &last_key).await {
        Ok(last) => last,
        Err(e) => return refused(e).block(report),
    };
    if let Some(why) = clock_refusal(now, &found.clocks, last.as_ref()) {
        return refused(why).block(report);
    }
    let blobs = store
        .list_dated(&blob_prefix(db))
        .await
        .map_err(store_err("listing the blobs"))?;
    let mut marks = dated(store, &mark_prefix(db)).await?;

    // Again, before anything is deleted: a branch made, reset or restored meanwhile changes
    // the timeline set.
    let Some(again) = source.database(db).await? else {
        return blocked("the database row went during the pass".into()).block(report);
    };
    if again.status != DatabaseStatus::Ready {
        return blocked(format!("the database became {}", again.status.as_str())).block(report);
    }
    let seen = timelines(&again.branches);
    if seen != timelines(&first.branches) {
        return blocked("the branches changed during the pass".into()).block(report);
    }
    match enumerate(source, db, &again.branches, false, false, &open).await {
        Ok(more) => {
            found.refs.extend(more.refs);
            found.live_uploads.extend(more.live_uploads);
        }
        Err(refusal) => return refusal.block(report),
    }
    let refs = found.refs;
    let retention = first.retention.max(again.retention);

    // A new timeline holds what its recorded ancestor held at its branch point: the marks made
    // after that point start over (all of them when the point is unknown).
    let recorded_key = format!("{}timelines", sweep_prefix(db));
    let recorded: Option<BTreeSet<String>> = match read_json(store, &recorded_key).await {
        Ok(recorded) => recorded,
        Err(e) => return refused(e).block(report),
    };
    if recorded.as_ref() != Some(&seen) {
        let mut void_from: Option<SystemTime> = None;
        let mut void_all = recorded.is_none();
        if let Some(recorded) = &recorded {
            for tl in seen.difference(recorded) {
                match source.branch_point(db, tl, recorded).await? {
                    Some(p) => {
                        let p = p.checked_sub(MAX_SKEW).unwrap_or(SystemTime::UNIX_EPOCH);
                        void_from = Some(void_from.map_or(p, |v| v.min(p)));
                    }
                    None => void_all = true,
                }
            }
        }
        let voided: Vec<String> = marks
            .iter()
            .filter(|(_, at)| void_all || void_from.is_some_and(|from| **at >= from))
            .map(|(k, _)| k.clone())
            .collect();
        report.marks_cleared += voided.len();
        for key in voided {
            marks.remove(&key);
            if !dry_run {
                delete_under(
                    store,
                    &mark_prefix(db),
                    &format!("{}{key}", mark_prefix(db)),
                )
                .await?;
            }
        }
        if !dry_run {
            let body = serde_json::to_vec(&seen).map_err(|e| e.to_string())?;
            store
                .put(&recorded_key, body.into(), PutOptions::default())
                .await
                .map_err(store_err("recording the timelines"))?;
        }
    }

    for blob in &blobs {
        report.blobs += 1;
        let Some(rest) = blob.key.strip_prefix(&blob_prefix(db)) else {
            continue;
        };
        let Some(version) = version_of(rest) else {
            report.unknown_keys += 1;
            continue;
        };
        let mark_key = format!("{}{rest}", mark_prefix(db));
        let mark = marks.remove(rest);
        // A mark older than its blob (the key written again) isn't this blob's wait.
        let stale = mark.is_some_and(|m| m < blob.modified);
        let mark = mark.filter(|_| !stale);
        if (stale || (mark.is_some() && refs.contains(&version))) && !dry_run {
            delete_under(store, &mark_prefix(db), &mark_key).await?;
        }
        if refs.contains(&version) {
            report.referenced += 1;
            report.marks_cleared += usize::from(mark.is_some() || stale);
        } else if age(now, blob.modified) < grace {
            report.young += 1;
            report.marks_cleared += usize::from(stale);
        } else if let Some(marked_at) = mark {
            if age(now, marked_at) >= grace + retention {
                report.deleted += 1;
                if !dry_run {
                    delete_under(store, &blob_prefix(db), &blob.key).await?;
                    delete_under(store, &mark_prefix(db), &mark_key).await?;
                }
            } else {
                report.waiting += 1;
            }
        } else {
            report.marked += 1;
            if !dry_run {
                store
                    .put(&mark_key, Bytes::new(), PutOptions::default())
                    .await
                    .map_err(store_err("marking a blob"))?;
            }
        }
    }
    // Marks of blobs that are gone.
    for key in marks.keys() {
        report.marks_cleared += 1;
        if !dry_run {
            delete_under(
                store,
                &mark_prefix(db),
                &format!("{}{key}", mark_prefix(db)),
            )
            .await?;
        }
    }
    abort_old_uploads(
        store,
        db,
        &uploads,
        &found.live_uploads,
        now,
        grace,
        &mut report,
    )
    .await?;
    if !dry_run && let Some(databases) = found.clocks.iter().max() {
        let pass = LastPass {
            store: secs(now),
            databases: secs(*databases),
        };
        let body = serde_json::to_vec(&pass).map_err(|e| e.to_string())?;
        store
            .put(&last_key, body.into(), PutOptions::default())
            .await
            .map_err(store_err("recording the pass's clocks"))?;
    }
    Ok(report)
}

/// The objects under `prefix`, by the rest of their key, with their times.
async fn dated(
    store: &dyn ObjectStore,
    prefix: &str,
) -> Result<BTreeMap<String, SystemTime>, String> {
    Ok(store
        .list_dated(prefix)
        .await
        .map_err(store_err("listing the marks"))?
        .into_iter()
        .filter_map(|o| Some((o.key.strip_prefix(prefix)?.to_string(), o.modified)))
        .collect())
}

/// Aborts the open uploads older than `grace` that no live upload session names. A store
/// that doesn't say when an upload began (SeaweedFS) has it timed by a mark
/// (`_sweep/{db}/uploads/{hex id}`, the store's time) made when a pass first sees it; marks of
/// uploads that are gone go too. The grace covers a multipart complete in flight (see
/// [`DEFAULT_GRACE`]).
async fn abort_old_uploads(
    store: &dyn ObjectStore,
    db: DatabaseId,
    uploads: &[chelabase_storage::OpenUpload],
    live: &HashSet<String>,
    now: SystemTime,
    grace: Duration,
    report: &mut SweepReport,
) -> Result<(), String> {
    let prefix = format!("{}uploads/", sweep_prefix(db));
    let mut marks = dated(store, &prefix).await?;
    for upload in uploads
        .iter()
        .filter(|u| u.key.starts_with(&blob_prefix(db)))
    {
        let id = hex::encode(&upload.upload_id);
        let mark = marks.remove(&id);
        if live.contains(&upload.upload_id) {
            continue;
        }
        let Some(began) = upload.initiated.or(mark) else {
            if !report.dry_run {
                store
                    .put(
                        &format!("{prefix}{id}"),
                        Bytes::new(),
                        PutOptions::default(),
                    )
                    .await
                    .map_err(store_err("marking an upload"))?;
            }
            continue;
        };
        if age(now, began) < grace {
            continue;
        }
        report.uploads_aborted += 1;
        if !report.dry_run {
            abort_under(store, &blob_prefix(db), upload).await?;
            if mark.is_some() {
                delete_under(store, &prefix, &format!("{prefix}{id}")).await?;
            }
        }
    }
    if !report.dry_run {
        for id in marks.keys() {
            delete_under(store, &prefix, &format!("{prefix}{id}")).await?;
        }
    }
    Ok(())
}

/// `{datoid}/{version}` → the version.
fn version_of(rest: &str) -> Option<Uuid> {
    let (oid, version) = rest.split_once('/')?;
    oid.parse::<u32>().ok()?;
    version.parse().ok()
}

/// Deletes `key`, which must be under `prefix` (a store that ignored a listing's prefix never
/// reaches another database's keys).
async fn delete_under(store: &dyn ObjectStore, prefix: &str, key: &str) -> Result<(), String> {
    if !key.starts_with(prefix) || prefix.is_empty() {
        return Err(format!(
            "refusing to delete {key:?}: it isn't under {prefix:?}"
        ));
    }
    store
        .delete(key)
        .await
        .map_err(|e| format!("deleting {key}: {e}"))
}

/// Aborts `upload`, which must be under `prefix`.
async fn abort_under(
    store: &dyn ObjectStore,
    prefix: &str,
    upload: &chelabase_storage::OpenUpload,
) -> Result<(), String> {
    if !upload.key.starts_with(prefix) || prefix.is_empty() {
        return Err(format!(
            "refusing to abort {:?}: it isn't under {prefix:?}",
            upload.key
        ));
    }
    match store.multipart_abort(&upload.key, &upload.upload_id).await {
        Ok(()) | Err(StorageError::NotFound) => Ok(()),
        Err(e) => Err(format!("aborting an upload: {e}")),
    }
}

/// A JSON record of the sweep's; `None` when there is none. A damaged one is an error.
async fn read_json<T: serde::de::DeserializeOwned>(
    store: &dyn ObjectStore,
    key: &str,
) -> Result<Option<T>, String> {
    match store.get(key, None).await {
        Ok(got) => {
            let body = chelabase_storage::collect(got.body)
                .await
                .map_err(store_err("reading the sweep's record"))?;
            serde_json::from_slice(&body)
                .map(Some)
                .map_err(|e| format!("the sweep's record {key} is damaged: {e}"))
        }
        Err(StorageError::NotFound) => Ok(None),
        Err(e) => Err(format!("reading {key}: {e}")),
    }
}

/// A deleted database without branches: every blob, open upload and mark goes, then its purge
/// marker.
async fn purge(
    store: &dyn ObjectStore,
    db: DatabaseId,
    mut report: SweepReport,
) -> Result<SweepReport, String> {
    let (blobs_at, state_at) = (blob_prefix(db), sweep_prefix(db));
    let blobs = store
        .list(&blobs_at)
        .await
        .map_err(store_err("listing the blobs"))?;
    let uploads = store
        .list_uploads(&blobs_at)
        .await
        .map_err(store_err("listing the open uploads"))?;
    let state = store
        .list(&state_at)
        .await
        .map_err(store_err("listing the marks"))?;
    report.blobs = blobs.len();
    report.deleted = blobs.len();
    report.uploads_aborted = uploads.len();
    report.purged = true;
    if report.dry_run {
        return Ok(report);
    }
    for upload in &uploads {
        abort_under(store, &blobs_at, upload).await?;
    }
    for o in &blobs {
        delete_under(store, &blobs_at, &o.key).await?;
    }
    for o in &state {
        delete_under(store, &state_at, &o.key).await?;
    }
    delete_under(store, PURGE_PREFIX, &purge_marker(db)).await?;
    Ok(report)
}

// --- Reading a branch -----------------------------------------------------------------------

/// Every database of the compute but `template0` (templates are read too: an owner can make
/// a live database one), hex-encoded with whether it takes connections (`d|hex|t`), the
/// compute's clock (`c|epoch`), then `end`.
pub const DATABASES_SQL: &str = "SET search_path = pg_catalog, pg_temp;
SELECT 'd|' || encode(convert_to(datname, 'UTF8'), 'hex') || '|' || datallowconn FROM pg_database WHERE datname <> 'template0';
SELECT 'c|' || extract(epoch FROM clock_timestamp())::text;
SELECT 'end';";

/// The storage tables a Chelabase `storage` schema can't be without.
pub const REQUIRED_TABLES: [&str; 2] = ["objects", "upload_tickets"];

/// What one database references, read as cloud_admin (past row level security) where the
/// `storage` schema is Chelabase's: the timeline it serves (`tl|…`), any required table
/// missing (`missing|…`), the versions of its objects, tickets and upload sessions (`v|…`), and
/// its unexpired sessions' upload ids (`u|hex`; `storage.upload_sessions`, m7, read only where
/// it exists). With `expire`, dead tickets (expired, or minted on another timeline) are
/// deleted first, and expired sessions whose upload isn't in `open` as `chelabase_storage`
/// (`t|count`). Ends with `end`, so a cut-off answer is never taken for a whole one.
pub fn references_sql(expire: bool, open: &[String]) -> String {
    let (tickets, sessions) = if expire {
        let open: Vec<String> = open
            .iter()
            .map(|id| format!("'{}'", hex::encode(id)))
            .collect();
        (
            "WITH d AS (DELETE FROM storage.upload_tickets t WHERE t.expires_at <= clock_timestamp() OR t.timeline_id IS DISTINCT FROM current_setting('neon.timeline_id', true) RETURNING 1) SELECT 't|' || count(*) FROM d;\n".to_string(),
            format!(
                "BEGIN;\nSET LOCAL ROLE chelabase_storage;\nWITH d AS (DELETE FROM storage.upload_sessions s WHERE s.expires_at <= clock_timestamp() AND encode(convert_to(s.upload_id::text, 'UTF8'), 'hex') <> ALL (ARRAY[{}]::text[]) RETURNING 1) SELECT 't|' || count(*) FROM d;\nCOMMIT;\n",
                open.join(", ")
            ),
        )
    } else {
        (String::new(), String::new())
    };
    let missing = REQUIRED_TABLES.map(|t| format!("'{t}'")).join(", ");
    format!(
        "SET search_path = pg_catalog, pg_temp;
SELECT 'tl|' || coalesce(current_setting('neon.timeline_id', true), '');
SELECT EXISTS (SELECT FROM pg_namespace n JOIN pg_roles r ON r.oid = n.nspowner WHERE n.nspname = 'storage' AND r.rolname = 'chelabase_admin') AS chelabase_storage_here \\gset
\\if :chelabase_storage_here
SELECT 'missing|' || t FROM unnest(ARRAY[{missing}]) t WHERE to_regclass('storage.' || t) IS NULL;
SELECT to_regclass('storage.objects') IS NOT NULL AS cb_objects, to_regclass('storage.upload_tickets') IS NOT NULL AS cb_tickets, to_regclass('storage.upload_sessions') IS NOT NULL AS cb_sessions \\gset
\\if :cb_objects
SELECT 'v|' || version_id FROM storage.objects WHERE version_id IS NOT NULL;
\\endif
\\if :cb_tickets
{tickets}SELECT 'v|' || version_id FROM storage.upload_tickets;
\\endif
\\if :cb_sessions
{sessions}SELECT 'v|' || version_id FROM storage.upload_sessions WHERE version_id IS NOT NULL;
SELECT 'u|' || encode(convert_to(upload_id::text, 'UTF8'), 'hex') FROM storage.upload_sessions WHERE expires_at > clock_timestamp();
\\endif
\\endif
SELECT 'end';"
    )
}

/// The `prefix|value` lines of a read that ended with `end`; other lines (command tags) are
/// skipped.
fn tagged<'a>(out: &'a str, prefix: &str) -> Result<Vec<&'a str>, String> {
    let lines: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.last() != Some(&"end") {
        return Err("the read didn't finish".into());
    }
    Ok(lines
        .iter()
        .filter_map(|l| l.strip_prefix(prefix))
        .collect())
}

fn unhex(h: &str) -> Result<String, String> {
    hex::decode(h)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| format!("{h:?} isn't hex UTF-8"))
}

/// The databases a [`DATABASES_SQL`] answer names (whether each takes connections) and the
/// compute's clock.
pub fn parse_databases(out: &str) -> Result<(Vec<(String, bool)>, SystemTime), String> {
    let databases = tagged(out, "d|")?
        .into_iter()
        .map(|line| {
            let (name, allow) = line
                .split_once('|')
                .ok_or_else(|| format!("a database line {line:?}"))?;
            Ok((unhex(name)?, allow == "t"))
        })
        .collect::<Result<_, String>>()?;
    let clock = match tagged(out, "c|")?.as_slice() {
        [c] => c
            .parse::<f64>()
            .ok()
            .filter(|c| c.is_finite() && *c >= 0.0)
            .map(|c| SystemTime::UNIX_EPOCH + Duration::from_secs_f64(c))
            .ok_or_else(|| format!("a clock {c:?}"))?,
        _ => return Err("no clock in the answer".into()),
    };
    Ok((databases, clock))
}

/// A [`references_sql`] answer.
pub fn parse_references(out: &str) -> Result<BranchRead, ReadError> {
    let missing = tagged(out, "missing|")?;
    if !missing.is_empty() {
        return Err(ReadError::Refused(format!(
            "Chelabase's storage schema lacks storage.{}",
            missing.join(", storage.")
        )));
    }
    let versions = tagged(out, "v|")?
        .into_iter()
        .map(|v| {
            v.parse()
                .map_err(|_| format!("a version {v:?} isn't a uuid"))
        })
        .collect::<Result<_, String>>()?;
    let deleted_rows = tagged(out, "t|")?
        .into_iter()
        .map(|n| n.parse::<u64>().map_err(|_| format!("a row count {n:?}")))
        .sum::<Result<u64, String>>()?;
    let timelines = tagged(out, "tl|")?.into_iter().map(str::to_owned).collect();
    let live_uploads = tagged(out, "u|")?
        .into_iter()
        .map(unhex)
        .collect::<Result<_, String>>()?;
    Ok(BranchRead {
        versions,
        deleted_rows,
        timelines,
        clock: None,
        live_uploads,
    })
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher> SweepSource
    for Worker<M, S, C, N, P>
{
    async fn database(&self, db: DatabaseId) -> Result<Option<SweepDatabase>, String> {
        let Some(record) = self
            .metadata
            .get_database(db)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        let branches = self
            .metadata
            .list_branches(db)
            .await
            .map_err(|e| e.to_string())?;
        let mut retention = Duration::ZERO;
        if record.status == DatabaseStatus::Ready {
            let settings = self
                .metadata
                .database_settings(db)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the database has no settings")?;
            let tenant = self
                .tenant_of(db)
                .await?
                .ok_or("the database has no tenant")?;
            let applied = self
                .storage
                .tenant_pitr_interval(&tenant)
                .await
                .map_err(|e| format!("reading the tenant's history window: {e}"))?;
            // Unset, Neon keeps its default.
            retention = settings
                .history_window
                .max(applied.unwrap_or(NEON_DEFAULT_PITR));
        }
        Ok(Some(SweepDatabase {
            status: record.status,
            branches: branches.iter().map(SweepBranch::from).collect(),
            retention,
        }))
    }

    async fn references(
        &self,
        db: DatabaseId,
        branch: &SweepBranch,
        expire: bool,
        open_uploads: &[String],
    ) -> Result<BranchRead, ReadError> {
        if branch.kind == BranchKind::Backup {
            return self.backup_references(db, branch).await;
        }
        let ep = branch.endpoint_id.as_deref().ok_or("no endpoint")?;
        self.compute_references(ep, expire, open_uploads).await
    }

    async fn branch_point(
        &self,
        db: DatabaseId,
        timeline: &str,
        recorded: &BTreeSet<String>,
    ) -> Result<Option<SystemTime>, String> {
        let Some(tenant) = self.tenant_of(db).await? else {
            return Ok(None);
        };
        let mut at: NeonId = timeline
            .parse()
            .map_err(|e| format!("timeline {timeline:?}: {e}"))?;
        // Bounded: Neon's ancestry has no cycles, but a damaged answer must not loop forever.
        for _ in 0..64 {
            let Some((parent, lsn)) = self
                .storage
                .ancestor(&tenant, &at)
                .await
                .map_err(|e| format!("reading timeline {at}'s ancestor: {e:#}"))?
            else {
                return Ok(None);
            };
            if recorded.contains(parent.as_str()) {
                let when = self
                    .storage
                    .timestamp_of_lsn(&tenant, &parent, lsn)
                    .await
                    .map_err(|e| format!("dating timeline {timeline}'s branch point: {e:#}"))?;
                return Ok(when.map(SystemTime::from));
            }
            at = parent;
        }
        Ok(None)
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Every database's references on the compute of `ep`. A database that doesn't take
    /// connections can't be read, so the pass is refused.
    async fn compute_references(
        &self,
        ep: &str,
        expire: bool,
        open_uploads: &[String],
    ) -> Result<BranchRead, ReadError> {
        let opts = SqlOpts {
            timeout: READ_TIMEOUT,
            ..SqlOpts::admin_default()
        };
        let read = |db: String, sql: String| {
            let opts = opts.clone();
            async move {
                self.computes
                    .admin_sql_with(ep, &db, &sql, &opts)
                    .await
                    .map_err(|e| format!("reading {db}: {e:#}"))
            }
        };
        let (names, clock) =
            parse_databases(&read("postgres".into(), DATABASES_SQL.into()).await?)?;
        if let Some((name, _)) = names.iter().find(|(_, allow)| !allow) {
            return Err(ReadError::Refused(format!(
                "database {name:?} doesn't take connections, so its objects can't be read"
            )));
        }
        let mut all = BranchRead {
            clock: Some(clock),
            ..BranchRead::default()
        };
        for (name, _) in names {
            let sql = references_sql(expire, open_uploads);
            let read = parse_references(&read(name.clone(), sql).await?).map_err(|e| match e {
                ReadError::Failed(e) => ReadError::Failed(format!("{name}: {e}")),
                ReadError::Refused(e) => ReadError::Refused(format!("{name}: {e}")),
            })?;
            all.versions.extend(read.versions);
            all.deleted_rows += read.deleted_rows;
            all.timelines.extend(read.timelines);
            all.live_uploads.extend(read.live_uploads);
        }
        Ok(all)
    }

    /// A backup has no compute: its references are read through a temporary static compute
    /// at its timeline's head (which nothing writes any more), removed whatever happened.
    async fn backup_references(
        &self,
        db: DatabaseId,
        b: &SweepBranch,
    ) -> Result<BranchRead, ReadError> {
        let tenant = self
            .tenant_of(db)
            .await?
            .ok_or("the database has no tenant")?;
        let timeline = b.timeline.as_deref().ok_or("the backup has no timeline")?;
        let timeline_id: NeonId = timeline
            .parse()
            .map_err(|e| format!("timeline {timeline:?}: {e}"))?;
        let head = self
            .storage
            .get_timeline(&tenant, &timeline_id)
            .await
            .map_err(|e| format!("reading the backup's head: {e:#}"))?;
        let ep = new_endpoint_id();
        let token = new_compute_token();
        self.metadata
            .create_static_endpoint(&NewStaticEndpoint {
                id: &ep,
                tenant_id: tenant.as_str(),
                timeline_id: timeline,
                compute_token_sha256: &sha256_hex(&token),
                static_lsn: &head.to_string(),
            })
            .await
            .map_err(|e| format!("recording the static compute: {e}"))?;
        let read = async {
            self.computes
                .create(&ep, &token)
                .await
                .map_err(|e| format!("starting the static compute: {e:#}"))?;
            self.wait_until_ready(&ep, "postgres").await?;
            self.compute_references(&ep, false, &[]).await
        }
        .await;
        if let Err(err) = self.remove_static_compute(&ep).await {
            warn!(endpoint_id = %ep, "removing the sweep's static compute; reconcile retries: {err}");
        }
        read
    }

    /// Delivery `attempt` (1-based) of a `chelabase.storage_sweep` job. A failure is retried
    /// [`SWEEP_ATTEMPTS`] times, then dropped (warned and counted): the next period queues the
    /// database again, so a permanent error doesn't spin the one consumer.
    pub async fn handle_storage_sweep_attempt(
        &self,
        db: DatabaseId,
        attempt: u64,
    ) -> Result<(), String> {
        match self.handle_storage_sweep(db).await {
            Err(e) if attempt >= SWEEP_ATTEMPTS => {
                let refusals = self.count_refusal();
                warn!(database_id = %db, attempt, refusals, "storage sweep failed {attempt} times; dropped until the next period: {e}");
                Ok(())
            }
            other => other,
        }
    }

    fn count_refusal(&self) -> u64 {
        self.storage_sweep_refusals
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// One pass over `db`, never two at once (a cross-process lock; a second job while one
    /// runs is dropped, the next period covers it). Without an object store configured,
    /// nothing.
    pub async fn handle_storage_sweep(&self, db: DatabaseId) -> Result<(), String> {
        let Some(store) = self.blob_store.clone() else {
            return Ok(());
        };
        let _lock = match self
            .metadata
            .lock_endpoint(&format!("storage-sweep:{db}"))
            .await
        {
            Ok(lock) => lock,
            Err(chelabase_metadata::MetadataError::EndpointLockTimeout(_)) => {
                info!(database_id = %db, "a storage sweep of this database is already running");
                return Ok(());
            }
            Err(e) => return Err(e.to_string()),
        };
        let settings = self.storage_sweep;
        let report =
            sweep_database(self, store.as_ref(), db, settings.grace, settings.dry_run).await?;
        // A refusal holds until someone acts (a branch stuck failed, a closed database, a
        // clock): the database's storage only grows meanwhile, so it's counted and named.
        if report.refused {
            let refusals = self.count_refusal();
            let (branch, status) = report.blocked_branch.map_or((None, None), |(b, s)| {
                (Some(b.to_string()), Some(s.as_str()))
            });
            warn!(
                database_id = %db,
                branch_id = ?branch,
                status = ?status,
                refusals,
                "storage sweep refused, nothing in this database is deleted: {}",
                report.blocked.as_deref().unwrap_or_default()
            );
        }
        match &report.blocked {
            Some(why) => warn!(database_id = %db, ?report, "storage sweep deleted nothing: {why}"),
            None => info!(database_id = %db, ?report, "storage sweep"),
        }
        Ok(())
    }

    /// Before a database's delete: its purge marker, so the periodic job queues the purge
    /// until it is done. Without an object store, nothing.
    pub(crate) async fn mark_purge_pending(&self, db: DatabaseId) -> Result<(), String> {
        let Some(store) = &self.blob_store else {
            return Ok(());
        };
        store
            .put(&purge_marker(db), Bytes::new(), PutOptions::default())
            .await
            .map(|_| ())
            .map_err(store_err("recording the storage purge"))
    }

    /// The periodic job: a sweep of every ready database, and a purge of every database whose
    /// purge is still pending. Each message id names the period, so the stream's duplicate
    /// window never swallows an event's request.
    pub async fn queue_storage_sweeps(&self) -> Result<usize, String> {
        let period = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            / self.storage_sweep.every.as_secs().max(1);
        let live = self
            .metadata
            .list_live_for_reconcile()
            .await
            .map_err(|e| e.to_string())?;
        let mut queued = 0;
        for d in live.iter().filter(|d| d.status == DatabaseStatus::Ready) {
            self.sweep_requests
                .request(d.id, SweepCause::Period(period));
            queued += 1;
        }
        if let Some(store) = &self.blob_store {
            for o in store
                .list(PURGE_PREFIX)
                .await
                .map_err(store_err("listing the pending purges"))?
            {
                if let Some(db) = o
                    .key
                    .strip_prefix(PURGE_PREFIX)
                    .and_then(|d| d.parse::<Uuid>().ok())
                {
                    let db = DatabaseId::from(db);
                    self.sweep_requests.request(db, SweepCause::Purge);
                    queued += 1;
                }
            }
        }
        Ok(queued)
    }
}

/// Why a sweep is queued: its message id ([`chelabase_nats::storage_sweep_msg_id`]'s tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepCause {
    /// The periodic job's period number.
    Period(u64),
    /// A branch delete, reset or restore.
    Event(Uuid),
    /// A database delete's purge.
    Purge,
}

impl SweepCause {
    pub fn tag(self) -> String {
        match self {
            Self::Period(n) => n.to_string(),
            Self::Event(id) => id.to_string(),
            Self::Purge => "purge".into(),
        }
    }
}

/// Where the worker queues sweeps (`chelabase.storage_sweep`). The process connects it to
/// JetStream once, only with an object store; until then (and in tests) requests are only
/// recorded. A failed publish is logged: the periodic job queues every ready database, and
/// every pending purge, again.
#[derive(Default)]
pub struct SweepRequests {
    js: std::sync::OnceLock<async_nats::jetstream::Context>,
    sent: std::sync::Mutex<Vec<(DatabaseId, SweepCause)>>,
}

impl SweepRequests {
    pub fn connect(&self, js: async_nats::jetstream::Context) {
        let _ = self.js.set(js);
    }

    /// Queues a sweep of `db`, off the caller's path.
    pub fn request(&self, db: DatabaseId, cause: SweepCause) {
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((db, cause));
        let Some(js) = self.js.get().cloned() else {
            return;
        };
        tokio::spawn(async move {
            let job = chelabase_nats::StorageSweepJob { database_id: db };
            if let Err(e) = chelabase_nats::publish_storage_sweep(&js, &job, &cause.tag()).await {
                warn!(database_id = %db, "storage sweep not queued: {e}");
            }
        });
    }

    /// The databases asked for so far.
    pub fn sent(&self) -> Vec<DatabaseId> {
        self.causes().into_iter().map(|(db, _)| db).collect()
    }

    /// The databases asked for so far, with why.
    pub fn causes(&self) -> Vec<(DatabaseId, SweepCause)> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The object store type the worker holds.
pub type BlobStore = Arc<dyn ObjectStore>;
