//! Carried gaps T4 (R7, R8, P11): provisioning waits for the new tenant's attach event between
//! `create_tenant` (and the history-window PATCH) and `create_timeline`. A wait that times out or
//! can't run is a warning, and the timeline create is tried once anyway (R8: the event gates,
//! the timeline create stays the authority). Branch timelines never wait (a re-attach to the
//! same node sends no event).

use std::collections::BTreeMap;
use std::time::Duration;

use chelabase_metadata::{BranchStatus, DatabaseStatus};
use chelabase_nats::PointInTime;
use chelabase_neon::{NeonId, StorconClient};
use chelabase_types::{BranchId, DatabaseId};

use crate::fakes::{
    FakeWorker, Journal, STORAGE_CAUSE, entries, position, ready_branch, ready_database, worker,
};
use crate::ports::{AttachSource, AttachWait, RealStorage, Storage};
use crate::safekeepers::SafekeeperSet;

fn pos(journal: &Journal, prefix: &str) -> usize {
    position(journal, prefix)
        .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
}

fn has(journal: &Journal, prefix: &str) -> bool {
    position(journal, prefix).is_some()
}

fn count(journal: &Journal, prefix: &str) -> usize {
    entries(journal)
        .iter()
        .filter(|e| e.starts_with(prefix))
        .count()
}

async fn provision(w: &FakeWorker) -> DatabaseId {
    let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
    w.handle_provision(id, "app").await.unwrap();
    id
}

fn tenant_of(w: &FakeWorker, id: DatabaseId) -> String {
    w.metadata
        .database(id)
        .neon_tenant_id
        .expect("the provision recorded its tenant")
}

/// Everything logged on this thread while the guard lives, as plain text (no colours).
#[derive(Clone, Default)]
struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

impl Logs {
    /// `#[tokio::test]` runs on one thread, so a thread-local default sees the whole test.
    fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    /// The lines at WARN level containing every one of `needles`.
    fn warnings_with(&self, needles: &[&str]) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains("WARN") && needles.iter().all(|n| l.contains(n)))
            .count()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

// P11: the wait sits right before the timeline create, after the tenant create and the history
// window's PATCH, for the new tenant, bounded by the worker's `attach_timeout`.
#[tokio::test]
async fn provision_waits_for_the_attach_between_the_tenant_and_the_timeline() {
    let (mut w, journal) = worker();
    w.attach_timeout = Duration::from_millis(1234);
    let id = provision(&w).await;
    assert_eq!(w.metadata.database(id).status, DatabaseStatus::Ready);
    let tenant = tenant_of(&w, id);
    let order = [
        pos(&journal, "storage.create_tenant"),
        pos(&journal, "storage.patch_tenant_config"),
        pos(&journal, "storage.wait_attached"),
        pos(&journal, "storage.create_timeline"),
        pos(&journal, "compute.create"),
    ];
    assert!(order.is_sorted(), "{:?}", entries(&journal));
    assert_eq!(
        entries(&journal)[pos(&journal, "storage.wait_attached")],
        format!("storage.wait_attached {tenant} 1234ms")
    );
    assert_eq!(count(&journal, "storage.wait_attached"), 1);
    assert_eq!(count(&journal, "storage.create_timeline"), 1);
}

// No tenant, nothing to wait for.
#[tokio::test]
async fn no_wait_when_the_tenant_create_fails() {
    // The control: a provision whose tenant create succeeds waits once.
    let (w, journal) = worker();
    provision(&w).await;
    assert_eq!(
        count(&journal, "storage.wait_attached"),
        1,
        "{:?}",
        entries(&journal)
    );

    let (w, journal) = worker();
    *w.storage.fail.lock().unwrap() = Some("create_tenant");
    let id = provision(&w).await;
    let db = w.metadata.database(id);
    assert_eq!(db.status, DatabaseStatus::Failed);
    assert!(
        db.error
            .unwrap_or_default()
            .starts_with("creating tenant: ")
    );
    assert!(
        !has(&journal, "storage.wait_attached"),
        "{:?}",
        entries(&journal)
    );
    assert!(!has(&journal, "storage.create_timeline"));
}

// R8: a wait that timed out, or couldn't run (NATS down, no stream, R10), logs a warning naming
// the tenant (and why it couldn't wait) and goes on: the timeline is created once, the provision
// completes.
#[tokio::test]
async fn a_timed_out_or_unavailable_wait_warns_and_creates_the_timeline_once() {
    for outcome in [
        AttachWait::TimedOut,
        AttachWait::Unavailable("no stream CHELABASE_NEON_EVENTS".into()),
    ] {
        let (logs, _guard) = Logs::capture();
        let (w, journal) = worker();
        *w.storage.attach_result.lock().unwrap() = outcome.clone();
        let id = provision(&w).await;
        let db = w.metadata.database(id);
        assert_eq!(
            db.status,
            DatabaseStatus::Ready,
            "{outcome:?}: {:?}",
            db.error
        );
        assert_eq!(count(&journal, "storage.wait_attached"), 1, "{outcome:?}");
        assert_eq!(count(&journal, "storage.create_timeline"), 1, "{outcome:?}");
        assert!(
            pos(&journal, "storage.wait_attached") < pos(&journal, "storage.create_timeline"),
            "{outcome:?}: {:?}",
            entries(&journal)
        );
        let tenant = tenant_of(&w, id);
        let mut needles = vec![tenant.as_str()];
        if let AttachWait::Unavailable(why) = &outcome {
            needles.push(why.as_str());
        }
        assert!(
            logs.warnings_with(&needles) >= 1,
            "{outcome:?}: no warning naming {needles:?} in:\n{}",
            logs.text()
        );
    }
}

// R8: not a 404 retry. After a wait that timed out, a timeline create that fails is tried once,
// and the provision fails (and tears down) as before.
#[tokio::test]
async fn after_a_timed_out_wait_a_failing_timeline_create_is_tried_once() {
    let (w, journal) = worker();
    *w.storage.attach_result.lock().unwrap() = AttachWait::TimedOut;
    *w.storage.fail.lock().unwrap() = Some("create_timeline");
    let id = provision(&w).await;
    let db = w.metadata.database(id);
    assert_eq!(db.status, DatabaseStatus::Failed);
    let error = db.error.unwrap_or_default();
    assert!(error.starts_with("creating timeline: "), "{error:?}");
    assert!(error.ends_with(STORAGE_CAUSE), "{error:?}");
    assert_eq!(
        count(&journal, "storage.wait_attached"),
        1,
        "{:?}",
        entries(&journal)
    );
    assert_eq!(count(&journal, "storage.create_timeline"), 1);
    assert!(has(&journal, "storage.delete_tenant"));
    assert!(w.storage.tenants.lock().unwrap().is_empty());
    assert!(w.metadata.endpoint_ids().is_empty());
}

// A delete that finished while the provision waited: the provision still notices the endpoint
// row is gone after the timeline create and removes the tenant (as with a delete before the
// tenant create, ruling 8).
#[tokio::test]
async fn a_delete_during_the_wait_still_leaves_no_tenant() {
    let (w, journal) = worker();
    let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
    let state = w.metadata.state.clone();
    *w.storage.on_wait_attached.lock().unwrap() = Some(Box::new(move || {
        // What handle_delete leaves: no endpoint row, the rows tombstones.
        let mut state = state.lock().unwrap();
        let main = state.branches.get_mut(&BranchId::from(id)).unwrap();
        let ep = main.endpoint_id.take().unwrap();
        main.status = BranchStatus::Deleted;
        state.endpoints.remove(&ep);
        state.databases.get_mut(&id).unwrap().status = DatabaseStatus::Deleted;
    }));

    w.handle_provision(id, "app").await.unwrap();

    assert!(
        w.storage.tenants.lock().unwrap().is_empty(),
        "orphan tenant"
    );
    assert!(w.metadata.endpoint_ids().is_empty());
    assert_eq!(w.metadata.database(id).status, DatabaseStatus::Deleted);
    let order = [
        pos(&journal, "storage.wait_attached"),
        pos(&journal, "storage.create_timeline"),
        pos(&journal, "storage.delete_tenant"),
    ];
    assert!(order.is_sorted(), "{:?}", entries(&journal));
    assert!(has(&journal, "metadata.mark_deleted"));
    assert!(!has(&journal, "metadata.set_failed"));
    assert!(!has(&journal, "compute.create"));
    assert!(!has(&journal, "metadata.set_branch_ready"));
}

fn set_status(w: &FakeWorker, id: BranchId, status: BranchStatus) {
    w.metadata.state().branches.get_mut(&id).unwrap().status = status;
}

// No event comes for a timeline on an existing tenant: a branch create, a reset to the parent
// and a restore never wait (each would stall for the whole timeout otherwise).
#[tokio::test]
async fn branch_timelines_never_wait() {
    let (w, journal) = worker();
    // A wait would not be harmless here: it would be noticed.
    *w.storage.attach_result.lock().unwrap() = AttachWait::TimedOut;
    let db = ready_database(&w).await;
    assert_eq!(
        count(&journal, "storage.wait_attached"),
        1,
        "the provision's own wait"
    );
    journal.lock().unwrap().clear();

    let main = w.metadata.main(db).id;
    let dev = ready_branch(&w, db, "dev", main).await;
    let feat = ready_branch(&w, db, "feat", main).await;

    set_status(&w, dev, BranchStatus::Resetting);
    w.handle_branch_reset_to_parent(dev).await.unwrap();
    assert_eq!(
        w.metadata.branch(dev).status,
        BranchStatus::Ready,
        "{:?}",
        w.metadata.branch(dev).error
    );

    set_status(&w, feat, BranchStatus::Resetting);
    w.handle_branch_restore(
        feat,
        BranchId::new(),
        "feat-backup",
        PointInTime::Lsn("0/2000000".into()),
    )
    .await
    .unwrap();
    assert_eq!(
        w.metadata.branch(feat).status,
        BranchStatus::Ready,
        "{:?}",
        w.metadata.branch(feat).error
    );

    assert!(
        count(&journal, "storage.create_branch_timeline") >= 3,
        "{:?}",
        entries(&journal)
    );
    assert!(
        !has(&journal, "storage.wait_attached"),
        "{:?}",
        entries(&journal)
    );
}

fn real_storage(attach: AttachSource) -> RealStorage {
    RealStorage {
        storcon: StorconClient::new("http://127.0.0.1:1", 0),
        safekeepers: SafekeeperSet::new(BTreeMap::new()),
        attach,
    }
}

// A8: a worker built without NATS for the events says so at once; it never waits.
#[tokio::test]
async fn a_disabled_attach_source_is_unavailable_at_once() {
    let storage = real_storage(AttachSource::Disabled);
    let started = std::time::Instant::now();
    let result = storage
        .wait_attached(&NeonId::new(), Duration::from_secs(60))
        .await;
    assert!(matches!(result, AttachWait::Unavailable(_)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

// NATS down for the whole wait: unavailable (with the reason) at about the timeout, not a hang.
#[tokio::test]
async fn an_unreachable_nats_is_unavailable_at_the_deadline() {
    let client = async_nats::ConnectOptions::new()
        .retry_on_initial_connect()
        .connect("nats://127.0.0.1:1")
        .await
        .unwrap();
    let storage = real_storage(AttachSource::Nats(async_nats::jetstream::new(client)));
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        storage.wait_attached(&NeonId::new(), Duration::from_secs(1)),
    )
    .await
    .expect("the wait ended within 10 s");
    match &result {
        AttachWait::Unavailable(why) => assert!(!why.is_empty()),
        other => panic!("{other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}
