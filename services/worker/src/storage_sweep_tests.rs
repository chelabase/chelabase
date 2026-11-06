//! The storage sweep (object-storage m8): its rules on a hand-made source, the worker's
//! reads through the fakes, and a model of random branch histories.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use chelabase_metadata::{BranchKind, BranchStatus, DatabaseStatus};
use chelabase_storage::{MemoryStore, ObjectStore, PutOptions};
use chelabase_types::{BranchId, DatabaseId};
use uuid::Uuid;

use crate::storage_sweep::{
    BranchRead, DATABASES_SQL, ReadError, SweepBranch, SweepDatabase, SweepReport, SweepSettings,
    SweepSource, parse_databases, parse_references, references_sql, sweep_database,
};

const GRACE: Duration = Duration::from_secs(1000);
const RETENTION: Duration = Duration::from_secs(3000);
const T0: Duration = Duration::from_secs(1_000_000_000);

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + T0 + Duration::from_secs(secs)
}

fn key(db: DatabaseId, oid: u32, v: Uuid) -> String {
    format!("{db}/{oid}/{v}")
}

/// A hand-made database: branches with their references, any of them unreadable.
struct Source {
    db: DatabaseId,
    state: Mutex<State>,
}

type Change = Box<dyn FnOnce(&mut State) + Send>;

struct State {
    status: DatabaseStatus,
    branches: Vec<(SweepBranch, HashSet<Uuid>, bool)>,
    /// Runs once, at the first `references` call (a change mid-pass).
    during: Option<Change>,
    expired: Vec<BranchId>,
    /// The databases' clock (Postgres `clock_timestamp()`); `pass` sets it to the store's.
    clock: Option<SystemTime>,
    /// Each new timeline's branch point (missing: unknown).
    points: HashMap<String, SystemTime>,
    /// Upload ids live sessions name.
    live_uploads: HashSet<String>,
    /// Upload ids expired sessions name: a real read deletes these rows, unless their upload
    /// is open.
    expired_sessions: HashSet<String>,
    /// Runs once, at the second `references` call (the repeat read).
    at_second_read: Option<Change>,
    reads: u32,
    /// Every read is refused (a closed database).
    closed: bool,
}

fn branch(kind: BranchKind) -> SweepBranch {
    SweepBranch {
        id: BranchId::new(),
        kind,
        status: BranchStatus::Ready,
        timeline: Some(Uuid::new_v4().simple().to_string()),
        pending_timeline: None,
        endpoint_id: Some(format!("ep-{}", Uuid::new_v4().simple())),
    }
}

impl Source {
    fn new(branches: Vec<(SweepBranch, Vec<Uuid>)>) -> Self {
        Self {
            db: DatabaseId::new(),
            state: Mutex::new(State {
                status: DatabaseStatus::Ready,
                branches: branches
                    .into_iter()
                    .map(|(b, refs)| (b, refs.into_iter().collect(), false))
                    .collect(),
                during: None,
                expired: Vec::new(),
                clock: None,
                points: HashMap::new(),
                live_uploads: HashSet::new(),
                expired_sessions: HashSet::new(),
                at_second_read: None,
                reads: 0,
                closed: false,
            }),
        }
    }
    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
}

impl SweepSource for Source {
    async fn database(&self, db: DatabaseId) -> Result<Option<SweepDatabase>, String> {
        assert_eq!(db, self.db);
        let st = self.st();
        Ok(Some(SweepDatabase {
            status: st.status,
            branches: st.branches.iter().map(|(b, _, _)| b.clone()).collect(),
            retention: RETENTION,
        }))
    }

    async fn references(
        &self,
        _db: DatabaseId,
        b: &SweepBranch,
        expire: bool,
        open: &[String],
    ) -> Result<BranchRead, ReadError> {
        let mut st = self.st();
        if let Some(during) = st.during.take() {
            during(&mut st);
        }
        st.reads += 1;
        if st.reads == 2
            && let Some(change) = st.at_second_read.take()
        {
            change(&mut st);
        }
        let mut deleted_rows = 0;
        if expire {
            st.expired.push(b.id);
            let before = st.expired_sessions.len();
            st.expired_sessions.retain(|id| open.contains(id));
            deleted_rows = (before - st.expired_sessions.len()) as u64;
        }
        if st.closed {
            return Err(ReadError::Refused(
                "database \"x\" doesn't take connections".into(),
            ));
        }
        match st.branches.iter().find(|(x, _, _)| x.id == b.id) {
            Some((_, _, true)) => Err("compute unreachable".into()),
            Some((x, refs, _)) => Ok(BranchRead {
                versions: refs.clone(),
                timelines: x.timeline.iter().cloned().collect(),
                clock: st.clock,
                live_uploads: st.live_uploads.clone(),
                deleted_rows,
            }),
            None => Err("no such branch".into()),
        }
    }

    async fn branch_point(
        &self,
        _db: DatabaseId,
        timeline: &str,
        _recorded: &BTreeSet<String>,
    ) -> Result<Option<SystemTime>, String> {
        Ok(self.st().points.get(timeline).copied())
    }
}

async fn blob(store: &MemoryStore, db: DatabaseId, v: Uuid) -> String {
    let k = key(db, 16384, v);
    store
        .put(&k, Bytes::from_static(b"x"), PutOptions::default())
        .await
        .unwrap();
    k
}

async fn exists(store: &MemoryStore, k: &str) -> bool {
    store.head(k).await.is_ok()
}

async fn pass(src: &Source, store: &MemoryStore) -> SweepReport {
    src.st().clock = Some(store.now());
    sweep_database(src, store, src.db, GRACE, false)
        .await
        .unwrap()
}

/// A blob nobody references, old enough: marked, then deleted once the mark has outlived the
/// grace period and the history window (and not a moment before).
#[tokio::test]
async fn sweep_deletes_an_old_unreferenced_blob_only_after_its_mark_outlives_the_window() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let orphan = blob(&store, src.db, Uuid::new_v4()).await;

    store.set_now(at(GRACE.as_secs() - 1));
    let r = pass(&src, &store).await;
    assert_eq!((r.young, r.marked, r.deleted), (1, 0, 0), "{r:?}");

    store.set_now(at(GRACE.as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!((r.marked, r.deleted), (1, 0), "{r:?}");
    let marked = GRACE.as_secs();

    store.set_now(at(marked + (GRACE + RETENTION).as_secs() - 1));
    let r = pass(&src, &store).await;
    assert_eq!((r.waiting, r.deleted), (1, 0), "{r:?}");
    assert!(exists(&store, &orphan).await);

    store.set_now(at(marked + (GRACE + RETENTION).as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!(r.deleted, 1, "{r:?}");
    assert!(!exists(&store, &orphan).await);
    assert!(
        store
            .list(&format!("_sweep/{}/marks/", src.db))
            .await
            .unwrap()
            .is_empty()
    );
}

/// Audit S1: the store's clock leaps ahead of the databases' between two passes. The leap is
/// refused, not taken as the mark's wait.
#[tokio::test]
async fn audit_a_store_clock_jump_does_not_collapse_the_mark_wait() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let orphan = blob(&store, src.db, Uuid::new_v4()).await;
    store.set_now(at(GRACE.as_secs()));
    assert_eq!(pass(&src, &store).await.marked, 1);
    // 100 s of the databases' time; two days of the store's.
    src.st().clock = Some(at(GRACE.as_secs() + 100));
    store.set_now(at(GRACE.as_secs() + 2 * 86_400));
    let r = sweep_database(&src, &store, src.db, GRACE, false)
        .await
        .unwrap();
    assert_eq!(r.deleted, 0, "{r:?}");
    assert!(r.blocked.is_some(), "{r:?}");
    assert!(exists(&store, &orphan).await);
    // The same leap with the databases agreeing (and real time passing) is a normal pass.
    store.set_now(at(GRACE.as_secs()) + Duration::from_secs(5000));
    assert_eq!(pass(&src, &store).await.deleted, 1);
}

/// Audit S1: a store clock far from the databases', with no earlier pass to compare with.
#[tokio::test]
async fn sweep_refuses_a_store_clock_far_from_the_databases() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let orphan = blob(&store, src.db, Uuid::new_v4()).await;
    store.set_now(at(10_000));
    src.st().clock = Some(at(10_000 - 3601));
    let r = sweep_database(&src, &store, src.db, GRACE, false)
        .await
        .unwrap();
    assert!(
        r.refused && r.blocked.as_deref().is_some_and(|w| w.contains("apart")),
        "{r:?}"
    );
    assert_eq!(r.marked, 0);
    // Without any clock, the pass is refused too.
    src.st().clock = None;
    let r = sweep_database(&src, &store, src.db, GRACE, false)
        .await
        .unwrap();
    assert!(r.refused, "{r:?}");
    assert!(exists(&store, &orphan).await);
}

/// Audit S1: both clocks near each other now, but the store's ran far ahead of the databases'
/// since the last pass.
#[tokio::test]
async fn sweep_refuses_a_store_clock_that_outran_the_databases() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    pass(&src, &store).await;
    // Back in step only through a databases' clock jump of its own: 100 days of the store's
    // since the last pass, against a recorded 10 days of the databases'.
    let last = format!("_sweep/{}/last-pass", src.db);
    let body = format!(
        r#"{{"store":{},"databases":{}}}"#,
        secs(at(0)),
        secs(at(90 * 86_400))
    );
    store
        .put(&last, body.into(), PutOptions::default())
        .await
        .unwrap();
    store.set_now(at(100 * 86_400));
    let r = pass(&src, &store).await;
    assert!(
        r.refused && r.blocked.as_deref().is_some_and(|w| w.contains("ran")),
        "{r:?}"
    );
}

fn secs(t: SystemTime) -> f64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// Audit S2: a damaged record refuses the pass (no error to retry forever).
#[tokio::test]
async fn sweep_refuses_a_damaged_record() {
    for record in ["timelines", "last-pass"] {
        let store = MemoryStore::default();
        let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
        store
            .put(
                &format!("_sweep/{}/{record}", src.db),
                Bytes::from_static(b"{nope"),
                PutOptions::default(),
            )
            .await
            .unwrap();
        let r = pass(&src, &store).await;
        assert!(
            r.refused && r.blocked.as_deref().is_some_and(|w| w.contains("damaged")),
            "{r:?}"
        );
    }
}

/// Audit S7: a new timeline voids only the marks made after its branch point (less the skew
/// margin): a branch at head voids nothing older, one at a past point voids what came after.
#[tokio::test]
async fn sweep_voids_only_the_marks_after_a_branch_point() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let k = blob(&store, src.db, Uuid::new_v4()).await;
    store.set_now(at(10_000));
    assert_eq!(pass(&src, &store).await.marked, 1);
    // A branch at head, more than the skew margin after the mark.
    let head = branch(BranchKind::Normal);
    src.st()
        .points
        .insert(head.timeline.clone().unwrap(), at(13_700));
    src.st().branches.push((head, HashSet::new(), false));
    store.set_now(at(13_900));
    let r = pass(&src, &store).await;
    assert_eq!((r.marks_cleared, r.waiting), (0, 1), "{r:?}");
    // A branch at a point before the mark's margin ended: it may hold the version.
    let past = branch(BranchKind::Normal);
    src.st()
        .points
        .insert(past.timeline.clone().unwrap(), at(13_000));
    src.st().branches.push((past, HashSet::new(), false));
    store.set_now(at(13_950));
    let r = pass(&src, &store).await;
    assert_eq!((r.marks_cleared, r.marked, r.deleted), (1, 1, 0), "{r:?}");
    assert!(exists(&store, &k).await);
}

/// Two new timelines in one pass: the earliest branch point decides (S3, M15).
#[tokio::test]
async fn sweep_voids_from_the_earliest_of_several_branch_points() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    blob(&store, src.db, Uuid::new_v4()).await;
    store.set_now(at(10_000));
    assert_eq!(pass(&src, &store).await.marked, 1);
    for point in [13_700, 12_000] {
        let b = branch(BranchKind::Normal);
        src.st()
            .points
            .insert(b.timeline.clone().unwrap(), at(point));
        src.st().branches.push((b, HashSet::new(), false));
    }
    store.set_now(at(13_900));
    let r = pass(&src, &store).await;
    assert_eq!(
        (r.blocked.as_deref(), r.marks_cleared, r.marked, r.waiting),
        (None, 1, 1, 0),
        "{r:?}"
    );
}

/// A reference only the repeat read sees keeps a blob whose mark is ripe (S3, M7).
#[tokio::test]
async fn sweep_honours_a_reference_only_the_second_read_sees() {
    let store = MemoryStore::default();
    let v = Uuid::new_v4();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let k = blob(&store, src.db, v).await;
    store.set_now(at(GRACE.as_secs()));
    assert_eq!(pass(&src, &store).await.marked, 1);
    store.set_now(at(GRACE.as_secs() + (GRACE + RETENTION).as_secs()));
    {
        let mut st = src.st();
        st.reads = 0;
        st.at_second_read = Some(Box::new(move |st: &mut State| {
            st.branches[0].1.insert(v);
        }));
    }
    let r = pass(&src, &store).await;
    assert!(src.st().at_second_read.is_none(), "the second read never came");
    assert_eq!(
        (r.blocked.as_deref(), r.deleted, r.referenced, r.marks_cleared),
        (None, 0, 1, 1),
        "{r:?}"
    );
    assert!(exists(&store, &k).await);
}

/// A reference that comes back takes the mark with it, so a later loss waits in full (N4, M10).
#[tokio::test]
async fn sweep_drops_a_mark_when_a_reference_returns() {
    let store = MemoryStore::default();
    let v = Uuid::new_v4();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let k = blob(&store, src.db, v).await;
    store.set_now(at(GRACE.as_secs()));
    assert_eq!(pass(&src, &store).await.marked, 1);
    let marks = format!("_sweep/{}/marks/", src.db);
    assert_eq!(store.list(&marks).await.unwrap().len(), 1);
    src.st().branches[0].1.insert(v);
    store.set_now(at(2 * GRACE.as_secs()));
    assert_eq!(pass(&src, &store).await.marks_cleared, 1);
    assert!(store.list(&marks).await.unwrap().is_empty());
    src.st().branches[0].1.remove(&v);
    store.set_now(at(10 * GRACE.as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!((r.marked, r.deleted), (1, 0), "{r:?}");
    assert!(exists(&store, &k).await);
}

/// An expired session's row stays while its upload is open, and goes once it is aborted.
#[tokio::test]
async fn sweep_deletes_an_expired_session_only_after_its_upload() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let id = store
        .multipart_create(&format!("{}/16384/{}", src.db, Uuid::new_v4()), None)
        .await
        .unwrap();
    src.st().expired_sessions.insert(id.clone());
    store.set_now(at(5 * GRACE.as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!((r.tickets_deleted, r.uploads_aborted), (0, 1), "{r:?}");
    assert!(
        src.st().expired_sessions.contains(&id),
        "the row went with its upload open"
    );
    assert_eq!(pass(&src, &store).await.tickets_deleted, 1);
    assert!(src.st().expired_sessions.is_empty());
}

/// S1: a restore's detached timeline voids every mark: said once per pass, and counted, so a
/// database restored more often than the wait shows why it never reclaims anything.
#[tokio::test]
async fn sweep_reports_marks_a_restore_voided() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    blob(&store, src.db, Uuid::new_v4()).await;
    store.set_now(at(GRACE.as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!(
        (r.marked, r.voided_by_restore),
        (1, 0),
        "the first pass voids nothing: {r:?}"
    );
    src.st().branches[0].0.timeline = Some(Uuid::new_v4().simple().to_string());
    store.set_now(at(2 * GRACE.as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!(
        (r.blocked.as_deref(), r.voided_by_restore, r.marks_cleared, r.marked),
        (None, 1, 1, 1),
        "{r:?}"
    );
    // A branch with a known point voids what it must, and that isn't a restore's.
    let b = branch(BranchKind::Normal);
    src.st().points.insert(b.timeline.clone().unwrap(), at(0));
    src.st().branches.push((b, HashSet::new(), false));
    store.set_now(at(3 * GRACE.as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!((r.marks_cleared, r.voided_by_restore), (1, 0), "{r:?}");
}

/// m7: an open upload a live session names is never aborted, however old; the others are.
#[tokio::test]
async fn sweep_keeps_uploads_of_live_sessions() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let live = store
        .multipart_create(&format!("{}/16384/{}", src.db, Uuid::new_v4()), None)
        .await
        .unwrap();
    store
        .multipart_create(&format!("{}/16384/{}", src.db, Uuid::new_v4()), None)
        .await
        .unwrap();
    src.st().live_uploads.insert(live.clone());
    store.set_now(at(5 * GRACE.as_secs()));
    assert_eq!(pass(&src, &store).await.uploads_aborted, 1);
    let open = store.list_uploads("").await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].upload_id, live);
}

/// A store whose listing ignores the prefix, as audit S9 fears.
struct LeakyStore(MemoryStore);

#[async_trait::async_trait]
impl ObjectStore for LeakyStore {
    // Honest, so a pass gets as far as the purge's own listing.
    async fn list_dated(
        &self,
        p: &str,
    ) -> Result<Vec<chelabase_storage::DatedObject>, chelabase_storage::StorageError> {
        self.0.list_dated(p).await
    }
    async fn multipart_part_copy(
        &self,
        k: &str,
        u: &str,
        n: u32,
        s: &str,
        r: std::ops::Range<u64>,
    ) -> Result<String, chelabase_storage::StorageError> {
        self.0.multipart_part_copy(k, u, n, s, r).await
    }
    async fn put(
        &self,
        k: &str,
        b: Bytes,
        o: PutOptions,
    ) -> Result<chelabase_storage::ObjectMeta, chelabase_storage::StorageError> {
        self.0.put(k, b, o).await
    }
    async fn get(
        &self,
        k: &str,
        r: Option<chelabase_storage::ByteRange>,
    ) -> Result<chelabase_storage::GetObject, chelabase_storage::StorageError> {
        self.0.get(k, r).await
    }
    async fn head(
        &self,
        k: &str,
    ) -> Result<chelabase_storage::ObjectMeta, chelabase_storage::StorageError> {
        self.0.head(k).await
    }
    async fn delete(&self, k: &str) -> Result<(), chelabase_storage::StorageError> {
        self.0.delete(k).await
    }
    async fn copy(
        &self,
        s: &str,
        d: &str,
    ) -> Result<chelabase_storage::ObjectMeta, chelabase_storage::StorageError> {
        self.0.copy(s, d).await
    }
    async fn list(
        &self,
        _p: &str,
    ) -> Result<Vec<chelabase_storage::ObjectMeta>, chelabase_storage::StorageError> {
        self.0.list("").await
    }
    async fn multipart_create(
        &self,
        k: &str,
        c: Option<&str>,
    ) -> Result<String, chelabase_storage::StorageError> {
        self.0.multipart_create(k, c).await
    }
    async fn multipart_part(
        &self,
        k: &str,
        u: &str,
        n: u32,
        b: Bytes,
    ) -> Result<String, chelabase_storage::StorageError> {
        self.0.multipart_part(k, u, n, b).await
    }
    async fn multipart_complete(
        &self,
        k: &str,
        u: &str,
        p: Vec<chelabase_storage::CompletedPart>,
    ) -> Result<chelabase_storage::ObjectMeta, chelabase_storage::StorageError> {
        self.0.multipart_complete(k, u, p).await
    }
    async fn multipart_abort(
        &self,
        k: &str,
        u: &str,
    ) -> Result<(), chelabase_storage::StorageError> {
        self.0.multipart_abort(k, u).await
    }
    async fn list_uploads(
        &self,
        _p: &str,
    ) -> Result<Vec<chelabase_storage::OpenUpload>, chelabase_storage::StorageError> {
        self.0.list_uploads("").await
    }
}

/// Audit S9: a purge never touches a key outside its prefix, whatever a listing returns.
#[tokio::test]
async fn sweep_purge_stays_inside_its_prefix() {
    let store = LeakyStore(MemoryStore::default());
    let src = Source::new(vec![]);
    let theirs = format!("{}/16384/{}", DatabaseId::new(), Uuid::new_v4());
    store
        .0
        .put(&theirs, Bytes::new(), PutOptions::default())
        .await
        .unwrap();
    src.st().status = DatabaseStatus::Deleted;
    let refused = sweep_database(&src, &store, src.db, GRACE, false)
        .await
        .unwrap_err();
    assert!(refused.contains("refusing to delete"), "{refused}");
    assert!(exists(&store.0, &theirs).await);
}

/// Audit N1: a mark older than its blob (a key written again) is not the blob's wait.
#[tokio::test]
async fn audit_a_mark_older_than_its_blob_is_not_honoured() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let v = Uuid::new_v4();
    blob(&store, src.db, v).await;
    store.set_now(at(GRACE.as_secs()));
    assert_eq!(pass(&src, &store).await.marked, 1);
    store.set_now(at(10_000));
    let k = blob(&store, src.db, v).await;
    store.set_now(at(10_000 + GRACE.as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!((r.deleted, r.marked), (0, 1), "{r:?}");
    assert!(exists(&store, &k).await);
}

/// Deleted on one branch, kept on another (or moved: the same version): it stays, and its
/// mark goes when a reference comes back.
#[tokio::test]
async fn sweep_keeps_a_blob_any_branch_references() {
    let store = MemoryStore::default();
    let (kept, gone) = (Uuid::new_v4(), Uuid::new_v4());
    let src = Source::new(vec![
        (branch(BranchKind::Normal), vec![]),
        (branch(BranchKind::Normal), vec![kept]),
        (branch(BranchKind::Backup), vec![gone]),
    ]);
    store.set_now(at(0));
    let k1 = blob(&store, src.db, kept).await;
    let k2 = blob(&store, src.db, gone).await;
    for step in 1..=10 {
        store.set_now(at(step * 2000));
        let r = pass(&src, &store).await;
        assert_eq!(r.referenced, 2, "{r:?}");
    }
    assert!(exists(&store, &k1).await && exists(&store, &k2).await);
    // The backup expires: its blob follows, after the mark's wait.
    src.st()
        .branches
        .retain(|(b, _, _)| b.kind != BranchKind::Backup);
    for step in 11..=20 {
        store.set_now(at(step * 2000));
        pass(&src, &store).await;
    }
    assert!(exists(&store, &k1).await);
    assert!(!exists(&store, &k2).await);
}

/// m7 (write-once): an object in a write-once bucket is never updated or deleted (the
/// database refuses it), so its row, and with it its version, stays on every read for ever:
/// the sweep sees the version referenced on every pass and never marks or deletes its blob,
/// however many grace periods and history windows go by, while an unreferenced blob next to
/// it is marked and deleted on time. The sweep needs no notion of the bucket for that: its
/// reads delete only dead tickets and expired sessions, never a row of `storage.objects` or
/// `storage.buckets`, and the guards fire on no SELECT.
#[tokio::test]
async fn sweep_never_touches_a_write_once_objects_blob() {
    let store = MemoryStore::default();
    let frozen = Uuid::new_v4();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![frozen])]);
    store.set_now(at(0));
    let kept = blob(&store, src.db, frozen).await;
    let stray = blob(&store, src.db, Uuid::new_v4()).await;
    let window = (GRACE + RETENTION).as_secs();
    let mut deleted = 0;
    for step in 1..=40 {
        store.set_now(at(step * window));
        let r = pass(&src, &store).await;
        assert_eq!((r.referenced, r.blocked.clone()), (1, None), "{r:?}");
        deleted += r.deleted;
    }
    assert_eq!(deleted, 1, "the stray blob went");
    assert!(exists(&store, &kept).await);
    assert!(!exists(&store, &stray).await);
    assert!(
        store
            .list(&format!("_sweep/{}/marks/", src.db))
            .await
            .unwrap()
            .is_empty(),
        "no mark on a referenced blob"
    );
    for expire in [false, true] {
        let sql = references_sql(expire, &["u-1".to_string()]);
        for line in sql.lines() {
            let writes = line.contains("DELETE") || line.contains("UPDATE") || line.contains("INSERT");
            assert!(
                !writes || line.contains("DELETE FROM storage.upload_tickets") || line.contains("DELETE FROM storage.upload_sessions"),
                "a sweep read writes elsewhere: {line}"
            );
            assert!(!(writes && (line.contains("storage.objects") || line.contains("storage.buckets"))), "{line}");
        }
    }
}

/// One branch that can't be read: nothing in the database is deleted, and the pass says why.
#[tokio::test]
async fn sweep_fails_closed_on_an_unreadable_branch() {
    let store = MemoryStore::default();
    let src = Source::new(vec![
        (branch(BranchKind::Normal), vec![]),
        (branch(BranchKind::Normal), vec![]),
    ]);
    store.set_now(at(0));
    let orphan = blob(&store, src.db, Uuid::new_v4()).await;
    store.set_now(at(2000));
    pass(&src, &store).await;
    src.st().branches[1].2 = true;
    store.set_now(at(100_000));
    let r = pass(&src, &store).await;
    let why = r.blocked.clone().expect("blocked");
    assert!(why.contains("compute unreachable"), "{why}");
    assert_eq!(r.deleted, 0);
    assert!(exists(&store, &orphan).await);
    // A branch mid-reset, or not ready, blocks too.
    src.st().branches[1].2 = false;
    src.st().branches[1].0.pending_timeline = Some("p".into());
    assert!(
        pass(&src, &store)
            .await
            .blocked
            .unwrap()
            .contains("in flight")
    );
    src.st().branches[1].0.pending_timeline = None;
    src.st().branches[1].0.status = BranchStatus::Failed;
    let failed = src.st().branches[1].0.id;
    let r = pass(&src, &store).await;
    let why = r.blocked.clone().unwrap();
    assert!(
        why.contains(&failed.to_string()) && why.contains("failed"),
        "{why}"
    );
    // The branch and its status, for the log and the refusal count.
    assert_eq!(r.blocked_branch, Some((failed, BranchStatus::Failed)));
    assert!(exists(&store, &orphan).await);
    src.st().status = DatabaseStatus::Deleting;
    assert!(
        pass(&src, &store)
            .await
            .blocked
            .unwrap()
            .contains("deleting")
    );
    assert!(exists(&store, &orphan).await);
}

/// A branch made while the pass reads: the repeated enumeration sees it and stops.
#[tokio::test]
async fn sweep_stops_when_a_branch_appears_mid_pass() {
    let store = MemoryStore::default();
    let v = Uuid::new_v4();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let k = blob(&store, src.db, v).await;
    store.set_now(at(2000));
    pass(&src, &store).await; // marked
    store.set_now(at(100_000));
    src.st().during = Some(Box::new(move |st: &mut State| {
        st.branches
            .push((branch(BranchKind::Normal), [v].into(), false));
    }));
    let r = pass(&src, &store).await;
    assert_eq!(
        r.blocked.as_deref(),
        Some("the branches changed during the pass"),
        "{r:?}"
    );
    assert!(exists(&store, &k).await);
}

/// A timeline the marks never saw (a restore, a reset, a branch at a past point) voids them:
/// the blob waits the whole window again from the next mark.
#[tokio::test]
async fn sweep_starts_marks_over_after_a_new_timeline() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let k = blob(&store, src.db, Uuid::new_v4()).await;
    store.set_now(at(2000));
    assert_eq!(pass(&src, &store).await.marked, 1);
    // A restore: the branch's timeline changes between passes.
    src.st().branches[0].0.timeline = Some("restored".into());
    store.set_now(at(2000 + 4000));
    let r = pass(&src, &store).await;
    assert_eq!((r.marks_cleared, r.marked, r.deleted), (1, 1, 0), "{r:?}");
    store.set_now(at(2000 + 4000 + 3999));
    assert_eq!(pass(&src, &store).await.deleted, 0);
    assert!(exists(&store, &k).await);
    store.set_now(at(2000 + 4000 + 4000));
    assert_eq!(pass(&src, &store).await.deleted, 1);
    // A branch deleted (a timeline fewer) voids nothing.
    src.st()
        .branches
        .push((branch(BranchKind::Normal), HashSet::new(), false));
    store.set_now(at(20_000));
    pass(&src, &store).await;
    let k = blob(&store, src.db, Uuid::new_v4()).await;
    store.set_now(at(22_000));
    assert_eq!(pass(&src, &store).await.marked, 1);
    src.st().branches.pop();
    store.set_now(at(26_000));
    let r = pass(&src, &store).await;
    assert_eq!((r.marks_cleared, r.deleted), (0, 1), "{r:?}");
    assert!(!exists(&store, &k).await);
}

/// A dry run counts and changes nothing: no mark, no delete, no abort, no ticket.
#[tokio::test]
async fn sweep_dry_run_deletes_nothing() {
    let store = MemoryStore::default();
    let b = branch(BranchKind::Normal);
    let src = Source::new(vec![(b.clone(), vec![])]);
    store.set_now(at(0));
    let k = blob(&store, src.db, Uuid::new_v4()).await;
    let upload = format!("{}/16384/{}", src.db, Uuid::new_v4());
    store.multipart_create(&upload, None).await.unwrap();
    store.set_now(at(2000));
    pass(&src, &store).await; // a real pass marks it
    store.multipart_create(&upload, None).await.unwrap();
    let before = store.list("").await.unwrap();
    src.st().expired.clear();
    store.set_now(at(100_000));
    src.st().clock = Some(store.now());
    let r = sweep_database(&src, &store, src.db, GRACE, true)
        .await
        .unwrap();
    assert!(r.dry_run);
    assert_eq!((r.deleted, r.uploads_aborted), (1, 1), "{r:?}");
    assert!(exists(&store, &k).await);
    assert_eq!(store.uploads_open(), 1);
    assert!(src.st().expired.is_empty(), "a dry run deleted tickets");
    let after: Vec<_> = store.list("").await.unwrap();
    // Only the clock probe is rewritten.
    assert_eq!(before.len(), after.len());
}

/// Open multipart uploads older than the grace period are aborted; younger ones aren't.
#[tokio::test]
async fn sweep_aborts_only_old_open_uploads() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let old = format!("{}/16384/{}", src.db, Uuid::new_v4());
    store.multipart_create(&old, None).await.unwrap();
    store.set_now(at(GRACE.as_secs()));
    let young = format!("{}/16384/{}", src.db, Uuid::new_v4());
    let id = store.multipart_create(&young, None).await.unwrap();
    // Another database's upload is never this one's.
    store
        .multipart_create(&format!("{}/16384/x", DatabaseId::new()), None)
        .await
        .unwrap();
    let r = pass(&src, &store).await;
    assert_eq!(r.uploads_aborted, 1, "{r:?}");
    let open = store.list_uploads("").await.unwrap();
    assert_eq!(open.len(), 2);
    assert!(open.iter().any(|u| u.upload_id == id));
}

/// A store that doesn't say when an upload began (SeaweedFS): the upload is marked when first
/// seen, and aborted once the mark is older than the grace period.
#[tokio::test]
async fn sweep_times_undated_uploads_by_their_mark() {
    let store = MemoryStore::default();
    store.hide_upload_times();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let k = format!("{}/16384/{}", src.db, Uuid::new_v4());
    store.multipart_create(&k, None).await.unwrap();
    store.set_now(at(5 * GRACE.as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!(r.uploads_aborted, 0, "{r:?}");
    store.set_now(at(6 * GRACE.as_secs() - 1));
    assert_eq!(pass(&src, &store).await.uploads_aborted, 0);
    assert_eq!(store.uploads_open(), 1);
    store.set_now(at(6 * GRACE.as_secs()));
    assert_eq!(pass(&src, &store).await.uploads_aborted, 1);
    assert_eq!(store.uploads_open(), 0);
    // Its mark goes with it.
    pass(&src, &store).await;
    assert!(
        store
            .list(&format!("_sweep/{}/uploads/", src.db))
            .await
            .unwrap()
            .is_empty()
    );
}

/// Dead tickets are deleted on branches with a compute, never through a backup's.
#[tokio::test]
async fn sweep_expires_tickets_on_live_branches_only() {
    let store = MemoryStore::default();
    let (live, backup) = (branch(BranchKind::Normal), branch(BranchKind::Backup));
    let src = Source::new(vec![(live.clone(), vec![]), (backup, vec![])]);
    pass(&src, &store).await;
    assert_eq!(src.st().expired, vec![live.id]);
}

/// A deleted database with no branch left (so no backup) loses its whole prefix; one whose
/// branches remain keeps it.
#[tokio::test]
async fn sweep_purges_a_deleted_database_once_its_branches_are_gone() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Backup), vec![])]);
    store.set_now(at(0));
    let k = blob(&store, src.db, Uuid::new_v4()).await;
    let other = DatabaseId::new();
    let theirs = blob(&store, other, Uuid::new_v4()).await;
    store
        .multipart_create(&format!("{}/1/u", src.db), None)
        .await
        .unwrap();
    src.st().status = DatabaseStatus::Deleted;
    let r = pass(&src, &store).await;
    assert!(r.blocked.unwrap().contains("remain"));
    assert!(exists(&store, &k).await);
    src.st().branches.clear();
    let r = pass(&src, &store).await;
    assert!(
        r.purged && r.deleted == 1 && r.uploads_aborted == 1,
        "{r:?}"
    );
    assert!(!exists(&store, &k).await);
    assert!(exists(&store, &theirs).await);
    assert!(
        store
            .list(&format!("_sweep/{}/", src.db))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.uploads_open(), 0);
}

/// Keys that aren't `{datoid}/{version}` are never touched.
#[tokio::test]
async fn sweep_leaves_unknown_keys_alone() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let k = format!("{}/notanoid/{}", src.db, Uuid::new_v4());
    store
        .put(&k, Bytes::new(), PutOptions::default())
        .await
        .unwrap();
    for step in 1..=5 {
        store.set_now(at(step * 5000));
        assert_eq!(pass(&src, &store).await.unknown_keys, 1);
    }
    assert!(exists(&store, &k).await);
}

#[test]
fn sweep_reads_parse_only_whole_answers() {
    let v = Uuid::new_v4();
    let read = parse_references(&format!(
        "SET\ntl|abc\nt|3\nt|1\nv|{v}\nu|{}\nend\n",
        hex::encode("up 1")
    ))
    .unwrap();
    assert_eq!(read.versions, [v].into());
    assert_eq!(read.deleted_rows, 4);
    assert_eq!(read.timelines, ["abc".to_string()].into());
    assert_eq!(read.live_uploads, ["up 1".to_string()].into());
    // Audit S5: Chelabase's schema without a required table refuses the pass.
    assert!(matches!(
        parse_references("tl|abc\nmissing|upload_tickets\nend"),
        Err(ReadError::Refused(e)) if e.contains("storage.upload_tickets")
    ));
    assert!(parse_references(&format!("v|{v}\n")).is_err(), "cut off");
    assert!(parse_references("v|nope\nend").is_err());
    let (dbs, clock) = parse_databases(&format!(
        "d|{}|true\nd|{}|false\nc|1000000000.5\nend",
        hex::encode("app"),
        hex::encode("a|b\nc")
    ))
    .unwrap();
    assert_eq!(
        dbs,
        [("app".to_string(), true), ("a|b\nc".to_string(), false)]
    );
    assert_eq!(
        clock,
        SystemTime::UNIX_EPOCH + Duration::from_secs_f64(1_000_000_000.5)
    );
    assert!(parse_databases("").is_err());
    assert!(parse_databases("d|61|true\nend").is_err(), "no clock");
    // What Postgres writes for a boolean joined to text; anything else is refused, not read
    // as closed.
    assert!(
        parse_databases("d|61|t\nc|1\nend").is_err(),
        "a value that isn't true or false"
    );
    // Audit B1: templates are read, only template0 is left out; closed ones are named.
    assert!(DATABASES_SQL.contains("WHERE datname <> 'template0'"));
    assert!(!DATABASES_SQL.contains("datistemplate"));
    let open = ["u-1".to_string()];
    let sql = references_sql(true, &open);
    assert!(sql.contains("DELETE FROM storage.upload_tickets"));
    // Audit S5 and m7: each table read only where it exists; expired sessions deleted, never
    // one whose upload is still open. Audit round 2 N5: no role switch for it (the guard
    // trigger fires on inserts and updates only), as for the tickets.
    assert!(!sql.contains("SET LOCAL ROLE"), "{sql}");
    for needle in [
        "\\if :cb_objects",
        "\\if :cb_tickets",
        "\\if :cb_sessions",
        "WITH d AS (DELETE FROM storage.upload_sessions s WHERE s.expires_at <= clock_timestamp()",
        &format!("ARRAY['{}']::text[]", hex::encode("u-1")),
        "SELECT 'tl|' || coalesce(current_setting('neon.timeline_id', true), '');",
        "WHERE expires_at > clock_timestamp();",
    ] {
        assert!(sql.contains(needle), "{needle}");
    }
    assert!(!references_sql(false, &open).contains("DELETE"));
    for sql in [references_sql(true, &[]), DATABASES_SQL.to_string()] {
        assert!(sql.starts_with("SET search_path = pg_catalog, pg_temp;"));
        assert!(sql.ends_with("SELECT 'end';"));
    }
}

#[test]
fn sweep_settings_keep_the_grace_above_the_longest_upload() {
    let get = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    };
    assert_eq!(
        SweepSettings::from_lookup(get(&[])).unwrap(),
        SweepSettings::default()
    );
    // m6: a signed URL pins a version for up to MAX_SIGNED_URL_TTL after its row goes; the
    // grace (and so every mark's wait) outlives it by a day, the default and the floor alike.
    let day = Duration::from_secs(86_400);
    assert_eq!(SweepSettings::default().grace, 8 * day);
    assert!(SweepSettings::default().grace >= chelabase_storage::MAX_SIGNED_URL_TTL + day);
    assert!(crate::storage_sweep::MIN_GRACE >= chelabase_storage::MAX_SIGNED_URL_TTL + day);
    let s = SweepSettings::from_lookup(get(&[
        ("CHELABASE_STORAGE_SWEEP_GRACE_SECS", "864000"),
        ("CHELABASE_STORAGE_SWEEP_SECS", "600"),
        ("CHELABASE_STORAGE_SWEEP_DRY_RUN", "true"),
    ]))
    .unwrap();
    assert_eq!(
        (s.grace.as_secs(), s.every.as_secs(), s.dry_run),
        (864_000, 600, true)
    );
    for bad in ["3600", "604800", "691199", "0", "x", "99999999"] {
        let pairs: &'static [(&str, &str)] =
            Box::leak(vec![("CHELABASE_STORAGE_SWEEP_GRACE_SECS", bad)].into_boxed_slice());
        assert!(SweepSettings::from_lookup(get(pairs)).is_err(), "{bad}");
    }
    assert!(
        SweepSettings::from_lookup(get(&[("CHELABASE_STORAGE_SWEEP_DRY_RUN", "maybe")])).is_err()
    );
}

// --- The worker's reads, through the fakes ---------------------------------------------------

mod worker_reads {
    use super::*;
    use crate::fakes::{ready_branch, ready_database, restored_by_hand, worker};
    use chelabase_nats::BranchJob;

    fn refs(timeline: &str, vs: &[Uuid]) -> String {
        let mut out = format!("tl|{timeline}\n");
        out.extend(vs.iter().map(|v| format!("v|{v}\n")));
        out.push_str("end\n");
        out
    }

    /// The databases answer, with the compute's clock at `now`.
    fn databases(names: &[&str], now: SystemTime) -> String {
        let mut out: String = names
            .iter()
            .map(|n| format!("d|{}|true\n", hex::encode(n)))
            .collect();
        let secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        out.push_str(&format!("c|{secs}\nend\n"));
        out
    }

    fn timeline(w: &crate::fakes::FakeWorker, b: BranchId) -> String {
        w.metadata.branch(b).timeline_id.unwrap()
    }

    /// An object in the branch's second database survives, as does one only a backup (read
    /// through a static compute, removed after) references.
    #[tokio::test]
    async fn sweep_reads_every_database_and_backups() {
        let (mut w, _journal) = worker();
        let store = Arc::new(MemoryStore::default());
        w.blob_store = Some(store.clone());
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let backup = restored_by_hand(&w, main.id, "before");
        let ep = w.metadata.main(db).endpoint_id.clone().unwrap();
        let (main_tl, backup_tl) = (timeline(&w, main.id), timeline(&w, backup));
        let (in_other, in_backup, orphan) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let answer = |now: SystemTime| {
            w.computes.answers_at.lock().unwrap().clear();
            w.computes.answers.lock().unwrap().clear();
            w.computes.answers_in.lock().unwrap().clear();
            w.computes
                .answer_at(&ep, "datallowconn", &databases(&["app", "other"], now));
            // Everything else (the backup's static compute) has its own database.
            w.computes
                .answer("datallowconn", &databases(&["kept"], now));
            w.computes
                .answer_in("app", "chelabase_storage_here", &refs(&main_tl, &[]));
            w.computes.answer_in(
                "other",
                "chelabase_storage_here",
                &refs(&main_tl, &[in_other]),
            );
            w.computes.answer_in(
                "kept",
                "chelabase_storage_here",
                &refs(&backup_tl, &[in_backup]),
            );
        };
        store.set_now(at(0));
        let keys = [
            blob(&store, db, in_other).await,
            blob(&store, db, in_backup).await,
            blob(&store, db, orphan).await,
        ];
        store.set_now(at(86_400 * 40));
        answer(at(86_400 * 40));
        w.computes.sql.lock().unwrap().clear();
        let grace = Duration::from_secs(60);
        let r = sweep_database(&w, store.as_ref(), db, grace, false)
            .await
            .unwrap();
        assert_eq!(r.blocked, None, "{r:?}");
        assert_eq!((r.referenced, r.marked), (2, 1), "{r:?}");
        // Live databases have tickets expired; the backup's static compute doesn't.
        let sql = w.computes.sql.lock().unwrap().clone();
        let deleting: Vec<_> = sql
            .iter()
            .filter(|(_, _, s)| {
                s.contains("chelabase_storage_here")
                    && s.contains("DELETE FROM storage.upload_tickets")
            })
            .collect();
        assert_eq!(deleting.len(), 2, "app and other on main: {deleting:?}");
        assert!(deleting.iter().all(|(e, _, _)| *e == ep));
        assert!(
            sql.iter().any(|(e, _, _)| *e != ep),
            "the backup was read on its own compute"
        );
        assert!(
            w.metadata.endpoint_ids().iter().all(|e| *e == ep),
            "the static compute's row went"
        );
        // A mark later (past the history window, Neon's 7 days when unset), only the orphan
        // goes.
        store.set_now(at(86_400 * 60));
        answer(at(86_400 * 60));
        let r = sweep_database(&w, store.as_ref(), db, grace, false)
            .await
            .unwrap();
        assert_eq!(r.deleted, 1, "{r:?}");
        assert!(exists(&store, &keys[0]).await && exists(&store, &keys[1]).await);
        assert!(!exists(&store, &keys[2]).await);
    }

    /// Audit B1: a database that doesn't take connections can't be read: the pass is refused
    /// and counted.
    #[tokio::test]
    async fn sweep_refuses_a_database_closed_to_connections() {
        let (mut w, _journal) = worker();
        let store = Arc::new(MemoryStore::default());
        w.blob_store = Some(store.clone());
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let now = SystemTime::now();
        let secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        w.computes.answer(
            "datallowconn",
            &format!(
                "d|{}|true\nd|{}|false\nc|{secs}\nend\n",
                hex::encode("app"),
                hex::encode("shut")
            ),
        );
        w.computes
            .answer("chelabase_storage_here", &refs(&timeline(&w, main.id), &[]));
        let r = sweep_database(&w, store.as_ref(), db, GRACE, false)
            .await
            .unwrap();
        let why = r.blocked.clone().expect("refused");
        assert!(why.contains("\"shut\"") && r.refused, "{r:?}");
        w.handle_storage_sweep(db).await.unwrap();
        assert_eq!(
            w.storage_sweep_refusals
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    /// Audit S4: a compute serving another timeline than its branch's is refused.
    #[tokio::test]
    async fn sweep_refuses_a_compute_on_the_wrong_timeline() {
        let (mut w, _journal) = worker();
        let store = Arc::new(MemoryStore::default());
        w.blob_store = Some(store.clone());
        let db = ready_database(&w).await;
        w.computes
            .answer("datallowconn", &databases(&["app"], SystemTime::now()));
        w.computes.answer(
            "chelabase_storage_here",
            &refs("0123456789abcdef0123456789abcdef", &[]),
        );
        let r = sweep_database(&w, store.as_ref(), db, GRACE, false)
            .await
            .unwrap();
        let why = r.blocked.clone().expect("refused");
        assert!(why.contains("serves timeline") && r.refused, "{r:?}");
    }

    /// Audit S7: a timeline branched from a recorded one at P voids only the marks made after
    /// P (less the skew margin), read through Neon's ancestry and the point's time.
    #[tokio::test]
    async fn sweep_branch_points_come_from_neons_ancestry() {
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let dev = ready_branch(&w, db, "dev", main.id).await;
        let (main_tl, dev_tl) = (timeline(&w, main.id), timeline(&w, dev));
        let when = chrono::Utc::now() - chrono::Duration::days(3);
        *w.storage.timestamp_at.lock().unwrap() = Some(when);
        let recorded: BTreeSet<String> = [main_tl.clone()].into();
        assert_eq!(
            w.branch_point(db, &dev_tl, &recorded).await.unwrap(),
            Some(SystemTime::from(when))
        );
        // No recorded ancestor: unknown.
        assert_eq!(
            w.branch_point(db, &dev_tl, &BTreeSet::new()).await.unwrap(),
            None
        );
        assert_eq!(w.branch_point(db, &main_tl, &recorded).await.unwrap(), None);
    }

    /// A branch delete queues a sweep of its database; a database delete too.
    #[tokio::test]
    async fn sweep_is_queued_after_a_branch_or_database_delete() {
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let dev = ready_branch(&w, db, "dev", main.id).await;
        assert!(w.sweep_requests.sent().is_empty());
        w.handle_branch_job(BranchJob::Delete { branch_id: dev })
            .await
            .unwrap();
        assert_eq!(w.sweep_requests.sent(), [db]);
        w.handle_delete(db).await.unwrap();
        assert_eq!(w.sweep_requests.sent(), [db, db]);
        // The periodic job queues every ready database.
        let other = ready_database(&w).await;
        assert_eq!(w.queue_storage_sweeps().await.unwrap(), 1);
        assert_eq!(w.sweep_requests.sent().last(), Some(&other));
    }

    /// A pass a branch's status refuses is counted (and logged with the database, branch and
    /// status): a branch stuck failed means storage only grows.
    #[tokio::test]
    async fn sweep_refusals_by_a_branch_status_are_counted() {
        use std::sync::atomic::Ordering::Relaxed;
        let (mut w, _journal) = worker();
        let store = Arc::new(MemoryStore::default());
        w.blob_store = Some(store.clone());
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        w.computes
            .answer("datallowconn", &databases(&["app"], SystemTime::now()));
        w.computes
            .answer("chelabase_storage_here", &refs(&timeline(&w, main.id), &[]));
        w.handle_storage_sweep(db).await.unwrap();
        assert_eq!(w.storage_sweep_refusals.load(Relaxed), 0);
        w.metadata
            .state()
            .branches
            .get_mut(&main.id)
            .unwrap()
            .status = BranchStatus::Failed;
        w.handle_storage_sweep(db).await.unwrap();
        w.handle_storage_sweep(db).await.unwrap();
        assert_eq!(w.storage_sweep_refusals.load(Relaxed), 2);
    }

    /// Audit B2: a deleted database is queued again by the periodic job until its prefix is
    /// purged, so a lost or swallowed purge request isn't lost for good.
    #[tokio::test]
    async fn audit_a_deleted_database_is_queued_again_by_the_periodic_job() {
        let (mut w, _journal) = worker();
        let store = Arc::new(MemoryStore::default());
        w.blob_store = Some(store.clone());
        let db = ready_database(&w).await;
        blob(&store, db, Uuid::new_v4()).await;
        w.handle_delete(db).await.unwrap();
        assert_eq!(w.sweep_requests.sent(), [db]);
        w.queue_storage_sweeps().await.unwrap();
        assert_eq!(
            w.sweep_requests.sent(),
            [db, db],
            "the periodic job forgot the deleted database"
        );
        // Once purged, it isn't queued any more.
        w.handle_storage_sweep(db).await.unwrap();
        assert!(store.list(&format!("{db}/")).await.unwrap().is_empty());
        w.queue_storage_sweeps().await.unwrap();
        assert_eq!(w.sweep_requests.sent(), [db, db]);
    }

    /// Audit S6: a tenant without `pitr_interval` keeps Neon's default, longer than the
    /// database's own window may be.
    #[tokio::test]
    async fn sweep_takes_neons_default_for_an_unset_history() {
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        w.metadata
            .state()
            .databases
            .get_mut(&db)
            .unwrap()
            .history_window = Duration::from_secs(3600);
        w.storage.pitr.lock().unwrap().clear();
        let d = w.database(db).await.unwrap().unwrap();
        assert_eq!(d.retention, crate::storage_sweep::NEON_DEFAULT_PITR);
    }

    /// S1: the worker counts a pass that a restore's void started over.
    #[tokio::test]
    async fn sweep_counts_passes_a_restore_voided() {
        use std::sync::atomic::Ordering::Relaxed;
        let (mut w, _journal) = worker();
        let store = Arc::new(MemoryStore::default());
        w.blob_store = Some(store.clone());
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        store.set_now(at(0));
        blob(&store, db, Uuid::new_v4()).await;
        let answer = |now: SystemTime, tl: String| {
            w.computes.answers.lock().unwrap().clear();
            w.computes.answer("datallowconn", &databases(&["app"], now));
            w.computes.answer("chelabase_storage_here", &refs(&tl, &[]));
        };
        store.set_now(at(86_400 * 30));
        answer(at(86_400 * 30), timeline(&w, main.id));
        w.storage_sweep.grace = Duration::from_secs(60);
        w.handle_storage_sweep(db).await.unwrap();
        assert_eq!(w.storage_sweep_restore_voids.load(Relaxed), 0);
        // The new timeline has no recorded ancestor in the fakes: as a real restore's, detached.
        let restored = "0123456789abcdef0123456789abcdef";
        w.metadata
            .state()
            .branches
            .get_mut(&main.id)
            .unwrap()
            .timeline_id = Some(restored.into());
        let tenant: chelabase_neon::NeonId = w
            .metadata
            .database(db)
            .neon_tenant_id
            .unwrap()
            .parse()
            .unwrap();
        w.storage
            .insert_timeline(tenant.as_str(), restored, chelabase_neon::Lsn(1));
        store.set_now(at(86_400 * 31));
        answer(at(86_400 * 31), restored.to_string());
        w.handle_storage_sweep(db).await.unwrap();
        assert_eq!(w.storage_sweep_restore_voids.load(Relaxed), 1);
    }

    /// Audit S2: a failing job is retried a few times, then dropped and counted.
    #[tokio::test]
    async fn sweep_jobs_stop_retrying_after_their_attempts() {
        use crate::storage_sweep::SWEEP_ATTEMPTS;
        use std::sync::atomic::Ordering::Relaxed;
        let (mut w, _journal) = worker();
        w.blob_store = Some(Arc::new(MemoryStore::default()));
        let db = ready_database(&w).await;
        w.metadata
            .state()
            .databases
            .get_mut(&db)
            .unwrap()
            .neon_tenant_id = None;
        assert!(w.handle_storage_sweep_attempt(db, 1).await.is_err());
        assert_eq!(w.storage_sweep_refusals.load(Relaxed), 0);
        w.handle_storage_sweep_attempt(db, SWEEP_ATTEMPTS)
            .await
            .unwrap();
        assert_eq!(w.storage_sweep_refusals.load(Relaxed), 1);
    }

    /// Audit B2: periodic ids name the period, a delete's request is a purge, an event's is
    /// its own.
    #[tokio::test]
    async fn sweep_requests_carry_their_cause() {
        use crate::storage_sweep::SweepCause;
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let dev = ready_branch(&w, db, "dev", main.id).await;
        w.handle_branch_job(BranchJob::Delete { branch_id: dev })
            .await
            .unwrap();
        w.queue_storage_sweeps().await.unwrap();
        w.handle_delete(db).await.unwrap();
        let causes = w.sweep_requests.causes();
        assert!(
            matches!(causes[0], (d, SweepCause::Event(_)) if d == db),
            "{causes:?}"
        );
        assert!(
            matches!(causes[1], (d, SweepCause::Period(_)) if d == db),
            "{causes:?}"
        );
        assert_eq!(causes[2], (db, SweepCause::Purge));
        assert_eq!(SweepCause::Purge.tag(), "purge");
        assert_eq!(SweepCause::Period(7).tag(), "7");
    }

    /// Two sweeps of one database never overlap: the second waits for the lock.
    #[tokio::test]
    async fn sweep_jobs_of_one_database_never_overlap() {
        let (mut w, _journal) = worker();
        let store = Arc::new(MemoryStore::default());
        w.blob_store = Some(store.clone());
        let db = ready_database(&w).await;
        w.computes
            .answer("datallowconn", &databases(&[], SystemTime::now()));
        use crate::ports::WorkerMetadata;
        let held = w
            .metadata
            .lock_endpoint(&format!("storage-sweep:{db}"))
            .await
            .unwrap();
        let w = Arc::new(w);
        let job = tokio::spawn({
            let w = w.clone();
            async move { w.handle_storage_sweep(db).await }
        });
        tokio::task::yield_now().await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(!job.is_finished(), "ran while another sweep held the lock");
        assert!(store.list("_sweep/").await.unwrap().is_empty());
        drop(held);
        job.await.unwrap().unwrap();
        assert!(!store.list("_sweep/").await.unwrap().is_empty());
    }
}

// --- The model: random histories against an oracle of reachable versions ---------------------

/// A tiny deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// One timeline: its states over time (the last is the current one).
#[derive(Clone)]
struct Timeline {
    id: String,
    history: Vec<(u64, HashSet<Uuid>)>,
}

/// Each timeline ever made: the one it branched off and when (Neon's ancestry).
type Ancestry = HashMap<String, Option<(String, u64)>>;

impl Timeline {
    fn at(&self, t: u64) -> HashSet<Uuid> {
        self.history
            .iter()
            .rev()
            .find(|(when, _)| *when <= t)
            .or(self.history.first())
            .map(|(_, s)| s.clone())
            .unwrap_or_default()
    }
}

struct ModelBranch {
    id: BranchId,
    kind: BranchKind,
    tl: Timeline,
}

struct World {
    now: u64,
    rng: Rng,
    branches: Vec<ModelBranch>,
    /// Blobs written whose row isn't there yet: (branch, version, started).
    in_flight: Vec<(BranchId, Uuid, u64)>,
    db: DatabaseId,
    ops: Vec<String>,
    ancestry: Ancestry,
    /// A restore's timeline has no ancestor, as production's: restores detach it.
    detached: bool,
    /// How far the databases' clock is from the store's, in seconds.
    skew: i64,
}

// Far above the sweep's one-hour skew margin, so marks older than a branch point survive.
const SEEDS: u64 = 600;
const M_GRACE: u64 = 20_000;
const M_RETENTION: u64 = 60_000;
/// A ticket's life: an upload's row comes within it of its blob.
const M_TICKET: u64 = 300;

fn new_timeline(now: u64, state: HashSet<Uuid>) -> Timeline {
    Timeline {
        id: Uuid::new_v4().simple().to_string(),
        history: vec![(now, state)],
    }
}

impl World {
    /// A new timeline holding `state`, branched off `parent` at a point.
    fn fresh(&mut self, state: HashSet<Uuid>, parent: Option<(String, u64)>) -> Timeline {
        let tl = new_timeline(self.now, state);
        self.ancestry.insert(tl.id.clone(), parent);
        tl
    }

    fn set(&mut self, i: usize, f: impl FnOnce(&mut HashSet<Uuid>)) {
        let now = self.now;
        let tl = &mut self.branches[i].tl;
        let mut s = tl.history.last().unwrap().1.clone();
        f(&mut s);
        tl.history.push((now, s));
    }

    fn live(&self) -> Vec<usize> {
        (0..self.branches.len())
            .filter(|i| self.branches[*i].kind == BranchKind::Normal)
            .collect()
    }

    /// Right after a new timeline, before a pass can see it, rows on it go: the void is all
    /// that keeps a mark from outliving a reference no pass ever read.
    async fn lose_rows(&mut self, i: usize, store: &MemoryStore, mid_pass: bool) {
        if mid_pass || self.rng.below(3) == 0 {
            return;
        }
        self.now += 1 + self.rng.below(50);
        store.set_now(at(self.now));
        let all: Vec<Uuid> = self.branches[i]
            .tl
            .history
            .last()
            .unwrap()
            .1
            .iter()
            .copied()
            .collect();
        // Every row, or a coin per row.
        let every = self.rng.below(2) == 0;
        let mut drop = Vec::new();
        for v in all {
            if every || self.rng.below(2) == 0 {
                drop.push(v);
            }
        }
        self.set(i, |s| {
            for v in &drop {
                s.remove(v);
            }
        });
    }

    /// A restorable point of `tl`: within the window and its own history.
    fn point(&mut self, tl: &Timeline) -> u64 {
        let from = self.now.saturating_sub(M_RETENTION).max(tl.history[0].0);
        from + self.rng.below(self.now - from + 1)
    }

    /// Every version some restore, branch or row can still need.
    fn reachable(&self) -> HashSet<Uuid> {
        let from = self.now.saturating_sub(M_RETENTION);
        let mut out: HashSet<Uuid> = self.in_flight.iter().map(|(_, v, _)| *v).collect();
        for b in &self.branches {
            out.extend(b.tl.at(from));
            for (when, s) in &b.tl.history {
                if *when >= from {
                    out.extend(s.iter().copied());
                }
            }
        }
        out
    }

    /// One random event. Writes to the store go through `store`.
    async fn step(&mut self, store: &MemoryStore, mid_pass: bool) {
        let live = self.live();
        let pick = live[self.rng.below(live.len() as u64) as usize];
        let id = self.branches[pick].id;
        let op = if mid_pass {
            [0, 1, 2, 3, 4, 5, 8][self.rng.below(7) as usize]
        } else {
            self.rng.below(11)
        };
        self.ops.push(format!("t={} op={op} on {pick}", self.now));
        match op {
            // An upload: the blob, then (later, within a ticket's life) its row.
            0 | 1 => {
                let v = Uuid::new_v4();
                blob(store, self.db, v).await;
                self.in_flight.push((id, v, self.now));
            }
            // A row is deleted (or its upload is abandoned).
            2 => {
                let current = self.branches[pick].tl.history.last().unwrap().1.clone();
                if let Some(v) = current
                    .iter()
                    .nth(self.rng.below(current.len() as u64) as usize)
                    .copied()
                {
                    self.set(pick, |s| {
                        s.remove(&v);
                    });
                }
                if !self.in_flight.is_empty() && self.rng.below(2) == 0 {
                    let i = self.rng.below(self.in_flight.len() as u64) as usize;
                    self.in_flight.remove(i);
                }
            }
            // A branch at a past point of its parent.
            3 => {
                let tl = self.branches[pick].tl.clone();
                let t = self.point(&tl);
                let tl = self.fresh(tl.at(t), Some((tl.id.clone(), t)));
                self.branches.push(ModelBranch {
                    id: BranchId::new(),
                    kind: BranchKind::Normal,
                    tl,
                });
                let i = self.branches.len() - 1;
                self.lose_rows(i, store, mid_pass).await;
            }
            // A point-in-time restore: the old timeline becomes a backup.
            4 => {
                let tl = self.branches[pick].tl.clone();
                let t = self.point(&tl);
                self.branches.push(ModelBranch {
                    id: BranchId::new(),
                    kind: BranchKind::Backup,
                    tl: tl.clone(),
                });
                let parent = if self.detached {
                    None
                } else {
                    Some((tl.id.clone(), t))
                };
                self.branches[pick].tl = self.fresh(tl.at(t), parent);
                self.lose_rows(pick, store, mid_pass).await;
            }
            // A restore from a backup.
            5 => {
                let backups: Vec<usize> = (0..self.branches.len())
                    .filter(|i| self.branches[*i].kind == BranchKind::Backup)
                    .collect();
                if let Some(&b) = backups.get(self.rng.below(backups.len().max(1) as u64) as usize)
                {
                    let state = self.branches[b].tl.history.last().unwrap().1.clone();
                    let parent = Some((self.branches[b].tl.id.clone(), self.now));
                    self.branches[pick].tl = self.fresh(state, parent);
                }
            }
            // A branch or a backup deleted (never the last live branch).
            6 => {
                let i = self.rng.below(self.branches.len() as u64) as usize;
                if self.branches[i].kind == BranchKind::Backup || live.len() > 1 {
                    let gone = self.branches.remove(i).id;
                    self.in_flight.retain(|(b, _, _)| *b != gone);
                }
            }
            // A row moves to another bucket: same version, nothing changes for references.
            7 => {}
            // A branch at head (audit S7: it voids no older mark).
            8 => {
                let tl = self.branches[pick].tl.clone();
                let state = tl.history.last().unwrap().1.clone();
                let tl = self.fresh(state, Some((tl.id, self.now)));
                self.branches.push(ModelBranch {
                    id: BranchId::new(),
                    kind: BranchKind::Normal,
                    tl,
                });
            }
            // Time passes: a little (uploads finish) or a lot.
            _ => {
                self.now += if self.rng.below(2) == 0 {
                    self.rng.below(M_TICKET)
                } else {
                    self.rng.below(2 * M_GRACE)
                };
                store.set_now(at(self.now));
            }
        }
        // Uploads finish (their row, while the ticket lives) or die with it.
        let now = self.now;
        let mut finished = Vec::new();
        self.in_flight.retain(|(b, v, started)| {
            if now - started > M_TICKET {
                return false;
            }
            if now > *started {
                finished.push((*b, *v));
                return false;
            }
            true
        });
        for (b, v) in finished {
            if let Some(i) = self.branches.iter().position(|x| x.id == b) {
                self.set(i, |s| {
                    s.insert(v);
                });
            }
        }
    }
}

fn skewed(t: u64, skew: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs((T0.as_secs() as i64 + t as i64 + skew) as u64)
}

struct Model {
    world: tokio::sync::Mutex<World>,
    store: Arc<MemoryStore>,
    /// The chance in 1000 that a read during a pass lets an event in first.
    interleave: std::sync::atomic::AtomicU64,
}

impl SweepSource for Model {
    async fn database(&self, db: DatabaseId) -> Result<Option<SweepDatabase>, String> {
        let w = self.world.lock().await;
        assert_eq!(db, w.db);
        Ok(Some(SweepDatabase {
            status: DatabaseStatus::Ready,
            branches: w
                .branches
                .iter()
                .map(|b| SweepBranch {
                    id: b.id,
                    kind: b.kind,
                    status: BranchStatus::Ready,
                    timeline: Some(b.tl.id.clone()),
                    pending_timeline: None,
                    endpoint_id: None,
                })
                .collect(),
            retention: Duration::from_secs(M_RETENTION),
        }))
    }

    async fn references(
        &self,
        _db: DatabaseId,
        b: &SweepBranch,
        _expire: bool,
        _open: &[String],
    ) -> Result<BranchRead, ReadError> {
        let mut w = self.world.lock().await;
        if w.rng.below(1000) < self.interleave.load(std::sync::atomic::Ordering::Relaxed) {
            w.step(&self.store, true).await;
        }
        let found = w
            .branches
            .iter()
            .find(|x| x.id == b.id)
            .ok_or("the branch is gone")?;
        if found.tl.id.as_str() != b.timeline.as_deref().unwrap_or_default() {
            return Err("the branch moved to another timeline".into());
        }
        Ok(BranchRead {
            versions: found.tl.history.last().unwrap().1.clone(),
            timelines: [found.tl.id.clone()].into(),
            clock: Some(skewed(w.now, w.skew)),
            ..BranchRead::default()
        })
    }

    async fn branch_point(
        &self,
        _db: DatabaseId,
        timeline: &str,
        recorded: &BTreeSet<String>,
    ) -> Result<Option<SystemTime>, String> {
        let mut w = self.world.lock().await;
        if w.rng.below(1000) < self.interleave.load(std::sync::atomic::Ordering::Relaxed) {
            w.step(&self.store, true).await;
        }
        let mut at_tl = timeline.to_string();
        loop {
            match w.ancestry.get(&at_tl).cloned().flatten() {
                None => return Ok(None),
                Some((parent, t)) if recorded.contains(&parent) => {
                    return Ok(Some(skewed(t, w.skew)));
                }
                Some((parent, _)) => at_tl = parent,
            }
        }
    }
}

/// The invariant (AGENTS.md: a second bug in one area gets a model): over random histories of
/// uploads, deletes, moves, branches at past points, point-in-time restores (detached, as
/// production's, on half the seeds), restores from backups, rows lost before any pass sees a new
/// timeline, branch and backup deletes and passing time, with clocks up to 3000 s apart and
/// events landing mid-pass, no pass ever deletes a blob some branch, backup or restore within the window can still need.
/// Once the history goes quiet, every other blob is deleted (the sweep isn't vacuous).
#[tokio::test]
async fn sweep_model_never_deletes_a_reachable_blob() {
    let mut deleted_total = 0;
    for seed in 1..=SEEDS {
        let store = Arc::new(MemoryStore::default());
        store.set_now(at(0));
        let model = Model {
            world: tokio::sync::Mutex::new(World {
                now: 0,
                rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
                branches: Vec::new(),
                in_flight: Vec::new(),
                db: DatabaseId::new(),
                ops: Vec::new(),
                ancestry: Ancestry::new(),
                detached: seed % 2 == 0,
                skew: (seed as i64 % 7 - 3) * 1000,
            }),
            store: store.clone(),
            interleave: (if seed % 3 == 0 { 150 } else { 0 }).into(),
        };
        {
            let mut w = model.world.lock().await;
            let tl = w.fresh(HashSet::new(), None);
            w.branches.push(ModelBranch {
                id: BranchId::new(),
                kind: BranchKind::Normal,
                tl,
            });
        }
        let db = model.world.lock().await.db;
        let check = |label: String, reachable: HashSet<Uuid>, ops: Vec<String>| {
            let store = store.clone();
            async move {
                for v in reachable {
                    assert!(
                        exists(&store, &key(db, 16384, v)).await,
                        "seed {seed}: {label}: reachable {v} deleted; ops {ops:?}"
                    );
                }
            }
        };
        for round in 0..120 {
            {
                let mut w = model.world.lock().await;
                let n = 1 + w.rng.below(4);
                for _ in 0..n {
                    w.step(&store, false).await;
                }
            }
            let reachable_before = model.world.lock().await.reachable();
            let report = sweep_database(
                &model,
                store.as_ref(),
                db,
                Duration::from_secs(M_GRACE),
                false,
            )
            .await
            .unwrap();
            deleted_total += report.deleted;
            let w = model.world.lock().await;
            check(
                format!("round {round}, before the pass"),
                reachable_before,
                w.ops.clone(),
            )
            .await;
            check(
                format!("round {round}, after the pass"),
                w.reachable(),
                w.ops.clone(),
            )
            .await;
        }
        // Quiet: time passes, passes run; only reachable blobs remain.
        model
            .interleave
            .store(0, std::sync::atomic::Ordering::Relaxed);
        for _ in 0..4 {
            {
                let mut w = model.world.lock().await;
                // Uploads still open are abandoned: their tickets expire.
                w.in_flight.clear();
                w.now += M_GRACE + M_RETENTION;
                store.set_now(at(w.now));
            }
            let r = sweep_database(
                &model,
                store.as_ref(),
                db,
                Duration::from_secs(M_GRACE),
                false,
            )
            .await
            .unwrap();
            assert_eq!(r.blocked, None);
            deleted_total += r.deleted;
        }
        let w = model.world.lock().await;
        let reachable = w.reachable();
        let left: HashSet<Uuid> = store
            .list(&format!("{db}/"))
            .await
            .unwrap()
            .iter()
            .filter_map(|o| o.key.rsplit('/').next()?.parse().ok())
            .collect();
        assert_eq!(
            left, reachable,
            "seed {seed}: quiet history keeps exactly the reachable blobs"
        );
    }
    assert!(
        deleted_total > 100,
        "the model deleted only {deleted_total} blobs"
    );
}

/// Audit round 2 B1, directed: a marked blob; a restore to before its row went brings the row
/// back; the row is deleted again before any pass runs. The blob stayed referenced on the
/// restored timeline until then, so it must outlive that by the wait, not by the old mark's.
/// The restored timeline is detached (no known branch point), as production's are.
#[tokio::test]
async fn sweep_a_restore_starts_the_wait_over_even_if_no_pass_saw_the_row() {
    let store = MemoryStore::default();
    let src = Source::new(vec![(branch(BranchKind::Normal), vec![])]);
    store.set_now(at(0));
    let k = blob(&store, src.db, Uuid::new_v4()).await;
    store.set_now(at(GRACE.as_secs()));
    assert_eq!(pass(&src, &store).await.marked, 1);
    let marked = GRACE.as_secs();
    // Restored at marked + 2000, to a point before the mark: the row is back, then deleted
    // before the next pass, so no pass ever reads it.
    src.st().branches[0].0.timeline = Some(Uuid::new_v4().simple().to_string());
    let gone = marked + 2000;
    let wait = (GRACE + RETENTION).as_secs();
    // The old mark's wait is over, but the restore voided it: the blob is marked again.
    let again = marked + wait;
    store.set_now(at(again));
    let r = pass(&src, &store).await;
    assert_eq!(
        (r.blocked.as_deref(), r.voided_by_restore, r.marked, r.deleted),
        (None, 1, 1, 0),
        "the wait wasn't started over: {r:?}"
    );
    // Inside the window of `gone`: still needed.
    store.set_now(at(gone + RETENTION.as_secs()));
    let r = pass(&src, &store).await;
    assert_eq!((r.blocked.as_deref(), r.waiting, r.deleted), (None, 1, 0), "{r:?}");
    assert!(exists(&store, &k).await);
    // The new mark waits in full, to the second.
    store.set_now(at(again + wait - 1));
    let r = pass(&src, &store).await;
    assert_eq!((r.blocked.as_deref(), r.waiting, r.deleted), (None, 1, 0), "{r:?}");
    assert!(exists(&store, &k).await);
    store.set_now(at(again + wait));
    let r = pass(&src, &store).await;
    assert_eq!((r.blocked.as_deref(), r.deleted), (None, 1), "{r:?}");
    assert!(!exists(&store, &k).await);
}
