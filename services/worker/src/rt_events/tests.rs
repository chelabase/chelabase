//! Edge cases beyond the RED set (RT1 T5 coder): the id cache, and compute-stopping lookups
//! that fail or find no live branch.

use std::sync::Arc;

use chelabase_metadata::{BranchStatus, DatabaseStatus, NewBranch};
use chelabase_neon::ComputeBackend;
use chelabase_types::BranchId;

use crate::fakes::worker;
use crate::ports::WorkerMetadata;
use crate::rt_events::{PublishingMetadata, RtPublisher};
use crate::rt1_tests::{RecordingPublisher, rt_worker, status_event};

#[tokio::test]
async fn a_branchs_ids_are_read_once_and_then_cached() {
    let (w, journal) = worker();
    let fake = w.metadata.clone();
    let rec = Arc::new(RecordingPublisher::new(journal));
    let dynamic: Arc<dyn RtPublisher> = rec.clone();
    let m = PublishingMetadata::new(fake.clone(), dynamic);
    let db = fake.insert_database("app", DatabaseStatus::Ready);
    let dev = BranchId::new();
    fake.create_branch(&NewBranch {
        id: dev,
        database_id: db,
        name: "dev",
        parent_branch_id: BranchId::from(db),
        branched_at_time: None,
        branched_at_lsn: None,
    });
    fake.state().branches.get_mut(&dev).unwrap().status = BranchStatus::Ready;

    assert!(
        m.set_branch_status(dev, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap()
    );
    // The lookups would fail now: the cached ids still carry the next event.
    fake.state().fail = Some("get_database");
    assert!(
        m.set_branch_status(dev, &[BranchStatus::Resetting], BranchStatus::Ready)
            .await
            .unwrap()
    );
    let events: Vec<_> = rec.events().into_iter().map(|(_, ev)| ev).collect();
    assert_eq!(
        events,
        [
            status_event(db, dev, BranchStatus::Resetting),
            status_event(db, dev, BranchStatus::Ready),
        ]
    );
}

/// The wrapper over a fresh fake with a ready database `app` and a branch `dev` in `status`.
fn wrapped(
    status: BranchStatus,
) -> (
    PublishingMetadata<crate::fakes::FakeMetadata>,
    crate::fakes::FakeMetadata,
    Arc<RecordingPublisher>,
    chelabase_types::DatabaseId,
    BranchId,
) {
    let (w, journal) = worker();
    let fake = w.metadata.clone();
    let rec = Arc::new(RecordingPublisher::new(journal));
    let dynamic: Arc<dyn RtPublisher> = rec.clone();
    let m = PublishingMetadata::new(fake.clone(), dynamic);
    let db = fake.insert_database("app", DatabaseStatus::Ready);
    let dev = BranchId::new();
    fake.create_branch(&NewBranch {
        id: dev,
        database_id: db,
        name: "dev",
        parent_branch_id: BranchId::from(db),
        branched_at_time: None,
        branched_at_lsn: None,
    });
    fake.state().branches.get_mut(&dev).unwrap().status = status;
    (m, fake, rec, db, dev)
}

#[tokio::test]
async fn a_failure_recorded_after_a_branch_delete_began_publishes_nothing() {
    // `set_branch_failed` is ignored once the branch is deleting: no false `failed` event.
    let (m, fake, rec, _db, dev) = wrapped(BranchStatus::Deleting);
    m.set_branch_failed(dev, "boom").await.unwrap();
    assert_eq!(fake.branch(dev).status, BranchStatus::Deleting);
    assert!(rec.all().is_empty(), "{:?}", rec.all());
}

#[tokio::test]
async fn a_failure_recorded_after_a_database_delete_began_publishes_nothing() {
    let (m, fake, rec, db, _dev) = wrapped(BranchStatus::Ready);
    fake.state().databases.get_mut(&db).unwrap().status = DatabaseStatus::Deleting;
    fake.state()
        .branches
        .get_mut(&BranchId::from(db))
        .unwrap()
        .status = BranchStatus::Deleting;
    m.set_failed(db, "boom").await.unwrap();
    assert_eq!(fake.database(db).status, DatabaseStatus::Deleting);
    assert!(rec.all().is_empty(), "{:?}", rec.all());
}

/// A client whose server never answers (nothing listens on port 1), with room for `capacity`
/// buffered commands.
async fn unreachable_nats(capacity: usize) -> async_nats::Client {
    async_nats::ConnectOptions::new()
        .retry_on_initial_connect()
        .client_capacity(capacity)
        .connect("127.0.0.1:1")
        .await
        .unwrap()
}

#[tokio::test]
async fn the_compute_stopping_signal_never_holds_a_stop_past_its_bound() {
    let publisher = crate::rt_events::NatsPublisher::new(unreachable_nats(8).await);
    let started = std::time::Instant::now();
    let sent = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        publisher.compute_stopping(uuid::Uuid::new_v4()),
    )
    .await
    .expect("bounded");
    assert!(sent.is_err(), "no server, no flush: {sent:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn a_project_event_never_holds_a_write_while_nats_is_unreachable() {
    // Once the client's buffer is full, each publish gives up after its bound instead of
    // waiting for the server to come back.
    let publisher = crate::rt_events::NatsPublisher::new(unreachable_nats(2).await);
    let ev = chelabase_nats::rt::ProjectEvent::DatabaseDeleted {
        database_id: uuid::Uuid::new_v4(),
    };
    let refused = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for _ in 0..64 {
            if publisher.project(uuid::Uuid::new_v4(), ev.clone()).await.is_err() {
                return true;
            }
        }
        false
    })
    .await
    .expect("bounded");
    assert!(refused, "a full buffer refuses the event");
}

#[tokio::test]
async fn a_failed_branch_lookup_still_stops_the_compute() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let ep = rt.ep(BranchId::from(db));
    rt.clear();
    rt.fake.state().fail = Some("compute_branch");

    rt.w.computes.delete(&ep).await.unwrap();

    assert!(rt.publisher.all().is_empty(), "{:?}", rt.publisher.all());
    assert!(!rt.w.computes.list().await.unwrap().contains(&ep));
}

#[tokio::test]
async fn a_deleted_branchs_leftover_compute_publishes_nothing() {
    // A teardown retried after the tombstone: no one watches a deleted branch.
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let main = BranchId::from(db);
    let ep = rt.ep(main);
    rt.fake.state().branches.get_mut(&main).unwrap().status = BranchStatus::Deleted;
    rt.clear();

    rt.w.computes.delete(&ep).await.unwrap();

    assert!(
        rt.publisher.stopping().is_empty(),
        "{:?}",
        rt.publisher.all()
    );
}
