//! RT1 T5 against real computes: the `chelabase_realtime` role (R1), its `CONNECT` grants
//! (P15, S22) and the compute-stopping signal (P9, S19).
//!
//! The workbench stack (the round-3 images in `CHELABASE_COMPUTE_IMAGE`), the runner's
//! `CHELABASE_REALTIME_PG_PASSWORD_FILE` and NATS credentials (`CHELABASE_NATS_*`):
//!
//! ```text
//! just workbench test -p worker --test realtime_role_live -- --run-ignored only
//! ```
//!
//! The worker runs in-process (`common::setup_with`); each test provisions its own database in
//! the dev tenant and deletes it afterwards, pass or fail. No test calls `reconcile_once`: it
//! acts on every database of the stack (and with the settings version at 4 it would restart
//! every older compute); the reconcile backfill is the unit tests'. The role setting is read from
//! `pg_db_role_setting` as `cloud_admin` (S21: in a walsender the fork refuses `SHOW` to the
//! role; the spill effect of the setting is T8's live check).

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chelabase_metadata::{
    BranchStatus, DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus, MetadataClient, NewBranch,
};
use chelabase_nats::rt::{COMPUTE_STOPPING, ComputeStopping, ProjectEvent};
use chelabase_nats::{BranchJob, NatsConfig, NatsError};
use chelabase_neon::{ComputeBackend, ComputeSettings, ComputeStatus, DockerBackend};
use chelabase_types::{BranchId, DatabaseId};
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use uuid::Uuid;
use worker::config::{RealWorker, WorkerConfig};
use worker::rt_events::{NatsPublisher, RtPublisher};

mod common;
use common::{cleanup, setup, setup_with, workspace_root};

const ROLE: &str = "chelabase_realtime";

/// The password the stack generated for the role (the file the worker reads too).
fn realtime_password() -> String {
    let raw = std::env::var("CHELABASE_REALTIME_PG_PASSWORD_FILE")
        .expect("CHELABASE_REALTIME_PG_PASSWORD_FILE (run through just workbench test)");
    let mut path = PathBuf::from(raw.trim());
    if path.is_relative() {
        path = workspace_root().join(path);
    }
    let password = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    password.trim_end_matches(['\n', '\r']).to_string()
}

/// Provisions a database in the dev tenant, runs `body` with it, and cleans up, pass or fail.
async fn with_database<F, Fut>(worker: RealWorker, metadata: MetadataClient, body: F)
where
    F: FnOnce(Arc<RealWorker>, MetadataClient, DatabaseId, String) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let worker = Arc::new(worker);
    let id = DatabaseId::new();
    let name = format!("rt-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");
    let outcome = tokio::spawn({
        let (worker, metadata) = (worker.clone(), metadata.clone());
        async move {
            tokio::time::timeout(Duration::from_secs(120), worker.handle_provision(id, &name))
                .await
                .expect("provision took over 120 s")
                .expect("handle_provision");
            let record = metadata.get_database(id).await.unwrap().unwrap();
            assert_eq!(record.status, DatabaseStatus::Ready, "{:?}", record.error);
            let database_name = record.database_name.expect("database name");
            body(worker, metadata, id, database_name).await;
        }
    })
    .await;
    // Branches first (their computes), then the database's own rows.
    let _ = worker.handle_delete(id).await;
    cleanup(&worker, &metadata, id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

/// `sql` as cloud_admin in `database` on `branch`'s compute; trimmed tuples-only output.
async fn sql(
    worker: &RealWorker,
    metadata: &MetadataClient,
    branch: BranchId,
    database: &str,
    sql: &str,
) -> String {
    let ep = metadata
        .get_branch(branch)
        .await
        .unwrap()
        .and_then(|b| b.endpoint_id)
        .expect("the branch has an endpoint");
    worker
        .computes
        .admin_sql(&ep, database, sql)
        .await
        .unwrap_or_else(|e| panic!("sql on {branch}: {e:#}"))
        .trim()
        .to_string()
}

/// The role exists with R1's attributes, the stack's password and its setting.
async fn assert_role(
    worker: &RealWorker,
    metadata: &MetadataClient,
    branch: BranchId,
    database: &str,
) {
    let attrs = sql(
        worker,
        metadata,
        branch,
        database,
        "SELECT rolcanlogin, rolreplication, rolbypassrls, rolinherit, rolconnlimit, rolsuper, \
         rolcreatedb, rolcreaterole FROM pg_roles WHERE rolname = 'chelabase_realtime';",
    )
    .await;
    assert_eq!(
        attrs, "t|t|f|f|12|f|f|f",
        "LOGIN REPLICATION NOBYPASSRLS NOINHERIT CONNECTION LIMIT 12"
    );
    let verifier = sql(
        worker,
        metadata,
        branch,
        database,
        "SELECT rolpassword FROM pg_authid WHERE rolname = 'chelabase_realtime';",
    )
    .await;
    assert!(
        chelabase_neon::scram::verifies(&verifier, &realtime_password()),
        "the role's password is the stack's secret"
    );
    // S21: the role setting, as cloud_admin (not `SHOW` in a walsender).
    let setting = sql(
        worker,
        metadata,
        branch,
        database,
        "SELECT array_to_string(s.setconfig, ',') FROM pg_db_role_setting s \
         JOIN pg_roles r ON r.oid = s.setrole \
         WHERE r.rolname = 'chelabase_realtime' AND s.setdatabase = 0;",
    )
    .await;
    assert!(
        setting
            .split(',')
            .any(|s| s == "logical_decoding_work_mem=8MB"),
        "{setting:?}"
    );
}

/// Whether `database`'s ACL names the role with CONNECT (not through PUBLIC).
async fn has_connect(
    worker: &RealWorker,
    metadata: &MetadataClient,
    branch: BranchId,
    database: &str,
) -> bool {
    let out = sql(
        worker,
        metadata,
        branch,
        "postgres",
        &format!(
            "SELECT EXISTS (SELECT FROM pg_database d, aclexplode(d.datacl) a \
             WHERE d.datname = '{}' AND a.grantee = 'chelabase_realtime'::regrole \
             AND a.privilege_type = 'CONNECT');",
            database.replace('\'', "''")
        ),
    )
    .await;
    out == "t"
}

/// What the API does for a branch create, then the create job.
async fn create_branch(
    worker: &RealWorker,
    metadata: &MetadataClient,
    db: DatabaseId,
    name: &str,
) -> BranchId {
    let id = BranchId::new();
    let outcome = metadata
        .create_branch(&NewBranch {
            id,
            database_id: db,
            name,
            parent_branch_id: BranchId::from(db),
            branched_at_time: None,
            branched_at_lsn: None,
        })
        .await
        .unwrap();
    assert_eq!(outcome, chelabase_metadata::CreateBranchOutcome::Created);
    worker
        .handle_branch_job(BranchJob::Create {
            branch_id: id,
            at: None,
        })
        .await
        .expect("create job");
    let b = metadata.get_branch(id).await.unwrap().unwrap();
    assert_eq!(b.status, BranchStatus::Ready, "{:?}", b.error);
    id
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn provisioned_branches_have_the_realtime_role_and_connect_everywhere() {
    let (worker, metadata, _config) = setup().await;
    with_database(
        worker,
        metadata,
        |worker, metadata, id, database| async move {
            let main = BranchId::from(id);
            assert_role(&worker, &metadata, main, &database).await;
            assert!(
                has_connect(&worker, &metadata, main, &database).await,
                "CONNECT on the main database"
            );

            // Never recorded as a login role: not in the proxy's role secrets nor any role list.
            let ep = metadata
                .get_branch(main)
                .await
                .unwrap()
                .unwrap()
                .endpoint_id
                .unwrap();
            let (copies, _) = metadata.role_copies(&ep).await.unwrap();
            assert!(
                copies.iter().all(|c| c.name != ROLE),
                "{:?}",
                copies.iter().map(|c| &c.name).collect::<Vec<_>>()
            );

            // A database made later (the DDL hook's member-roles reconcile): granted too.
            sql(
                &worker,
                &metadata,
                main,
                "postgres",
                "CREATE DATABASE rt_later;",
            )
            .await;
            assert!(
                !has_connect(&worker, &metadata, main, "rt_later").await,
                "not before the reconcile"
            );
            worker.reconcile_member_roles(main, None).await;
            assert!(
                has_connect(&worker, &metadata, main, "rt_later").await,
                "CONNECT on the later database"
            );

            // A child branch inherits the role, its attributes and its setting.
            let dev = create_branch(&worker, &metadata, id, "dev").await;
            assert_role(&worker, &metadata, dev, &database).await;
            assert!(has_connect(&worker, &metadata, dev, &database).await);
        },
    )
    .await;
}

/// RT1 final fix round (coordinator ruling 2): PUBLIC's `CONNECT` (the default, or granted
/// explicitly) never stands in for the role's own grant, so a later `REVOKE … FROM PUBLIC`
/// leaves realtime connected; the guarded script runs twice without error.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn public_connect_never_stands_in_for_the_roles_own_grant() {
    let (worker, metadata, _config) = setup().await;
    with_database(
        worker,
        metadata,
        |worker, metadata, id, _database| async move {
            let main = BranchId::from(id);
            let run = |q: String| {
                let (worker, metadata) = (worker.clone(), metadata.clone());
                async move { sql(&worker, &metadata, main, "postgres", &q).await }
            };
            run("CREATE DATABASE rt_public;".into()).await;
            run("GRANT CONNECT ON DATABASE rt_public TO PUBLIC;".into()).await;
            assert_eq!(
                run("SELECT has_database_privilege('chelabase_realtime', 'rt_public', 'CONNECT');"
                    .into())
                .await,
                "t",
                "PUBLIC's CONNECT answers for the role"
            );
            assert!(!has_connect(&worker, &metadata, main, "rt_public").await);
            let script = worker::realtime::realtime_connect_sql(&["rt_public"]);
            run(script.clone()).await;
            assert!(
                has_connect(&worker, &metadata, main, "rt_public").await,
                "the role's own entry, despite PUBLIC's"
            );
            run(script).await;
            run("REVOKE CONNECT ON DATABASE rt_public FROM PUBLIC;".into()).await;
            assert_eq!(
                run("SELECT has_database_privilege('chelabase_realtime', 'rt_public', 'CONNECT');"
                    .into())
                .await,
                "t",
                "still connected once PUBLIC lost CONNECT"
            );
        },
    )
    .await;
}

/// Each compute-stopping publish: the branch, and `ep`'s compute status at that moment.
type Seen = Arc<Mutex<Vec<(Uuid, Option<ComputeStatus>)>>>;

/// Publishes through NATS, but first records whether `ep`'s compute was still running.
struct Watching {
    nats: NatsPublisher,
    docker: DockerBackend,
    ep: Arc<Mutex<Option<String>>>,
    running_at_publish: Seen,
}

impl RtPublisher for Watching {
    fn project(&self, project_id: Uuid, ev: ProjectEvent) -> BoxFuture<'_, Result<(), NatsError>> {
        self.nats.project(project_id, ev)
    }

    fn compute_stopping(&self, branch_id: Uuid) -> BoxFuture<'_, Result<(), NatsError>> {
        Box::pin(async move {
            let ep = self.ep.lock().unwrap().clone();
            if let Some(ep) = ep {
                let status = self.docker.status(&ep).await.ok().flatten();
                self.running_at_publish
                    .lock()
                    .unwrap()
                    .push((branch_id, status));
            }
            self.nats.compute_stopping(branch_id).await
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn compute_stopping_is_published_before_a_reset_stops_the_compute() {
    let nats = chelabase_nats::connect(&NatsConfig::from_env().expect("nats config"))
        .await
        .expect("nats (the runner's credentials)");
    let config = WorkerConfig::from_env().expect("worker config");
    let docker = DockerBackend::connect(ComputeSettings {
        image: config.compute_image.clone(),
        network: config.compute_network.clone(),
        control_plane_uri: config.control_plane_uri.clone(),
        stack: config.stack.clone(),
        control_network: config.compute_control_network.clone(),
        ca: None,
    })
    .expect("a compute backend for the stack's computes (CHELABASE_STACK)");
    let ep_slot = Arc::new(Mutex::new(None));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let publisher = Arc::new(Watching {
        nats: NatsPublisher::new(nats.clone()),
        docker,
        ep: ep_slot.clone(),
        running_at_publish: seen.clone(),
    });
    let (worker, metadata, _config) = setup_with(publisher).await;
    with_database(
        worker,
        metadata,
        move |worker, metadata, id, _database| async move {
            let dev = create_branch(&worker, &metadata, id, "dev").await;
            let ep = metadata
                .get_branch(dev)
                .await
                .unwrap()
                .unwrap()
                .endpoint_id
                .unwrap();
            *ep_slot.lock().unwrap() = Some(ep.clone());
            let mut sub = nats
                .subscribe(format!("{COMPUTE_STOPPING}.{}", dev.as_uuid().hyphenated()))
                .await
                .expect("subscribe");
            nats.flush().await.expect("flush");

            // What the API does, then the job.
            assert!(
                metadata
                    .set_branch_status(dev, &[BranchStatus::Ready], BranchStatus::Resetting)
                    .await
                    .unwrap()
            );
            worker
                .handle_branch_job(BranchJob::ResetToParent { branch_id: dev })
                .await
                .expect("reset job");
            let b = metadata.get_branch(dev).await.unwrap().unwrap();
            assert_eq!(b.status, BranchStatus::Ready, "{:?}", b.error);

            let seen = seen.lock().unwrap().clone();
            assert_eq!(
                seen.len(),
                1,
                "one compute-stopping for the reset: {seen:?}"
            );
            assert_eq!(seen[0].0, dev.as_uuid());
            assert_eq!(
                seen[0].1,
                Some(ComputeStatus::Running),
                "published while the compute still ran"
            );
            let msg = tokio::time::timeout(Duration::from_secs(10), sub.next())
                .await
                .expect("delivered within 10 s")
                .expect("a message");
            let body: ComputeStopping =
                serde_json::from_slice(&msg.payload).expect("the pinned payload");
            assert_eq!(
                body,
                ComputeStopping {
                    branch_id: dev.as_uuid()
                }
            );
        },
    )
    .await;
}
