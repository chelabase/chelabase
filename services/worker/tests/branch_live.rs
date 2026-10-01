//! The branching story against the real Neon stack: create at a time, reset to parent,
//! restore, a point outside the history window, and delete everything.
//!
//! Same setup as `neon_live` (`just deps-up`, `just dev`, `CHELADB_COMPUTE_IMAGE`):
//!
//! ```text
//! set -a; . .cheladb/zitadel/demo.env; eval "$(scripts/neon-images.sh)"
//! CHELADB_COMPUTE_IMAGE=$COMPUTE_NODE_IMAGE \
//!   cargo test -p worker --test branch_live -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Jobs go through `handle_branch_job`, the dispatch the worker's consumer uses, after the
//! claims the orchestrator makes before it publishes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cheladb_metadata::{
    BranchDeleteOutcome, BranchRecord, BranchStatus, DEMO_PROJECT_ID, DEMO_TENANT_ID,
    DatabaseStatus, MetadataClient, NewBranch,
};
use cheladb_nats::{BranchJob, PointInTime};
use cheladb_neon::{ComputeBackend, NeonId, StorconClient, StorconError};
use cheladb_types::{BranchId, DatabaseId};
use worker::config::{RealWorker, WorkerConfig};

mod common;
use common::{cleanup, setup, sqlx_delete};

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
        assert_eq!(outcome, cheladb_metadata::CreateBranchOutcome::Created);
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
                &cheladb_metadata::SettingsPatch {
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
    let storcon = StorconClient::new(&config.storcon_url, 0);
    for timeline in &timelines {
        match storcon.get_timeline(&tenant, timeline).await {
            Err(StorconError::Rejected { status, .. }) => assert_eq!(status.as_u16(), 404),
            other => panic!("timeline {timeline} still there: {other:?}"),
        }
    }
    for branch in metadata.list_branches(id).await.unwrap() {
        assert_eq!(branch.status, BranchStatus::Deleted, "{}", branch.name);
    }
    sqlx_delete(metadata, id).await.unwrap();
}
