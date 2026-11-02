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
//! Why marks: a point-in-time restore or a branch made at a past point can bring back rows a
//! pass never saw. Once a blob is older than the grace period no upload can reference it any
//! more (tickets live minutes; grace must exceed the longest upload), and a move keeps its
//! version on the same branch, so only a new timeline can bring a reference back. The pass
//! records the timelines it saw (`_sweep/{database_id}/timelines`); any timeline it hasn't
//! recorded voids every mark first. With the set unchanged since a mark, the blob was
//! unreferenced on every branch from the mark on, so no restore target within the window can
//! need it once the mark is older than the window.
//!
//! A database that is `deleted` with no live branch (so no backup) loses its whole prefix.

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
///   far above the longest upload.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(8 * 86_400);

/// The shortest grace the setting accepts (see [`DEFAULT_GRACE`]).
pub const MIN_GRACE: Duration = DEFAULT_GRACE;

const _: () =
    assert!(MIN_GRACE.as_secs() >= chelabase_storage::MAX_SIGNED_URL_TTL.as_secs() + 86_400);

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

/// What a pass reads; the worker's ports in production, a model in the tests.
pub trait SweepSource: Send + Sync {
    /// The database and its live branches; `None` when there's no such row.
    fn database(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Option<SweepDatabase>, String>> + Send;
    /// The versions `branch` references in all its databases, and how many dead tickets were
    /// deleted (only with `expire`).
    fn references(
        &self,
        db: DatabaseId,
        branch: &SweepBranch,
        expire: bool,
    ) -> impl Future<Output = Result<(HashSet<Uuid>, u64), String>> + Send;
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
    pub tickets_deleted: u64,
    /// Keys under the prefix that aren't `{datoid}/{version}`: never touched.
    pub unknown_keys: usize,
    /// The whole prefix went (a deleted database).
    pub purged: bool,
    /// Why the pass stopped before deleting anything.
    pub blocked: Option<String>,
    /// The branch whose status stopped it, with that status.
    pub blocked_branch: Option<(BranchId, BranchStatus)>,
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

fn age(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
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

/// Why a pass couldn't read every branch; with the branch and its status when that was it.
type Refusal = (String, Option<(BranchId, BranchStatus)>);

/// The references of `branches` (backups only with `backups`), or why they can't all be read.
async fn enumerate<S: SweepSource>(
    source: &S,
    db: DatabaseId,
    branches: &[SweepBranch],
    expire: bool,
    backups: bool,
) -> Result<(HashSet<Uuid>, u64), Refusal> {
    let mut refs = HashSet::new();
    let mut tickets = 0;
    for b in branches {
        if b.status != BranchStatus::Ready {
            let why = format!("branch {} is {}", b.id, b.status.as_str());
            return Err((why, Some((b.id, b.status))));
        }
        if b.pending_timeline.is_some() {
            let why = format!(
                "branch {} is {} with a reset or restore in flight",
                b.id,
                b.status.as_str()
            );
            return Err((why, Some((b.id, b.status))));
        }
        if b.kind == BranchKind::Backup && !backups {
            continue;
        }
        let expire = expire && b.kind != BranchKind::Backup;
        let (found, deleted) = source
            .references(db, b, expire)
            .await
            .map_err(|e| (format!("branch {}: {e}", b.id), None))?;
        refs.extend(found);
        tickets += deleted;
    }
    Ok((refs, tickets))
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
    let mut report = SweepReport {
        dry_run,
        ..SweepReport::default()
    };
    let blocked = |mut report: SweepReport, why: String| {
        report.blocked = Some(why);
        Ok(report)
    };
    let now = store_now(store, db).await?;
    let Some(first) = source.database(db).await? else {
        return blocked(report, "no such database".into());
    };
    match first.status {
        DatabaseStatus::Ready => {}
        DatabaseStatus::Deleted if first.branches.is_empty() => {
            return purge(store, db, report).await;
        }
        DatabaseStatus::Deleted => {
            let why = format!("deleted, but {} branch(es) remain", first.branches.len());
            return blocked(report, why);
        }
        other => return blocked(report, format!("the database is {}", other.as_str())),
    }
    let (mut refs, tickets) = match enumerate(source, db, &first.branches, !dry_run, true).await {
        Ok(found) => found,
        Err((why, branch)) => {
            report.blocked_branch = branch;
            return blocked(report, why);
        }
    };
    report.tickets_deleted = tickets;
    let blobs = store
        .list_dated(&blob_prefix(db))
        .await
        .map_err(store_err("listing the blobs"))?;
    let mut marks: BTreeMap<String, SystemTime> = store
        .list_dated(&mark_prefix(db))
        .await
        .map_err(store_err("listing the marks"))?
        .into_iter()
        .filter_map(|o| {
            Some((
                o.key.strip_prefix(&mark_prefix(db))?.to_string(),
                o.modified,
            ))
        })
        .collect();
    let uploads = store
        .list_uploads(&blob_prefix(db))
        .await
        .map_err(store_err("listing the open uploads"))?;

    // Again, before anything is deleted: a branch made, reset or restored meanwhile changes
    // the timeline set.
    let Some(again) = source.database(db).await? else {
        return blocked(report, "the database row went during the pass".into());
    };
    if again.status != DatabaseStatus::Ready {
        return blocked(
            report,
            format!("the database became {}", again.status.as_str()),
        );
    }
    let seen = timelines(&again.branches);
    if seen != timelines(&first.branches) {
        return blocked(report, "the branches changed during the pass".into());
    }
    match enumerate(source, db, &again.branches, false, false).await {
        Ok((found, _)) => refs.extend(found),
        Err((why, branch)) => {
            report.blocked_branch = branch;
            return blocked(report, why);
        }
    }
    let retention = first.retention.max(again.retention);

    // A timeline these marks never saw may hold any of them: they all start over.
    let recorded_key = format!("{}timelines", sweep_prefix(db));
    let recorded = recorded_timelines(store, &recorded_key).await?;
    if recorded.as_ref() != Some(&seen) {
        if recorded.as_ref().is_none_or(|r| !seen.is_subset(r)) {
            report.marks_cleared += marks.len();
            if !dry_run {
                for key in marks.keys() {
                    delete(store, &format!("{}{key}", mark_prefix(db))).await?;
                }
            }
            marks.clear();
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
        let mark = marks.remove(rest);
        let mark_key = format!("{}{rest}", mark_prefix(db));
        if refs.contains(&version) {
            report.referenced += 1;
            if mark.is_some() {
                report.marks_cleared += 1;
                if !dry_run {
                    delete(store, &mark_key).await?;
                }
            }
        } else if age(now, blob.modified) < grace {
            report.young += 1;
        } else if let Some(marked_at) = mark {
            if age(now, marked_at) >= grace + retention {
                report.deleted += 1;
                if !dry_run {
                    delete(store, &blob.key).await?;
                    delete(store, &mark_key).await?;
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
            delete(store, &format!("{}{key}", mark_prefix(db))).await?;
        }
    }
    abort_old_uploads(store, db, &uploads, now, grace, &mut report).await?;
    Ok(report)
}

/// Aborts the open uploads older than `grace`. A store that doesn't say when an upload began
/// (SeaweedFS) has it timed by a mark (`_sweep/{db}/uploads/{hex id}`, the store's time)
/// made when a pass first sees it; marks of uploads that are gone go too.
async fn abort_old_uploads(
    store: &dyn ObjectStore,
    db: DatabaseId,
    uploads: &[chelabase_storage::OpenUpload],
    now: SystemTime,
    grace: Duration,
    report: &mut SweepReport,
) -> Result<(), String> {
    let prefix = format!("{}uploads/", sweep_prefix(db));
    let mut marks: BTreeMap<String, SystemTime> = store
        .list_dated(&prefix)
        .await
        .map_err(store_err("listing the upload marks"))?
        .into_iter()
        .filter_map(|o| Some((o.key.strip_prefix(&prefix)?.to_string(), o.modified)))
        .collect();
    for upload in uploads {
        let id = hex::encode(&upload.upload_id);
        let mark = marks.remove(&id);
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
            match store.multipart_abort(&upload.key, &upload.upload_id).await {
                Ok(()) | Err(StorageError::NotFound) => {}
                Err(e) => return Err(format!("aborting an upload: {e}")),
            }
            if mark.is_some() {
                delete(store, &format!("{prefix}{id}")).await?;
            }
        }
    }
    if !report.dry_run {
        for id in marks.keys() {
            delete(store, &format!("{prefix}{id}")).await?;
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

async fn delete(store: &dyn ObjectStore, key: &str) -> Result<(), String> {
    store
        .delete(key)
        .await
        .map_err(|e| format!("deleting {key}: {e}"))
}

async fn recorded_timelines(
    store: &dyn ObjectStore,
    key: &str,
) -> Result<Option<BTreeSet<String>>, String> {
    match store.get(key, None).await {
        Ok(got) => {
            let body = chelabase_storage::collect(got.body)
                .await
                .map_err(store_err("reading the timelines"))?;
            serde_json::from_slice(&body)
                .map(Some)
                .map_err(|e| format!("the recorded timelines: {e}"))
        }
        Err(StorageError::NotFound) => Ok(None),
        Err(e) => Err(format!("reading the timelines: {e}")),
    }
}

/// A deleted database without branches: every blob, open upload and mark goes.
async fn purge(
    store: &dyn ObjectStore,
    db: DatabaseId,
    mut report: SweepReport,
) -> Result<SweepReport, String> {
    let blobs = store
        .list(&blob_prefix(db))
        .await
        .map_err(store_err("listing the blobs"))?;
    let uploads = store
        .list_uploads(&blob_prefix(db))
        .await
        .map_err(store_err("listing the open uploads"))?;
    let state = store
        .list(&sweep_prefix(db))
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
        match store.multipart_abort(&upload.key, &upload.upload_id).await {
            Ok(()) | Err(StorageError::NotFound) => {}
            Err(e) => return Err(format!("aborting an upload: {e}")),
        }
    }
    for o in blobs.iter().chain(&state) {
        delete(store, &o.key).await?;
    }
    Ok(report)
}

// --- Reading a branch -----------------------------------------------------------------------

/// Every database of the compute, hex-encoded (`d|…`), then `end`.
pub const DATABASES_SQL: &str = "SET search_path = pg_catalog, pg_temp;
SELECT 'd|' || encode(convert_to(datname, 'UTF8'), 'hex') FROM pg_database WHERE datallowconn AND NOT datistemplate;
SELECT 'end';";

/// The versions one database references (`v|…`): its objects and its tickets, read as
/// cloud_admin (past row level security), only where the `storage` schema is Chelabase's.
/// With `expire`, its dead tickets (expired, or minted on another timeline) are deleted
/// first (`t|count`). Ends with `end`, so a cut-off answer is never taken for a whole one.
pub fn references_sql(expire: bool) -> String {
    let expire = if expire {
        "WITH d AS (DELETE FROM storage.upload_tickets t WHERE t.expires_at <= clock_timestamp() OR t.timeline_id IS DISTINCT FROM current_setting('neon.timeline_id', true) RETURNING 1) SELECT 't|' || count(*) FROM d;\n"
    } else {
        ""
    };
    format!(
        "SET search_path = pg_catalog, pg_temp;
SELECT EXISTS (SELECT FROM pg_namespace n JOIN pg_roles r ON r.oid = n.nspowner WHERE n.nspname = 'storage' AND r.rolname = 'chelabase_admin')
  AND to_regclass('storage.objects') IS NOT NULL AND to_regclass('storage.upload_tickets') IS NOT NULL AS chelabase_storage_here \\gset
\\if :chelabase_storage_here
{expire}SELECT 'v|' || v FROM (SELECT version_id AS v FROM storage.objects WHERE version_id IS NOT NULL UNION SELECT version_id FROM storage.upload_tickets) s;
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

/// The databases a [`DATABASES_SQL`] answer names.
pub fn parse_databases(out: &str) -> Result<Vec<String>, String> {
    tagged(out, "d|")?
        .into_iter()
        .map(|h| {
            hex::decode(h)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| format!("a database name {h:?} isn't hex UTF-8"))
        })
        .collect()
}

/// The versions and the deleted-ticket count a [`references_sql`] answer holds.
pub fn parse_references(out: &str) -> Result<(HashSet<Uuid>, u64), String> {
    let versions = tagged(out, "v|")?
        .into_iter()
        .map(|v| {
            v.parse()
                .map_err(|_| format!("a version {v:?} isn't a uuid"))
        })
        .collect::<Result<_, String>>()?;
    let tickets = tagged(out, "t|")?
        .into_iter()
        .map(|n| {
            n.parse::<u64>()
                .map_err(|_| format!("a ticket count {n:?}"))
        })
        .sum::<Result<u64, String>>()?;
    Ok((versions, tickets))
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
            retention = settings.history_window.max(applied.unwrap_or_default());
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
    ) -> Result<(HashSet<Uuid>, u64), String> {
        if branch.kind == BranchKind::Backup {
            return self.backup_references(db, branch).await;
        }
        let ep = branch.endpoint_id.as_deref().ok_or("no endpoint")?;
        self.compute_references(ep, expire).await
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Every database's references on the compute of `ep`.
    async fn compute_references(
        &self,
        ep: &str,
        expire: bool,
    ) -> Result<(HashSet<Uuid>, u64), String> {
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
        let names = parse_databases(&read("postgres".into(), DATABASES_SQL.into()).await?)?;
        let (mut refs, mut tickets) = (HashSet::new(), 0);
        for name in names {
            let (found, deleted) =
                parse_references(&read(name.clone(), references_sql(expire)).await?)
                    .map_err(|e| format!("{name}: {e}"))?;
            refs.extend(found);
            tickets += deleted;
        }
        Ok((refs, tickets))
    }

    /// A backup has no compute: its references are read through a temporary static compute
    /// at its timeline's head (which nothing writes any more), removed whatever happened.
    async fn backup_references(
        &self,
        db: DatabaseId,
        b: &SweepBranch,
    ) -> Result<(HashSet<Uuid>, u64), String> {
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
            self.compute_references(&ep, false).await
        }
        .await;
        if let Err(err) = self.remove_static_compute(&ep).await {
            warn!(endpoint_id = %ep, "removing the sweep's static compute; reconcile retries: {err}");
        }
        read
    }

    /// The `chelabase.storage_sweep` job: one pass over `db`, never two at once (a
    /// cross-process lock; a second job while one runs is dropped, the next period covers
    /// it). Without an object store configured, nothing.
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
        // A branch stuck in a status that refuses every pass means the database's storage only
        // grows: counted, and named in the warning.
        if let Some((branch, status)) = report.blocked_branch {
            let refusals = self
                .storage_sweep_refusals
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            warn!(
                database_id = %db,
                branch_id = %branch,
                status = status.as_str(),
                refusals,
                "storage sweep refused: a branch isn't ready, so nothing in this database is deleted"
            );
        }
        match &report.blocked {
            Some(why) => warn!(database_id = %db, ?report, "storage sweep deleted nothing: {why}"),
            None => info!(database_id = %db, ?report, "storage sweep"),
        }
        Ok(())
    }

    /// Queues a sweep of every ready database (the periodic job).
    pub async fn queue_storage_sweeps(&self) -> Result<usize, String> {
        let live = self
            .metadata
            .list_live_for_reconcile()
            .await
            .map_err(|e| e.to_string())?;
        let ready: Vec<_> = live
            .iter()
            .filter(|d| d.status == DatabaseStatus::Ready)
            .collect();
        for d in &ready {
            self.sweep_requests.request(d.id);
        }
        Ok(ready.len())
    }
}

/// Where the worker queues sweeps (`chelabase.storage_sweep`). The process connects it to
/// JetStream once; until then (and in tests) requests are only recorded. A failed publish is
/// logged: the periodic job is the backstop.
#[derive(Default)]
pub struct SweepRequests {
    js: std::sync::OnceLock<async_nats::jetstream::Context>,
    sent: std::sync::Mutex<Vec<DatabaseId>>,
}

impl SweepRequests {
    pub fn connect(&self, js: async_nats::jetstream::Context) {
        let _ = self.js.set(js);
    }

    /// Queues a sweep of `db`, off the caller's path.
    pub fn request(&self, db: DatabaseId) {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).push(db);
        let Some(js) = self.js.get().cloned() else {
            return;
        };
        tokio::spawn(async move {
            let job = chelabase_nats::StorageSweepJob { database_id: db };
            if let Err(e) = chelabase_nats::publish_storage_sweep(&js, &job).await {
                warn!(database_id = %db, "storage sweep not queued: {e}");
            }
        });
    }

    /// The databases asked for so far.
    pub fn sent(&self) -> Vec<DatabaseId> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The object store type the worker holds.
pub type BlobStore = Arc<dyn ObjectStore>;
