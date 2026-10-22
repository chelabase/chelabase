//! The branching story against the real Neon stack: create at a time, reset to parent,
//! restore, a point outside the history window, and delete everything.
//!
//! Same setup as `neon_live` (the workbench stack, `CHELABASE_COMPUTE_IMAGE`):
//!
//! ```text
//! just workbench test -p worker --test branch_live -- --run-ignored only
//! ```
//!
//! Jobs go through `handle_branch_job`, the dispatch the worker's consumer uses, after the
//! claims the orchestrator makes before it publishes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chelabase_metadata::{
    BranchDeleteOutcome, BranchRecord, BranchStatus, DEMO_PROJECT_ID, DEMO_TENANT_ID,
    DatabaseStatus, MetadataClient, NewBranch,
};
use chelabase_nats::{BranchJob, PointInTime};
use chelabase_neon::{ComputeBackend, Lsn, NeonId, StorconClient, StorconError};
use chelabase_types::{BranchId, DatabaseId};
use worker::config::{RealWorker, WorkerConfig};

mod common;
use common::{cleanup, pageserver_container, setup, sqlx_delete, storage_broker_container};

struct Story<'a> {
    worker: &'a RealWorker,
    metadata: &'a MetadataClient,
    db: DatabaseId,
    database_name: String,
}

impl Story<'_> {
    async fn sql(&self, branch: &BranchRecord, sql: &str) -> String {
        let ep = branch.endpoint_id.as_deref().expect("branch endpoint");
        self.worker
            .computes
            .admin_sql(ep, &self.database_name, sql)
            .await
            .unwrap_or_else(|e| panic!("sql on {}: {e:#}", branch.name))
    }

    /// The rows of `t`, sorted and comma separated.
    async fn rows(&self, branch: &BranchRecord) -> String {
        let out = self
            .sql(
                branch,
                "SELECT coalesce(string_agg(v, ',' ORDER BY v), '') FROM t;",
            )
            .await;
        out.lines()
            .map(str::trim)
            .find(|l| {
                !l.is_empty()
                    && !l.starts_with('(')
                    && !l.starts_with('-')
                    && *l != "string_agg"
                    && *l != "coalesce"
            })
            .unwrap_or("")
            .to_string()
    }

    async fn branch(&self, id: BranchId) -> BranchRecord {
        self.metadata
            .get_branch(id)
            .await
            .unwrap()
            .expect("branch row")
    }

    /// What the API does for a create: insert the pending row, then publish the job.
    async fn create(&self, name: &str, parent: BranchId, at: Option<PointInTime>) -> BranchId {
        let id = BranchId::new();
        let (time, lsn) = match &at {
            Some(PointInTime::Timestamp(t)) => (Some(*t), None),
            Some(PointInTime::Lsn(l)) => (None, Some(l.as_str())),
            None => (None, None),
        };
        let outcome = self
            .metadata
            .create_branch(&NewBranch {
                id,
                database_id: self.db,
                name,
                parent_branch_id: parent,
                branched_at_time: time,
                branched_at_lsn: lsn,
            })
            .await
            .unwrap();
        assert_eq!(outcome, chelabase_metadata::CreateBranchOutcome::Created);
        let started = Instant::now();
        self.worker
            .handle_branch_job(BranchJob::Create { branch_id: id, at })
            .await
            .expect("create job");
        println!("create {name} took {:?}", started.elapsed());
        id
    }

    async fn claim(&self, id: BranchId) {
        let moved = self
            .metadata
            .set_branch_status(id, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap();
        assert!(moved, "branch was not ready");
        assert!(self.metadata.live_children(id).await.unwrap().is_empty());
    }

    async fn delete(&self, id: BranchId) {
        self.claim(id).await;
        assert_eq!(
            self.metadata.begin_branch_delete(id).await.unwrap(),
            BranchDeleteOutcome::Started
        );
        let started = Instant::now();
        self.worker
            .handle_branch_job(BranchJob::Delete { branch_id: id })
            .await
            .expect("delete job");
        println!("delete branch took {:?}", started.elapsed());
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn branching_story() {
    let (worker, metadata, config) = setup().await;
    let worker = Arc::new(worker);
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");

    // A panic inside the task is caught here, so the cleanup below always runs.
    let outcome = tokio::spawn({
        let (worker, metadata, config) = (worker.clone(), metadata.clone(), config.clone());
        async move { story(&worker, &metadata, &config, id, &name).await }
    })
    .await;
    if let Err(panic) = outcome {
        cleanup(&worker, &metadata, id).await;
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn story(
    worker: &RealWorker,
    metadata: &MetadataClient,
    config: &WorkerConfig,
    id: DatabaseId,
    name: &str,
) {
    // 1. Provision; row A; the noted time; row B.
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(90), worker.handle_provision(id, name))
        .await
        .expect("provision took over 90 s")
        .expect("handle_provision");
    println!("provision took {:?}", started.elapsed());
    let record = metadata.get_database(id).await.unwrap().unwrap();
    assert_eq!(record.status, DatabaseStatus::Ready, "{:?}", record.error);
    let story = Story {
        worker,
        metadata,
        db: id,
        database_name: record.database_name.clone().expect("database name"),
    };
    let main_id = BranchId::from(id);
    let main = story.branch(main_id).await;
    let tenant: NeonId = record
        .neon_tenant_id
        .as_deref()
        .expect("tenant")
        .parse()
        .unwrap();

    story
        .sql(
            &main,
            "CREATE TABLE t (v text); INSERT INTO t VALUES ('A');",
        )
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let noted = chrono::Utc::now();
    tokio::time::sleep(Duration::from_secs(2)).await;
    story.sql(&main, "INSERT INTO t VALUES ('B');").await;
    assert_eq!(story.rows(&main).await, "A,B");

    // 2. A branch at the noted time has A and not B.
    let dev_id = story
        .create("dev", main_id, Some(PointInTime::Timestamp(noted)))
        .await;
    let dev = story.branch(dev_id).await;
    assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
    assert_eq!(story.rows(&dev).await, "A", "branch at the noted time");

    // 3. C on main; reset the branch to its parent: A, B and C.
    story.sql(&main, "INSERT INTO t VALUES ('C');").await;
    let old_timeline = dev.timeline_id.clone();
    story.claim(dev_id).await;
    let started = Instant::now();
    worker
        .handle_branch_job(BranchJob::ResetToParent { branch_id: dev_id })
        .await
        .expect("reset job");
    println!("reset to parent took {:?}", started.elapsed());
    let dev = story.branch(dev_id).await;
    assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
    assert_ne!(dev.timeline_id, old_timeline, "the timeline didn't change");
    assert_eq!(story.rows(&dev).await, "A,B,C", "branch after reset");

    // Restore needs main without children.
    story.delete(dev_id).await;
    assert_eq!(story.branch(dev_id).await.status, BranchStatus::Deleted);

    // 4. Restore main to the noted time: A only; the backup has B and C.
    story.claim(main_id).await;
    let backup_id = BranchId::new();
    let backup_name = "app-before-restore-20260930T120000Z".to_string();
    let started = Instant::now();
    worker
        .handle_branch_job(BranchJob::Restore {
            branch_id: main_id,
            backup_id,
            backup_name: backup_name.clone(),
            at: PointInTime::Timestamp(noted),
        })
        .await
        .expect("restore job");
    println!("restore took {:?}", started.elapsed());
    let main = story.branch(main_id).await;
    assert_eq!(main.status, BranchStatus::Ready, "{:?}", main.error);
    assert_eq!(story.rows(&main).await, "A", "main after restore");
    let backup = story.branch(backup_id).await;
    assert_eq!(backup.name, backup_name);
    assert_eq!(backup.status, BranchStatus::Ready, "{:?}", backup.error);
    assert_eq!(backup.endpoint_id, None, "a backup has no compute");
    let from_backup_id = story.create("from-backup", backup_id, None).await;
    let from_backup = story.branch(from_backup_id).await;
    assert_eq!(
        from_backup.status,
        BranchStatus::Ready,
        "{:?}",
        from_backup.error
    );
    assert_eq!(
        story.rows(&from_backup).await,
        "A,B,C",
        "branch of the backup"
    );

    // 5. A point outside the history window (1 h; 2 h back).
    assert!(
        metadata
            .update_database_settings(
                id,
                &chelabase_metadata::SettingsPatch {
                    history_window: Some(Duration::from_secs(3600)),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    );
    worker
        .handle_database_settings(id)
        .await
        .expect("settings job");
    let old = chrono::Utc::now() - chrono::Duration::hours(2);
    let too_old_id = story
        .create("too-old", main_id, Some(PointInTime::Timestamp(old)))
        .await;
    let too_old = story.branch(too_old_id).await;
    assert_eq!(too_old.status, BranchStatus::Failed);
    let error = too_old.error.clone().unwrap_or_default();
    println!("outside the window: {error}");
    let oldest = error
        .rsplit_once("outside the history window; the oldest available point is ")
        .map(|(_, oldest)| oldest)
        .unwrap_or_else(|| panic!("{error}"));
    assert!(
        chrono::DateTime::parse_from_rfc3339(oldest).is_ok(),
        "the oldest point is a time: {error}"
    );

    // 6. Delete everything.
    delete_everything(worker, metadata, config, id, &tenant).await;
}

/// Deletes the database, then checks that its computes, endpoint rows and timelines are gone
/// and its branches deleted, and removes its rows.
async fn delete_everything(
    worker: &RealWorker,
    metadata: &MetadataClient,
    config: &WorkerConfig,
    id: DatabaseId,
    tenant: &NeonId,
) {
    let branches = metadata.list_branches(id).await.unwrap();
    let endpoints: Vec<String> = branches
        .iter()
        .filter_map(|b| b.endpoint_id.clone())
        .collect();
    let timelines: Vec<NeonId> = branches
        .iter()
        .filter(|b| b.status != BranchStatus::Deleted)
        .flat_map(|b| {
            [
                &b.timeline_id,
                &b.pending_timeline_id,
                &b.superseded_timeline_id,
            ]
        })
        .flatten()
        .filter_map(|t| t.parse().ok())
        .collect();
    let started = Instant::now();
    worker.handle_delete(id).await.expect("handle_delete");
    println!("delete database took {:?}", started.elapsed());
    for ep in &endpoints {
        if let Ok(Some(status)) = worker.computes.status(ep).await {
            panic!("compute {ep} still there: {status:?}");
        }
        assert!(
            metadata.neon_endpoint(ep).await.unwrap().is_none(),
            "endpoint row {ep}"
        );
    }
    let storcon = admin_storcon(config);
    for timeline in &timelines {
        match storcon.get_timeline(tenant, timeline).await {
            Err(StorconError::Rejected { status, .. }) => assert_eq!(status.as_u16(), 404),
            other => panic!("timeline {timeline} still there: {other:?}"),
        }
    }
    for branch in metadata.list_branches(id).await.unwrap() {
        assert_eq!(branch.status, BranchStatus::Deleted, "{}", branch.name);
    }
    sqlx_delete(metadata, id).await.unwrap();
}

/// Provisions the pending database `id` named `name`; returns its story and tenant.
async fn provisioned<'a>(
    worker: &'a RealWorker,
    metadata: &'a MetadataClient,
    id: DatabaseId,
    name: &str,
) -> (Story<'a>, NeonId) {
    tokio::time::timeout(Duration::from_secs(90), worker.handle_provision(id, name))
        .await
        .expect("provision took over 90 s")
        .expect("handle_provision");
    let record = metadata.get_database(id).await.unwrap().unwrap();
    assert_eq!(record.status, DatabaseStatus::Ready, "{:?}", record.error);
    let tenant: NeonId = record
        .neon_tenant_id
        .as_deref()
        .expect("tenant")
        .parse()
        .unwrap();
    let story = Story {
        worker,
        metadata,
        db: id,
        database_name: record.database_name.clone().expect("database name"),
    };
    (story, tenant)
}

/// The storage controller with an admin NeonJWT (it runs without --dev).
fn admin_storcon(config: &WorkerConfig) -> StorconClient {
    StorconClient::new(&config.storcon_url, 0).with_token(
        chelabase_neon::jwt::storage_token_from_dir(
            &config.compute_jwt_dir,
            &chelabase_neon::jwt::StorageScope::Admin,
        )
        .expect("the compute-JWT key pair"),
    )
}

/// The ancestor of `timeline` and its point on it (`None` once detached), read from the
/// storage controller.
async fn ancestor_of(
    config: &WorkerConfig,
    tenant: &NeonId,
    timeline: &Option<String>,
) -> Option<(String, Lsn)> {
    let timeline: NeonId = timeline.as_deref().expect("a timeline").parse().unwrap();
    let info = admin_storcon(config)
        .get_timeline(tenant, &timeline)
        .await
        .expect("timeline");
    match (info.ancestor_timeline_id, info.ancestor_lsn) {
        (Some(id), Some(lsn)) => Some((id, lsn)),
        (None, None) => None,
        other => panic!("timeline {timeline}: half an ancestor: {other:?}"),
    }
}

/// Restores the claimed `branch` to `at` through the job dispatch; returns the backup's id.
async fn restore(
    story: &Story<'_>,
    branch: BranchId,
    backup_name: &str,
    at: PointInTime,
) -> BranchId {
    story.claim(branch).await;
    let backup_id = BranchId::new();
    let started = Instant::now();
    story
        .worker
        .handle_branch_job(BranchJob::Restore {
            branch_id: branch,
            backup_id,
            backup_name: backup_name.to_string(),
            at,
        })
        .await
        .expect("restore job");
    println!("restore took {:?}", started.elapsed());
    backup_id
}

/// A time with writes 2 s before and after it.
async fn noted_time() -> chrono::DateTime<chrono::Utc> {
    tokio::time::sleep(Duration::from_secs(2)).await;
    let noted = chrono::Utc::now();
    tokio::time::sleep(Duration::from_secs(2)).await;
    noted
}

/// Restoring a child branch: one detach of the restored timeline copies its level and main's
/// up to dev's branch point (Neon fork round 2, patches 1 and 3).
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn restore_a_child_branch() {
    let (worker, metadata, config) = setup().await;
    let worker = Arc::new(worker);
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");

    let outcome = tokio::spawn({
        let (worker, metadata, config) = (worker.clone(), metadata.clone(), config.clone());
        async move { child_story(&worker, &metadata, &config, id, &name).await }
    })
    .await;
    if let Err(panic) = outcome {
        cleanup(&worker, &metadata, id).await;
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn child_story(
    worker: &RealWorker,
    metadata: &MetadataClient,
    config: &WorkerConfig,
    id: DatabaseId,
    name: &str,
) {
    // 1. Provision; A on main; dev from main; M on main after dev's branch point.
    let (story, tenant) = provisioned(worker, metadata, id, name).await;
    let main_id = BranchId::from(id);
    let main = story.branch(main_id).await;
    story
        .sql(
            &main,
            "CREATE TABLE t (v text); INSERT INTO t VALUES ('A');",
        )
        .await;
    let dev_id = story.create("dev", main_id, None).await;
    let dev = story.branch(dev_id).await;
    assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
    story.sql(&main, "INSERT INTO t VALUES ('M');").await;

    // 2. D1 on dev; the noted time; D2 on dev.
    story.sql(&dev, "INSERT INTO t VALUES ('D1');").await;
    let noted = noted_time().await;
    story.sql(&dev, "INSERT INTO t VALUES ('D2');").await;
    assert_eq!(story.rows(&dev).await, "A,D1,D2");
    let old_timeline = dev.timeline_id.clone();
    let old_ancestor = ancestor_of(config, &tenant, &old_timeline).await;
    assert_eq!(
        old_ancestor.as_ref().map(|(id, _)| id.as_str()),
        main.timeline_id.as_deref(),
        "dev starts as a child of main's timeline"
    );

    // 3. Restore dev to the noted time: A and D1.
    let backup_name = "dev-before-restore-20261007T120000Z";
    let backup_id = restore(&story, dev_id, backup_name, PointInTime::Timestamp(noted)).await;
    let dev = story.branch(dev_id).await;
    assert_eq!(dev.error, None, "the restore of dev failed");
    assert_eq!(dev.status, BranchStatus::Ready);
    assert_ne!(dev.timeline_id, old_timeline, "the timeline didn't change");
    assert_eq!(dev.parent_branch_id, Some(main_id), "still main's child");
    assert_eq!(story.rows(&dev).await, "A,D1", "dev after restore");
    assert_eq!(
        ancestor_of(config, &tenant, &dev.timeline_id).await,
        None,
        "the restored timeline is detached"
    );

    // 4. The backup: the old timeline, still a child of main's; a branch of it has D2.
    let backup = story.branch(backup_id).await;
    assert_eq!(backup.name, backup_name);
    assert_eq!(backup.status, BranchStatus::Ready, "{:?}", backup.error);
    assert_eq!(backup.parent_branch_id, Some(dev_id));
    assert_eq!(backup.endpoint_id, None, "a backup has no compute");
    assert_eq!(backup.timeline_id, old_timeline);
    assert_eq!(
        ancestor_of(config, &tenant, &old_timeline).await,
        old_ancestor,
        "only the restored timeline is detached: the old one stays on main's, at its point"
    );
    let from_backup_id = story.create("from-backup", backup_id, None).await;
    let from_backup = story.branch(from_backup_id).await;
    assert_eq!(
        from_backup.status,
        BranchStatus::Ready,
        "{:?}",
        from_backup.error
    );
    assert_eq!(
        story.rows(&from_backup).await,
        "A,D1,D2",
        "branch of the backup"
    );

    // 5. Main is untouched, and dev keeps working: a write, and a child branch of its own.
    let main = story.branch(main_id).await;
    assert_eq!(main.status, BranchStatus::Ready, "{:?}", main.error);
    assert_eq!(story.rows(&main).await, "A,M", "main");
    story.sql(&dev, "INSERT INTO t VALUES ('D3');").await;
    assert_eq!(story.rows(&dev).await, "A,D1,D3");

    // 5b. T14 M8: a restore of dev to an LSN below its branch point (its restored timeline
    // carries main's older history) is refused; dev stays as it is. A restore needs a branch
    // with no live child, so this runs before dev-child exists.
    let refused_name = "dev-before-restore-20261008T130000Z";
    let dev_before = story.branch(dev_id).await;
    let branch_point: Lsn = dev_before
        .branched_at_lsn
        .as_deref()
        .expect("dev's branch point")
        .parse()
        .unwrap();
    let below = Lsn(branch_point.0 - 1).to_string();
    restore(&story, dev_id, refused_name, PointInTime::Lsn(below)).await;
    let dev = story.branch(dev_id).await;
    assert_eq!(dev.status, BranchStatus::Ready);
    assert!(
        dev.error
            .as_deref()
            .is_some_and(|e| e.contains("is before dev's history begins (its branch point,")),
        "{:?}",
        dev.error
    );
    assert_eq!(dev.timeline_id, dev_before.timeline_id, "nothing switched");
    assert_eq!(story.rows(&dev).await, "A,D1,D3");

    let dev_child_id =story.create("dev-child", dev_id, None).await;
    let dev_child = story.branch(dev_child_id).await;
    assert_eq!(
        dev_child.status,
        BranchStatus::Ready,
        "{:?}",
        dev_child.error
    );
    assert_eq!(story.rows(&dev_child).await, "A,D1,D3", "dev's child");

    // 6. Delete everything.
    delete_everything(worker, metadata, config, id, &tenant).await;
}

/// Restoring a grandchild (main → dev → feat): one detach copies all three levels, each up to
/// its child's branch point, and feat's own up to the restore point.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn restore_a_grandchild_branch() {
    let (worker, metadata, config) = setup().await;
    let worker = Arc::new(worker);
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");

    let outcome = tokio::spawn({
        let (worker, metadata, config) = (worker.clone(), metadata.clone(), config.clone());
        async move { grandchild_story(&worker, &metadata, &config, id, &name).await }
    })
    .await;
    if let Err(panic) = outcome {
        cleanup(&worker, &metadata, id).await;
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn grandchild_story(
    worker: &RealWorker,
    metadata: &MetadataClient,
    config: &WorkerConfig,
    id: DatabaseId,
    name: &str,
) {
    // 1. A on main; dev from main; M on main after dev's branch point.
    let (story, tenant) = provisioned(worker, metadata, id, name).await;
    let main_id = BranchId::from(id);
    let main = story.branch(main_id).await;
    story
        .sql(
            &main,
            "CREATE TABLE t (v text); INSERT INTO t VALUES ('A');",
        )
        .await;
    let dev_id = story.create("dev", main_id, None).await;
    let dev = story.branch(dev_id).await;
    assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
    story.sql(&main, "INSERT INTO t VALUES ('M');").await;

    // 2. D1 on dev; feat from dev; D2 on dev after feat's branch point.
    story.sql(&dev, "INSERT INTO t VALUES ('D1');").await;
    let feat_id = story.create("feat", dev_id, None).await;
    let feat = story.branch(feat_id).await;
    assert_eq!(feat.status, BranchStatus::Ready, "{:?}", feat.error);
    story.sql(&dev, "INSERT INTO t VALUES ('D2');").await;

    // 3. F1 on feat; the noted time; F2 on feat.
    story.sql(&feat, "INSERT INTO t VALUES ('F1');").await;
    let noted = noted_time().await;
    story.sql(&feat, "INSERT INTO t VALUES ('F2');").await;
    assert_eq!(story.rows(&feat).await, "A,D1,F1,F2");
    let old_timeline = feat.timeline_id.clone();
    let old_ancestor = ancestor_of(config, &tenant, &old_timeline).await;
    assert_eq!(
        old_ancestor.as_ref().map(|(id, _)| id.as_str()),
        dev.timeline_id.as_deref(),
        "feat starts as a child of dev's timeline"
    );
    let dev_ancestor = ancestor_of(config, &tenant, &dev.timeline_id).await;
    assert_eq!(
        dev_ancestor.as_ref().map(|(id, _)| id.as_str()),
        main.timeline_id.as_deref(),
        "dev is a child of main's timeline"
    );

    // 4. Restore feat to the noted time: main's A (not M), dev's D1 (not D2), feat's F1 (not F2).
    let backup_name = "feat-before-restore-20261008T120000Z";
    let backup_id = restore(&story, feat_id, backup_name, PointInTime::Timestamp(noted)).await;
    let feat = story.branch(feat_id).await;
    assert_eq!(feat.error, None, "the restore of feat failed");
    assert_eq!(feat.status, BranchStatus::Ready);
    assert_ne!(feat.timeline_id, old_timeline, "the timeline didn't change");
    assert_eq!(feat.parent_branch_id, Some(dev_id), "still dev's child");
    assert_eq!(story.rows(&feat).await, "A,D1,F1", "feat after restore");
    assert_eq!(
        ancestor_of(config, &tenant, &feat.timeline_id).await,
        None,
        "the restored timeline is detached"
    );

    // 5. The backup keeps the old timeline (still dev's child); a branch of it has F2.
    let backup = story.branch(backup_id).await;
    assert_eq!(backup.name, backup_name);
    assert_eq!(backup.status, BranchStatus::Ready, "{:?}", backup.error);
    assert_eq!(backup.parent_branch_id, Some(feat_id));
    assert_eq!(backup.timeline_id, old_timeline);
    assert_eq!(
        ancestor_of(config, &tenant, &old_timeline).await,
        old_ancestor,
        "only the restored timeline is detached: the old one stays on dev's, at its point"
    );
    assert_eq!(
        ancestor_of(config, &tenant, &dev.timeline_id).await,
        dev_ancestor,
        "dev's timeline is untouched"
    );
    let from_backup_id = story.create("from-backup", backup_id, None).await;
    let from_backup = story.branch(from_backup_id).await;
    assert_eq!(
        from_backup.status,
        BranchStatus::Ready,
        "{:?}",
        from_backup.error
    );
    assert_eq!(
        story.rows(&from_backup).await,
        "A,D1,F1,F2",
        "branch of the backup"
    );

    // 6. The ancestors are untouched, and feat keeps working.
    let main = story.branch(main_id).await;
    assert_eq!(main.status, BranchStatus::Ready, "{:?}", main.error);
    assert_eq!(story.rows(&main).await, "A,M", "main");
    let dev = story.branch(dev_id).await;
    assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
    assert_eq!(story.rows(&dev).await, "A,D1,D2", "dev");
    story.sql(&feat, "INSERT INTO t VALUES ('F3');").await;
    assert_eq!(story.rows(&feat).await, "A,D1,F1,F3");
    let feat_child_id = story.create("feat-child", feat_id, None).await;
    let feat_child = story.branch(feat_child_id).await;
    assert_eq!(
        feat_child.status,
        BranchStatus::Ready,
        "{:?}",
        feat_child.error
    );
    assert_eq!(story.rows(&feat_child).await, "A,D1,F1,F3", "feat's child");

    // 7. Delete everything.
    delete_everything(worker, metadata, config, id, &tenant).await;
}

/// Patch 6 (D11): right after the pageserver restarts, its `last_record_lsn` for main can lag
/// what the safekeepers have committed; a branch of main with no point must still see main's
/// last committed write.
///
/// The window is produced deterministically, not raced: a graceful restart freezes and flushes
/// everything the pageserver ingested, and it ingests a committed write within milliseconds, so
/// "write, then restart" leaves no lag. Instead the storage broker is stopped (the pageserver
/// finds safekeepers only through it), the pageserver restarted (dropping its WAL stream), and
/// main written: the safekeepers commit the write and nobody streams it to the pageserver. The
/// test asserts the lag is real, then starts the broker and branches at once, while the
/// pageserver is still catching up.

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn branch_right_after_a_pageserver_restart_sees_the_last_write() {
    let (worker, metadata, _config) = setup().await;
    let worker = Arc::new(worker);
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");

    let outcome = tokio::spawn({
        let (worker, metadata) = (worker.clone(), metadata.clone());
        async move {
            stale_head_story(&worker, &metadata, id, &name).await;
            // Right after the pageserver's re-attach the storage controller may not schedule
            // it yet; the client retries that (T1), so the delete is plain.
            worker.handle_delete(id).await.expect("handle_delete");
        }
    })
    .await;
    // A panic inside the task is caught here, so the cleanup below always runs.
    if let Err(panic) = outcome {
        cleanup(&worker, &metadata, id).await;
        std::panic::resume_unwind(panic.into_panic());
    }
    sqlx_delete(&metadata, id).await.unwrap();
}

/// A provision (tenant create) right after a pageserver restart succeeds: the storage
/// controller may still hold the restarted pageserver unschedulable, and the client retries
/// that. The test asserts the provision succeeds, not that a retry happened.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn provision_right_after_a_pageserver_restart_succeeds() {
    let (worker, metadata, _config) = setup().await;
    let worker = Arc::new(worker);
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");

    let outcome = tokio::spawn({
        let (worker, metadata) = (worker.clone(), metadata.clone());
        async move {
            docker("restart", &pageserver_container()).await;
            tokio::time::timeout(Duration::from_secs(120), worker.handle_provision(id, &name))
                .await
                .expect("provision took over 120 s")
                .expect("handle_provision right after the restart");
            let record = metadata.get_database(id).await.unwrap().unwrap();
            assert_eq!(record.status, DatabaseStatus::Ready, "{:?}", record.error);
            worker.handle_delete(id).await.expect("handle_delete");
        }
    })
    .await;
    if let Err(panic) = outcome {
        cleanup(&worker, &metadata, id).await;
        std::panic::resume_unwind(panic.into_panic());
    }
    sqlx_delete(&metadata, id).await.unwrap();
}

/// `docker <verb> <container>`, asserting it succeeded.
async fn docker(verb: &str, container: &str) {
    let status = tokio::process::Command::new("docker")
        .args([verb, container])
        .stdout(std::process::Stdio::null())
        .status()
        .await
        .expect("run docker");
    assert!(status.success(), "docker {verb} {container}");
}

/// The stack's storage broker, stopped while this lives. Dropping it (a panic included) starts
/// the broker again, so a failed run never leaves the shared stack without one.
struct BrokerStopped {
    container: String,
    running: bool,
}

impl BrokerStopped {
    async fn stop() -> Self {
        let container = storage_broker_container();
        docker("stop", &container).await;
        Self {
            container,
            running: false,
        }
    }

    async fn start(mut self) {
        docker("start", &self.container).await;
        self.running = true;
    }
}

impl Drop for BrokerStopped {
    fn drop(&mut self) {
        if !self.running {
            let started = std::process::Command::new("docker")
                .args(["start", &self.container])
                .stdout(std::process::Stdio::null())
                .status();
            if !matches!(started, Ok(s) if s.success()) {
                eprintln!("could not start {} again: {started:?}", self.container);
            }
        }
    }
}

/// Main's timeline as the pageserver sees it, once the storage controller answers for it again.
async fn timeline_after_restart(
    worker: &RealWorker,
    tenant: &NeonId,
    timeline: &NeonId,
) -> chelabase_neon::storcon::TimelineInfo {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match worker.storage.storcon.get_timeline(tenant, timeline).await {
            Ok(info) => return info,
            Err(e) if Instant::now() < deadline => {
                println!("waiting for get_timeline: {e:?}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => panic!("get_timeline after the restart: {e:?}"),
        }
    }
}

/// The safekeepers' commit LSN for the timeline, read from each active one directly: a refused
/// or failed read fails the test (the worker's `head` would fold it into its fallback).
async fn safekeepers_commit_lsn(worker: &RealWorker, tenant: &NeonId, timeline: &NeonId) -> Lsn {
    let active = worker
        .storage
        .storcon
        .active_safekeeper_ids()
        .await
        .expect("active safekeepers");
    assert!(!active.is_empty(), "no active safekeepers");
    let mut commit = None;
    for id in active {
        let client =
            worker.storage.safekeepers.client(id).unwrap_or_else(|| {
                panic!("safekeeper {id} has no URL in CHELABASE_SAFEKEEPER_HTTP")
            });
        match client.commit_lsn(tenant, timeline).await {
            Ok(lsn) => commit = commit.max(lsn),
            Err(e) => panic!("safekeeper {id}: reading the commit LSN: {e:#}"),
        }
    }
    commit.expect("no safekeeper knows main's timeline")
}

/// A committed write on main; right after a restart the compute may need a moment to reconnect
/// to the pageserver, so the statement is retried for a while.
async fn write_main(worker: &RealWorker, endpoint: &str, database_name: &str, value: &str) {
    let sql = format!("INSERT INTO t VALUES ('{value}');");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match worker
            .computes
            .admin_sql(endpoint, database_name, &sql)
            .await
        {
            Ok(_) => return,
            Err(e) if Instant::now() < deadline => {
                println!("write retry: {e:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => panic!("write on main: {e:#}"),
        }
    }
}

async fn stale_head_story(
    worker: &RealWorker,
    metadata: &MetadataClient,
    id: DatabaseId,
    name: &str,
) {
    tokio::time::timeout(Duration::from_secs(90), worker.handle_provision(id, name))
        .await
        .expect("provision took over 90 s")
        .expect("handle_provision");
    let record = metadata.get_database(id).await.unwrap().unwrap();
    assert_eq!(record.status, DatabaseStatus::Ready, "{:?}", record.error);
    let database_name = record.database_name.clone().expect("database name");
    let story = Story {
        worker,
        metadata,
        db: id,
        database_name: database_name.clone(),
    };
    let main_id = BranchId::from(id);
    let main = story.branch(main_id).await;
    let tenant: NeonId = record
        .neon_tenant_id
        .as_deref()
        .expect("tenant")
        .parse()
        .unwrap();
    let timeline: NeonId = main
        .timeline_id
        .as_deref()
        .expect("main timeline")
        .parse()
        .unwrap();
    let endpoint = main.endpoint_id.clone().expect("main endpoint");

    story.sql(&main, "CREATE TABLE t (v text);").await;

    // w1: a committed write the pageserver ingests (and flushes on the graceful restart).
    write_main(worker, &endpoint, &database_name, "w1").await;
    let broker = BrokerStopped::stop().await;
    docker("restart", &pageserver_container()).await;
    let reloaded = timeline_after_restart(worker, &tenant, &timeline).await;
    // w2: committed by the safekeepers; with no broker the pageserver has no WAL stream.
    write_main(worker, &endpoint, &database_name, "w2").await;
    let info = timeline_after_restart(worker, &tenant, &timeline).await;
    let commit = safekeepers_commit_lsn(worker, &tenant, &timeline).await;
    println!(
        "pageserver last_record_lsn {:?} (reloaded at {:?}), safekeepers commit_lsn {commit:?}",
        info.last_record_lsn, reloaded.last_record_lsn
    );
    assert!(
        commit > info.last_record_lsn,
        "no stale head: the pageserver's last_record_lsn {:?} isn't behind the safekeepers' \
         commit_lsn {commit:?} although it had no WAL stream when w2 was written",
        info.last_record_lsn
    );

    // At once: the pageserver can stream again (it still has to find the safekeepers and catch
    // up), and a branch of main with no point sees every committed write.
    broker.start().await;
    let after_id = story.create("after-restart", main_id, None).await;
    let after = story.branch(after_id).await;
    assert_eq!(after.status, BranchStatus::Ready, "{:?}", after.error);
    assert_eq!(
        story.rows(&after).await,
        "w1,w2",
        "branch right after a pageserver restart"
    );
}
