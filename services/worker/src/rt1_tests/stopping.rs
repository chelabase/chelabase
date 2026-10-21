//! P9/S19: every compute the worker stops (`ComputeBackend::delete`) is announced first on
//! `chelabase.rt.compute.stopping.<branch_id>`, the branch resolved from the endpoint; static
//! computes (no branch) and rehearsal branches' computes publish nothing. One wrapper; one test
//! per caller family (reset, restore, branch delete, database delete, reconcile upgrade). The
//! merge's after-merge reset goes through the reset's `stop_compute`; `data_api.rs` stops no
//! compute (it recreates through `recreate_compute`, which only creates).

use chelabase_metadata::{BranchKind, BranchStatus, DatabaseStatus};
use chelabase_nats::PointInTime;
use chelabase_neon::ComputeBackend;
use chelabase_types::BranchId;

use super::{Rt, at, rt_worker};

/// `rt.compute_stopping <branch>` was journaled exactly once, before `compute.delete <ep>`.
fn announced_before_delete(rt: &Rt, branch: BranchId, ep: &str) {
    let stopping = rt.publisher.stopping();
    assert_eq!(
        stopping.iter().filter(|b| **b == branch.as_uuid()).count(),
        1,
        "one compute-stopping for {branch}: {stopping:?}"
    );
    let announced = at(
        &rt.journal,
        &format!("rt.compute_stopping {}", branch.as_uuid()),
    )
    .expect("announced");
    let deleted =
        at(&rt.journal, &format!("compute.delete {ep}")).expect("the compute was deleted");
    assert!(
        announced < deleted,
        "announced before the compute went: {:?}",
        rt.journal.lock().unwrap()
    );
}

// --- The wrapper ------------------------------------------------------------------------------

#[tokio::test]
async fn delete_announces_the_branch_before_the_compute_goes() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let main = BranchId::from(db);
    let ep = rt.ep(main);
    rt.clear();

    rt.w.computes.delete(&ep).await.unwrap();

    announced_before_delete(&rt, main, &ep);
    assert!(!rt.w.computes.list().await.unwrap().contains(&ep));
}

#[tokio::test]
async fn a_failed_publish_still_stops_the_compute() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let ep = rt.ep(BranchId::from(db));
    rt.clear();
    *rt.publisher.fail.lock().unwrap() = true;

    rt.w.computes.delete(&ep).await.unwrap();

    assert_eq!(rt.publisher.stopping(), [db.as_uuid()], "it was tried");
    assert!(!rt.w.computes.list().await.unwrap().contains(&ep));
}

#[tokio::test]
async fn a_failed_delete_was_still_announced() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let ep = rt.ep(BranchId::from(db));
    rt.clear();
    *rt.w.computes.inner().fail_delete.lock().unwrap() = Some("docker down".into());

    assert!(rt.w.computes.delete(&ep).await.is_err());
    assert_eq!(rt.publisher.stopping(), [db.as_uuid()]);
}

#[tokio::test]
async fn a_compute_without_a_branch_publishes_nothing() {
    // A static compute (a schema base read) or an orphan container: no branch row links it.
    let rt = rt_worker();
    rt.w.computes.inner().insert("ep-static");

    rt.w.computes.delete("ep-static").await.unwrap();

    assert!(rt.publisher.all().is_empty(), "{:?}", rt.publisher.all());
    assert!(
        !rt.w
            .computes
            .list()
            .await
            .unwrap()
            .contains(&"ep-static".to_string())
    );
}

#[tokio::test]
async fn a_rehearsal_compute_publishes_nothing() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let rehearsal = rt
        .ready_branch(db, "merge-rehearsal", BranchId::from(db))
        .await;
    rt.fake.state().branches.get_mut(&rehearsal).unwrap().kind = BranchKind::Rehearsal;
    let ep = rt.ep(rehearsal);
    rt.clear();

    rt.w.computes.delete(&ep).await.unwrap();

    assert!(
        rt.publisher.stopping().is_empty(),
        "{:?}",
        rt.publisher.all()
    );
}

#[tokio::test]
async fn creating_and_reading_publish_nothing() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let ep = rt.ep(BranchId::from(db));
    rt.w.computes.status(&ep).await.unwrap();
    rt.w.computes
        .admin_sql(&ep, "app", "SELECT 1")
        .await
        .unwrap();
    assert!(
        rt.publisher.stopping().is_empty(),
        "{:?}",
        rt.publisher.all()
    );
}

// --- Caller families --------------------------------------------------------------------------

#[tokio::test]
async fn a_reset_announces_before_it_stops_the_compute() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let dev = rt.ready_branch(db, "dev", BranchId::from(db)).await;
    let ep = rt.ep(dev);
    rt.fake.state().branches.get_mut(&dev).unwrap().status = BranchStatus::Resetting;
    rt.clear();

    rt.w.handle_branch_reset_to_parent(dev).await.unwrap();

    assert_eq!(
        rt.fake.branch(dev).status,
        BranchStatus::Ready,
        "{:?}",
        rt.fake.branch(dev).error
    );
    announced_before_delete(&rt, dev, &ep);
}

#[tokio::test]
async fn a_restore_announces_before_it_stops_the_compute() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let dev = rt.ready_branch(db, "dev", BranchId::from(db)).await;
    let ep = rt.ep(dev);
    rt.fake.state().branches.get_mut(&dev).unwrap().status = BranchStatus::Resetting;
    rt.clear();

    rt.w.handle_branch_restore(
        dev,
        BranchId::new(),
        "dev-before-restore",
        PointInTime::Lsn("0/2000000".into()),
    )
    .await
    .unwrap();

    assert_eq!(
        rt.fake.branch(dev).status,
        BranchStatus::Ready,
        "{:?}",
        rt.fake.branch(dev).error
    );
    announced_before_delete(&rt, dev, &ep);
}

#[tokio::test]
async fn a_branch_delete_announces_before_it_stops_the_compute() {
    // The branch is `deleting` by the time its compute goes: the lookup must still find it
    // (`WorkerMetadata::endpoint_branch` skips deleting branches).
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let dev = rt.ready_branch(db, "dev", BranchId::from(db)).await;
    let ep = rt.ep(dev);
    rt.clear();

    rt.w.handle_branch_delete(dev).await.unwrap();

    assert_eq!(rt.fake.branch(dev).status, BranchStatus::Deleted);
    announced_before_delete(&rt, dev, &ep);
}

#[tokio::test]
async fn a_database_delete_announces_every_branch_before_its_compute_goes() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let main = BranchId::from(db);
    let dev = rt.ready_branch(db, "dev", main).await;
    let (main_ep, dev_ep) = (rt.ep(main), rt.ep(dev));
    {
        // What the API's `begin_delete` leaves.
        let mut state = rt.fake.state();
        state.databases.get_mut(&db).unwrap().status = DatabaseStatus::Deleting;
        for b in [main, dev] {
            state.branches.get_mut(&b).unwrap().status = BranchStatus::Deleting;
        }
    }
    rt.clear();

    rt.w.handle_delete(db).await.unwrap();

    announced_before_delete(&rt, main, &main_ep);
    announced_before_delete(&rt, dev, &dev_ep);
}

#[tokio::test]
async fn a_reconcile_upgrade_announces_before_it_stops_the_compute() {
    let rt = rt_worker();
    let db = rt.ready_database().await;
    let main = BranchId::from(db);
    let ep = rt.ep(main);
    rt.w.computes
        .inner()
        .answer("to_regclass('chelabase.schema_version')", "t\n");
    rt.fake
        .state()
        .endpoints
        .get_mut(&ep)
        .unwrap()
        .compute_settings_version = 0;
    rt.clear();

    rt.w.reconcile_once().await.unwrap();

    announced_before_delete(&rt, main, &ep);
}
