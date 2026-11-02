//! The storage sweep (object-storage m8): its rules on a hand-made source, the worker's
//! reads through the fakes, and a model of random branch histories.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use chelabase_metadata::{BranchKind, BranchStatus, DatabaseStatus};
use chelabase_storage::{MemoryStore, ObjectStore, PutOptions};
use chelabase_types::{BranchId, DatabaseId};
use uuid::Uuid;

use crate::storage_sweep::{
    DATABASES_SQL, SweepBranch, SweepDatabase, SweepReport, SweepSettings, SweepSource,
    parse_databases, parse_references, references_sql, sweep_database,
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
    ) -> Result<(HashSet<Uuid>, u64), String> {
        let mut st = self.st();
        if let Some(during) = st.during.take() {
            during(&mut st);
        }
        if expire {
            st.expired.push(b.id);
        }
        match st.branches.iter().find(|(x, _, _)| x.id == b.id) {
            Some((_, _, true)) => Err("compute unreachable".into()),
            Some((_, refs, _)) => Ok((refs.clone(), 0)),
            None => Err("no such branch".into()),
        }
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
    let r = sweep_database(&src, &store, src.db, GRACE, false).await.unwrap();
    assert_eq!(r.deleted, 0, "{r:?}");
    assert!(r.blocked.is_some(), "{r:?}");
    assert!(exists(&store, &orphan).await);
    // The same leap with the databases agreeing (and real time passing) is a normal pass.
    store.set_now(at(GRACE.as_secs()) + Duration::from_secs(5000));
    assert_eq!(pass(&src, &store).await.deleted, 1);
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
    let (refs, tickets) = parse_references(&format!("SET\nt|3\nv|{v}\nend\n")).unwrap();
    assert_eq!((refs, tickets), ([v].into(), 3));
    assert!(parse_references(&format!("v|{v}\n")).is_err(), "cut off");
    assert!(parse_references("v|nope\nend").is_err());
    assert_eq!(
        parse_databases(&format!(
            "d|{}\nd|{}\nend",
            hex::encode("app"),
            hex::encode("a|b\nc")
        ))
        .unwrap(),
        ["app", "a|b\nc"]
    );
    assert!(parse_databases("").is_err());
    assert!(references_sql(true).contains("DELETE FROM storage.upload_tickets"));
    assert!(!references_sql(false).contains("DELETE"));
    for sql in [references_sql(true), DATABASES_SQL.to_string()] {
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

    fn refs(vs: &[Uuid]) -> String {
        let mut out: String = vs.iter().map(|v| format!("v|{v}\n")).collect();
        out.push_str("end\n");
        out
    }

    fn databases(names: &[&str]) -> String {
        let mut out: String = names
            .iter()
            .map(|n| format!("d|{}\n", hex::encode(n)))
            .collect();
        out.push_str("end\n");
        out
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
        restored_by_hand(&w, main.id, "before");
        let ep = w.metadata.main(db).endpoint_id.clone().unwrap();
        let (in_other, in_backup, orphan) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        w.computes
            .answer("datallowconn", &databases(&["app", "other"]));
        w.computes
            .answer_at(&ep, "chelabase_storage_here", &refs(&[]));
        w.computes
            .answer_in("other", "chelabase_storage_here", &refs(&[in_other]));
        // Everything else (the backup's static compute) answers with the backup's references.
        w.computes
            .answer("chelabase_storage_here", &refs(&[in_backup]));
        store.set_now(at(0));
        let keys = [
            blob(&store, db, in_other).await,
            blob(&store, db, in_backup).await,
            blob(&store, db, orphan).await,
        ];
        store.set_now(at(86_400 * 40));
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
        // A mark later (past the history window, 7 days by default), only the orphan goes.
        store.set_now(at(86_400 * 60));
        let r = sweep_database(&w, store.as_ref(), db, grace, false)
            .await
            .unwrap();
        assert_eq!(r.deleted, 1, "{r:?}");
        assert!(exists(&store, &keys[0]).await && exists(&store, &keys[1]).await);
        assert!(!exists(&store, &keys[2]).await);
    }

    /// A branch whose compute doesn't answer stops the pass.
    #[tokio::test]
    async fn sweep_stops_on_a_compute_that_fails() {
        let (mut w, _journal) = worker();
        let store = Arc::new(MemoryStore::default());
        w.blob_store = Some(store.clone());
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let dev = ready_branch(&w, db, "dev", main.id).await;
        w.computes.answer("datallowconn", &databases(&["app"]));
        w.computes.answer("chelabase_storage_here", &refs(&[]));
        let dev_ep = w.metadata.branch(dev).endpoint_id.unwrap();
        w.computes.answer_at(&dev_ep, "chelabase_storage_here", "");
        let r = sweep_database(&w, store.as_ref(), db, GRACE, false)
            .await
            .unwrap();
        let why = r.blocked.expect("blocked");
        assert!(
            why.contains(&dev.to_string()) && why.contains("didn't finish"),
            "{why}"
        );
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
        w.computes.answer("datallowconn", &databases(&["app"]));
        w.computes.answer("chelabase_storage_here", &refs(&[]));
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
        assert_eq!(w.sweep_requests.sent(), [db, db], "the periodic job forgot the deleted database");
        // Once purged, it isn't queued any more.
        w.handle_storage_sweep(db).await.unwrap();
        assert!(store.list(&format!("{db}/")).await.unwrap().is_empty());
        w.queue_storage_sweeps().await.unwrap();
        assert_eq!(w.sweep_requests.sent(), [db, db]);
    }

    /// Two sweeps of one database never overlap: the second waits for the lock.
    #[tokio::test]
    async fn sweep_jobs_of_one_database_never_overlap() {
        let (mut w, _journal) = worker();
        let store = Arc::new(MemoryStore::default());
        w.blob_store = Some(store.clone());
        let db = ready_database(&w).await;
        w.computes.answer("datallowconn", &databases(&[]));
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
}

const M_GRACE: u64 = 1000;
const M_RETENTION: u64 = 3000;
/// A ticket's life: an upload's row comes within it of its blob.
const M_TICKET: u64 = 300;

fn new_timeline(now: u64, state: HashSet<Uuid>) -> Timeline {
    Timeline {
        id: Uuid::new_v4().simple().to_string(),
        history: vec![(now, state)],
    }
}

impl World {
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
        let op = self.rng.below(if mid_pass { 6 } else { 10 });
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
                let state = tl.at(t);
                self.branches.push(ModelBranch {
                    id: BranchId::new(),
                    kind: BranchKind::Normal,
                    tl: new_timeline(self.now, state),
                });
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
                self.branches[pick].tl = new_timeline(self.now, tl.at(t));
            }
            // A restore from a backup.
            5 => {
                let backups: Vec<usize> = (0..self.branches.len())
                    .filter(|i| self.branches[*i].kind == BranchKind::Backup)
                    .collect();
                if let Some(&b) = backups.get(self.rng.below(backups.len().max(1) as u64) as usize)
                {
                    let state = self.branches[b].tl.history.last().unwrap().1.clone();
                    self.branches[pick].tl = new_timeline(self.now, state);
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
            // Time passes.
            _ => {
                self.now += self.rng.below(2 * M_GRACE);
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
    ) -> Result<(HashSet<Uuid>, u64), String> {
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
        Ok((found.tl.history.last().unwrap().1.clone(), 0))
    }
}

/// The invariant (AGENTS.md: a second bug in one area gets a model): over random histories of
/// uploads, deletes, moves, branches at past points, point-in-time restores, restores from
/// backups, branch and backup deletes and passing time, with events landing mid-pass, no
/// pass ever deletes a blob some branch, backup or restore within the window can still need.
/// Once the history goes quiet, every other blob is deleted (the sweep isn't vacuous).
#[tokio::test]
async fn sweep_model_never_deletes_a_reachable_blob() {
    let mut deleted_total = 0;
    for seed in 1..=150u64 {
        let store = Arc::new(MemoryStore::default());
        store.set_now(at(0));
        let model = Model {
            world: tokio::sync::Mutex::new(World {
                now: 0,
                rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
                branches: vec![ModelBranch {
                    id: BranchId::new(),
                    kind: BranchKind::Normal,
                    tl: new_timeline(0, HashSet::new()),
                }],
                in_flight: Vec::new(),
                db: DatabaseId::new(),
                ops: Vec::new(),
            }),
            store: store.clone(),
            interleave: (if seed % 3 == 0 { 150 } else { 0 }).into(),
        };
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
