//! P10/S20: every branch and database status write publishes exactly one `ProjectEvent` on the
//! database's project, after the write returned success; never for a failed write or a guarded
//! one that changed nothing; a failed publish never fails the write; deletes read their ids
//! before the write. The table of what publishes what is in `crate::rt_events`'s docs.

use std::sync::Arc;

use chelabase_metadata::{
    BranchDeleteOutcome, BranchStatus, ClaimOutcome, DatabaseStatus, NewBranch,
};
use chelabase_nats::rt::ProjectEvent;
use chelabase_types::{BranchId, DatabaseId};
use uuid::Uuid;

use super::{RecordingPublisher, at, status_event};
use crate::fakes::{FakeMetadata, Journal, worker};
use crate::ports::WorkerMetadata;
use crate::rt_events::{PublishingMetadata, RtPublisher};

/// The wrapper over a fresh fake: a ready database `app` (main ready) with a ready branch
/// `dev`; nothing journaled or published yet.
struct P {
    m: PublishingMetadata<FakeMetadata>,
    fake: FakeMetadata,
    rec: Arc<RecordingPublisher>,
    journal: Journal,
    db: DatabaseId,
    project: Uuid,
    dev: BranchId,
}

fn p() -> P {
    let (w, journal) = worker();
    let fake = w.metadata.clone();
    let rec = Arc::new(RecordingPublisher::new(journal.clone()));
    let dynamic: Arc<dyn RtPublisher> = rec.clone();
    let m = PublishingMetadata::new(fake.clone(), dynamic);
    let db = fake.insert_database("app", DatabaseStatus::Ready);
    let project = fake.database(db).project_id.as_uuid();
    let dev = BranchId::new();
    let created = fake.create_branch(&NewBranch {
        id: dev,
        database_id: db,
        name: "dev",
        parent_branch_id: BranchId::from(db),
        branched_at_time: None,
        branched_at_lsn: None,
    });
    assert_eq!(created, chelabase_metadata::CreateBranchOutcome::Created);
    set(&fake, dev, BranchStatus::Ready);
    journal.lock().unwrap().clear();
    P {
        m,
        fake,
        rec,
        journal,
        db,
        project,
        dev,
    }
}

fn set(fake: &FakeMetadata, id: BranchId, status: BranchStatus) {
    fake.state().branches.get_mut(&id).unwrap().status = status;
}

impl P {
    /// Exactly one project event, published after the write `metadata.<method>`.
    fn one_after(&self, method: &str) -> ProjectEvent {
        let events = self.rec.events();
        assert_eq!(events.len(), 1, "exactly one event: {events:?}");
        let (project, ev) = events.into_iter().next().unwrap();
        assert_eq!(project, self.project, "on the database's project");
        let write = at(&self.journal, &format!("metadata.{method}")).expect("the write ran");
        let publish = at(&self.journal, "rt.project").expect("published");
        assert!(
            write < publish,
            "published after the write: {:?}",
            self.journal.lock().unwrap()
        );
        ev
    }

    fn none(&self) {
        assert!(
            self.rec.all().is_empty(),
            "nothing published: {:?}",
            self.rec.all()
        );
    }

    fn fail(&self, method: &'static str) {
        self.fake.state().fail = Some(method);
    }
}

// --- Branch status writes -------------------------------------------------------------------

#[tokio::test]
async fn set_branch_status_publishes_the_new_status_once_after_the_write() {
    let p = p();
    assert!(
        p.m.set_branch_status(p.dev, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap()
    );
    assert_eq!(
        p.one_after("set_branch_status"),
        status_event(p.db, p.dev, BranchStatus::Resetting)
    );
}

#[tokio::test]
async fn a_guarded_write_that_changed_nothing_publishes_nothing() {
    let p = p();
    assert!(
        !p.m.set_branch_status(p.dev, &[BranchStatus::Pending], BranchStatus::Ready)
            .await
            .unwrap()
    );
    p.none();
}

#[tokio::test]
async fn a_failed_write_publishes_nothing() {
    let p = p();
    p.fail("set_branch_status");
    assert!(
        p.m.set_branch_status(p.dev, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .is_err()
    );
    p.none();
}

#[tokio::test]
async fn a_failed_publish_never_fails_the_write() {
    let p = p();
    *p.rec.fail.lock().unwrap() = true;
    let changed =
        p.m.set_branch_status(p.dev, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await;
    assert_eq!(p.rec.all().len(), 1, "it was tried once");
    assert!(matches!(changed, Ok(true)), "{changed:?}");
    assert_eq!(p.fake.branch(p.dev).status, BranchStatus::Resetting);
}

#[tokio::test]
async fn set_branch_ready_publishes_ready() {
    let p = p();
    set(&p.fake, p.dev, BranchStatus::Provisioning);
    assert!(
        p.m.set_branch_ready(p.dev, "sealed", None, None)
            .await
            .unwrap()
    );
    assert_eq!(
        p.one_after("set_branch_ready"),
        status_event(p.db, p.dev, BranchStatus::Ready)
    );
    // Not provisioning any more: nothing changes, nothing is published.
    p.rec.published.lock().unwrap().clear();
    assert!(
        !p.m.set_branch_ready(p.dev, "sealed", None, None)
            .await
            .unwrap()
    );
    p.none();
}

#[tokio::test]
async fn set_branch_failed_publishes_failed() {
    let p = p();
    p.m.set_branch_failed(p.dev, "boom").await.unwrap();
    assert_eq!(
        p.one_after("set_branch_failed"),
        status_event(p.db, p.dev, BranchStatus::Failed)
    );
}

#[tokio::test]
async fn set_branch_error_publishes_the_status_the_branch_holds() {
    let p = p();
    p.m.set_branch_error(p.dev, Some("refused")).await.unwrap();
    assert_eq!(
        p.one_after("set_branch_error"),
        status_event(p.db, p.dev, BranchStatus::Ready)
    );
}

#[tokio::test]
async fn begin_branch_delete_publishes_deleting_only_when_it_started() {
    let p = p();
    assert_eq!(
        p.m.begin_branch_delete(p.dev, None).await.unwrap(),
        BranchDeleteOutcome::Started
    );
    assert_eq!(
        p.one_after("begin_branch_delete"),
        status_event(p.db, p.dev, BranchStatus::Deleting)
    );
    p.rec.published.lock().unwrap().clear();
    assert_eq!(
        p.m.begin_branch_delete(p.dev, None).await.unwrap(),
        BranchDeleteOutcome::NotMoved
    );
    p.none();
}

#[tokio::test]
async fn a_refused_branch_delete_publishes_nothing() {
    let p = p();
    p.fake.state().branches.get_mut(&p.dev).unwrap().protected = true;
    assert_eq!(
        p.m.begin_branch_delete(p.dev, None).await.unwrap(),
        BranchDeleteOutcome::Protected
    );
    p.none();
}

#[tokio::test]
async fn claim_branch_for_switch_publishes_resetting_only_when_claimed() {
    let p = p();
    assert_eq!(
        p.m.claim_branch_for_switch(p.dev, false, None)
            .await
            .unwrap(),
        ClaimOutcome::Claimed
    );
    assert_eq!(
        p.one_after("claim_branch_for_switch"),
        status_event(p.db, p.dev, BranchStatus::Resetting)
    );
    p.rec.published.lock().unwrap().clear();
    assert_eq!(
        p.m.claim_branch_for_switch(p.dev, false, None)
            .await
            .unwrap(),
        ClaimOutcome::NotReady
    );
    p.none();
}

#[tokio::test]
async fn claim_stale_reset_publishes_resetting_when_it_claimed() {
    let p = p();
    set(&p.fake, p.dev, BranchStatus::Resetting);
    assert!(
        !p.m.claim_stale_reset(p.dev, 60).await.unwrap(),
        "not stale yet"
    );
    p.none();
    p.fake.state().branch_ages.insert(p.dev, 3600);
    assert!(p.m.claim_stale_reset(p.dev, 60).await.unwrap());
    assert_eq!(
        p.one_after("claim_stale_reset"),
        status_event(p.db, p.dev, BranchStatus::Resetting)
    );
}

#[tokio::test]
async fn fail_stuck_restore_publishes_failed() {
    let p = p();
    {
        let mut state = p.fake.state();
        let b = state.branches.get_mut(&p.dev).unwrap();
        b.status = BranchStatus::Resetting;
        b.pending_timeline_id = Some("pending".into());
        b.pending_timeline_at = Some(chrono::Utc::now() - chrono::Duration::hours(2));
    }
    assert!(p.m.fail_stuck_restore(p.dev, 60, "stuck").await.unwrap());
    assert_eq!(
        p.one_after("fail_stuck_restore"),
        status_event(p.db, p.dev, BranchStatus::Failed)
    );
}

#[tokio::test]
async fn fail_branch_if_stale_publishes_failed_only_when_it_failed() {
    let p = p();
    set(&p.fake, p.dev, BranchStatus::Provisioning);
    assert!(!p.m.fail_branch_if_stale(p.dev, 60).await.unwrap());
    p.none();
    p.fake.state().branch_ages.insert(p.dev, 3600);
    assert!(p.m.fail_branch_if_stale(p.dev, 60).await.unwrap());
    assert_eq!(
        p.one_after("fail_branch_if_stale"),
        status_event(p.db, p.dev, BranchStatus::Failed)
    );
}

#[tokio::test]
async fn a_restore_publishes_its_new_backup() {
    let p = p();
    {
        let mut state = p.fake.state();
        let b = state.branches.get_mut(&p.dev).unwrap();
        b.status = BranchStatus::Resetting;
        b.timeline_id = Some("old".into());
        b.pending_timeline_id = Some("restored".into());
    }
    let backup = BranchId::new();
    assert!(
        p.m.switch_to_restored_timeline(p.dev, "restored", backup, "dev-before-restore", "0/1")
            .await
            .unwrap()
    );
    assert_eq!(
        p.one_after("switch_to_restored_timeline"),
        ProjectEvent::BranchCreated {
            database_id: p.db.as_uuid(),
            branch_id: backup.as_uuid(),
        }
    );
}

#[tokio::test]
async fn mark_branch_deleted_publishes_branch_deleted_with_ids_read_before_the_write() {
    let p = p();
    set(&p.fake, p.dev, BranchStatus::Deleting);
    // The row is gone once the write returns: only a lookup made before it knows the ids.
    p.fake.state().purge_on_mark_deleted = true;
    p.m.mark_branch_deleted(p.dev).await.unwrap();
    assert_eq!(
        p.one_after("mark_branch_deleted"),
        ProjectEvent::BranchDeleted {
            database_id: p.db.as_uuid(),
            branch_id: p.dev.as_uuid(),
        }
    );
}

#[tokio::test]
async fn a_failed_branch_tombstone_publishes_nothing() {
    let p = p();
    p.fail("mark_branch_deleted");
    assert!(p.m.mark_branch_deleted(p.dev).await.is_err());
    p.none();
}

// --- Database status writes (main's status mirrors the database's) ---------------------------

#[tokio::test]
async fn set_status_publishes_mains_status() {
    let p = p();
    let main = BranchId::from(p.db);
    p.fake.state().databases.get_mut(&p.db).unwrap().status = DatabaseStatus::Provisioning;
    assert!(p.m.set_status(p.db, DatabaseStatus::Ready).await.unwrap());
    assert_eq!(
        p.one_after("set_status"),
        status_event(p.db, main, BranchStatus::from(DatabaseStatus::Ready))
    );
    // Deleting: refused, nothing published.
    p.rec.published.lock().unwrap().clear();
    p.fake.state().databases.get_mut(&p.db).unwrap().status = DatabaseStatus::Deleting;
    assert!(!p.m.set_status(p.db, DatabaseStatus::Ready).await.unwrap());
    p.none();
}

#[tokio::test]
async fn set_failed_publishes_mains_failed() {
    let p = p();
    p.m.set_failed(p.db, "boom").await.unwrap();
    assert_eq!(
        p.one_after("set_failed"),
        status_event(p.db, BranchId::from(p.db), BranchStatus::Failed)
    );
}

#[tokio::test]
async fn fail_if_stale_publishes_mains_failed_only_when_it_failed() {
    let p = p();
    p.fake.state().databases.get_mut(&p.db).unwrap().status = DatabaseStatus::Provisioning;
    assert!(!p.m.fail_if_stale(p.db, 60).await.unwrap());
    p.none();
    p.fake.state().ages.insert(p.db, 3600);
    assert!(p.m.fail_if_stale(p.db, 60).await.unwrap());
    assert_eq!(
        p.one_after("fail_if_stale"),
        status_event(p.db, BranchId::from(p.db), BranchStatus::Failed)
    );
}

#[tokio::test]
async fn mark_deleted_publishes_database_deleted_with_ids_read_before_the_write() {
    let p = p();
    p.fake.state().databases.get_mut(&p.db).unwrap().status = DatabaseStatus::Deleting;
    p.fake.state().purge_on_mark_deleted = true;
    p.m.mark_deleted(p.db).await.unwrap();
    assert_eq!(
        p.one_after("mark_deleted"),
        ProjectEvent::DatabaseDeleted {
            database_id: p.db.as_uuid(),
        }
    );
}

#[tokio::test]
async fn a_lookup_failure_never_fails_the_write() {
    let p = p();
    // The wrapper's own reads fail: the write still happens and succeeds.
    p.fail("get_database");
    let changed =
        p.m.set_branch_status(p.dev, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await;
    assert!(matches!(changed, Ok(true)), "{changed:?}");
    assert_eq!(p.fake.branch(p.dev).status, BranchStatus::Resetting);
}

#[tokio::test]
async fn reads_and_other_writes_publish_nothing() {
    let p = p();
    p.m.get_branch(p.dev).await.unwrap();
    p.m.get_database(p.db).await.unwrap();
    p.m.list_branches(p.db).await.unwrap();
    p.m.set_pending_timeline(p.dev, Some("tl")).await.unwrap();
    p.none();
}

#[tokio::test]
async fn the_payload_is_the_pinned_json() {
    // The subject's payload is the shared `ProjectEvent` (tagged `event`), as realtime reads it.
    let p = p();
    p.m.set_branch_failed(p.dev, "boom").await.unwrap();
    let entry = p
        .journal
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.starts_with("rt.project "))
        .cloned()
        .expect("published");
    let json = entry.splitn(3, ' ').nth(2).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(json).unwrap(),
        serde_json::json!({
            "event": "branch.status",
            "database_id": p.db.as_uuid(),
            "branch_id": p.dev.as_uuid(),
            "status": "failed",
        })
    );
}

// --- RT1 final fix round (batch B, T5 hardening) ---------------------------------------------

/// `dev` with a backup (`dev-before-restore`, made by a restore); nothing published yet.
async fn with_backup(p: &P) -> BranchId {
    {
        let mut state = p.fake.state();
        let b = state.branches.get_mut(&p.dev).unwrap();
        b.status = BranchStatus::Resetting;
        b.timeline_id = Some("old".into());
        b.pending_timeline_id = Some("restored".into());
    }
    let backup = BranchId::new();
    assert!(
        p.m.switch_to_restored_timeline(p.dev, "restored", backup, "dev-before-restore", "0/1")
            .await
            .unwrap()
    );
    set(&p.fake, p.dev, BranchStatus::Ready);
    set(&p.fake, backup, BranchStatus::Ready);
    p.rec.published.lock().unwrap().clear();
    p.journal.lock().unwrap().clear();
    backup
}

#[tokio::test]
async fn a_branch_delete_publishes_deleting_for_the_backups_it_takes_along() {
    let p = p();
    let backup = with_backup(&p).await;
    assert_eq!(
        p.m.begin_branch_delete(p.dev, None).await.unwrap(),
        BranchDeleteOutcome::Started
    );
    // The fake moved the backup too (the real update does).
    assert_eq!(p.fake.branch(backup).status, BranchStatus::Deleting);
    let mut events = p.rec.events();
    events.sort_by_key(|(_, ev)| format!("{ev:?}"));
    let mut expected = vec![
        (p.project, status_event(p.db, p.dev, BranchStatus::Deleting)),
        (p.project, status_event(p.db, backup, BranchStatus::Deleting)),
    ];
    expected.sort_by_key(|(_, ev)| format!("{ev:?}"));
    assert_eq!(events, expected, "the branch and each backup it moved");
    let write = at(&p.journal, "metadata.begin_branch_delete").expect("the write ran");
    let publish = at(&p.journal, "rt.project").expect("published");
    assert!(write < publish, "{:?}", p.journal.lock().unwrap());
}

#[tokio::test]
async fn a_backup_already_deleting_is_not_announced_again() {
    let p = p();
    let backup = with_backup(&p).await;
    set(&p.fake, backup, BranchStatus::Deleting);
    assert_eq!(
        p.m.begin_branch_delete(p.dev, None).await.unwrap(),
        BranchDeleteOutcome::Started
    );
    assert_eq!(
        p.rec.events(),
        vec![(p.project, status_event(p.db, p.dev, BranchStatus::Deleting))]
    );
}

#[tokio::test]
async fn a_second_tombstone_publishes_nothing() {
    let p = p();
    set(&p.fake, p.dev, BranchStatus::Deleting);
    p.m.mark_branch_deleted(p.dev).await.unwrap();
    assert_eq!(p.rec.events().len(), 1, "{:?}", p.rec.events());
    p.rec.published.lock().unwrap().clear();
    // The worker retries a delete (a crash after the tombstone, a redelivered job).
    p.m.mark_branch_deleted(p.dev).await.unwrap();
    p.none();
}

#[tokio::test]
async fn rehearsal_branches_publish_nothing() {
    // A merge's rehearsal is never shown to anyone: its status writes stay off the bus.
    let p = p();
    let rehearsal = p
        .m
        .create_rehearsal_branch(p.db, p.dev, "merge-rehearsal-1")
        .await
        .unwrap();
    assert_eq!(rehearsal.kind, chelabase_metadata::BranchKind::Rehearsal);
    let id = rehearsal.id;
    assert!(
        p.m.set_branch_status(id, &[BranchStatus::Pending], BranchStatus::Provisioning)
            .await
            .unwrap()
    );
    assert!(p.m.set_branch_ready(id, "sealed", None, None).await.unwrap());
    p.m.set_branch_error(id, Some("x")).await.unwrap();
    p.m.set_branch_failed(id, "boom").await.unwrap();
    set(&p.fake, id, BranchStatus::Ready);
    assert_eq!(
        p.m.begin_branch_delete(id, None).await.unwrap(),
        BranchDeleteOutcome::Started
    );
    p.m.mark_branch_deleted(id).await.unwrap();
    p.none();
}
