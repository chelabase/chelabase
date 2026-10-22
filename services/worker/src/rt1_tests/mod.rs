//! RT1 T5 RED tests: the worker's side of realtime. One file per area; the shared helpers are
//! here.
//!
//! - `publisher`: the platform-event choke point ([`crate::rt_events::PublishingMetadata`],
//!   P10/S20);
//! - `stopping`: the compute-stopping signal ([`crate::rt_events::StoppingBackend`], P9/S19),
//!   per caller family;
//! - `role`: the `chelabase_realtime` role (R1): its SQL, provisioning, backfill, rotation;
//! - `connect`: `CONNECT` grants (P15, W21, S22);
//! - `exclusions`: the worker's reserved lists (W30, S23);
//! - `config`: `CHELABASE_REALTIME_PG_PASSWORD_FILE` (S1) and `build`'s publisher (S20).

mod config;
mod connect;
mod exclusions;
mod publisher;
mod role;
mod stopping;

use std::sync::{Arc, Mutex};

use chelabase_metadata::{BranchStatus, CreateBranchOutcome, DatabaseStatus, NewBranch};
use chelabase_nats::NatsError;
use chelabase_nats::rt::ProjectEvent;
use chelabase_neon::backend::fake::FakeBackend;
use chelabase_types::{BranchId, DatabaseId};
use futures_util::future::BoxFuture;
use uuid::Uuid;

use crate::Worker;
use crate::fakes::{FakeCache, FakeMetadata, FakePusher, FakeStorage, Journal, worker};
use crate::rt_events::{PublishingMetadata, RtPublisher, StoppingBackend};

/// What a [`RecordingPublisher`] was asked to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Published {
    Project(Uuid, ProjectEvent),
    Stopping(Uuid),
}

/// Records every publish (also in the shared journal, as `rt.project <project> <event json>`
/// and `rt.compute_stopping <branch>`); with `fail` set each one fails like a NATS outage
/// (after it was recorded).
pub(crate) struct RecordingPublisher {
    pub published: Mutex<Vec<Published>>,
    pub fail: Mutex<bool>,
    pub journal: Journal,
}

impl RecordingPublisher {
    pub(crate) fn new(journal: Journal) -> Self {
        Self {
            published: Mutex::default(),
            fail: Mutex::default(),
            journal,
        }
    }

    pub(crate) fn all(&self) -> Vec<Published> {
        self.published.lock().unwrap().clone()
    }

    /// The project events only.
    pub(crate) fn events(&self) -> Vec<(Uuid, ProjectEvent)> {
        self.all()
            .into_iter()
            .filter_map(|p| match p {
                Published::Project(project, ev) => Some((project, ev)),
                Published::Stopping(_) => None,
            })
            .collect()
    }

    /// The branches a compute-stopping signal was published for, in order.
    pub(crate) fn stopping(&self) -> Vec<Uuid> {
        self.all()
            .into_iter()
            .filter_map(|p| match p {
                Published::Stopping(b) => Some(b),
                Published::Project(..) => None,
            })
            .collect()
    }

    fn outcome(&self) -> Result<(), NatsError> {
        if *self.fail.lock().unwrap() {
            Err(NatsError::JetStream("injected NATS outage".into()))
        } else {
            Ok(())
        }
    }
}

impl RtPublisher for RecordingPublisher {
    fn project(&self, project_id: Uuid, ev: ProjectEvent) -> BoxFuture<'_, Result<(), NatsError>> {
        Box::pin(async move {
            let json = serde_json::to_string(&ev).unwrap();
            self.journal
                .lock()
                .unwrap()
                .push(format!("rt.project {project_id} {json}"));
            self.published
                .lock()
                .unwrap()
                .push(Published::Project(project_id, ev));
            self.outcome()
        })
    }

    fn compute_stopping(&self, branch_id: Uuid) -> BoxFuture<'_, Result<(), NatsError>> {
        Box::pin(async move {
            self.journal
                .lock()
                .unwrap()
                .push(format!("rt.compute_stopping {branch_id}"));
            self.published
                .lock()
                .unwrap()
                .push(Published::Stopping(branch_id));
            self.outcome()
        })
    }
}

/// The worker over the fakes, wrapped as `config::build` wraps the real ports.
pub(crate) type RtWorker = Worker<
    PublishingMetadata<FakeMetadata>,
    FakeStorage,
    StoppingBackend<FakeBackend, FakeMetadata>,
    FakeCache,
    FakePusher,
>;

/// An [`RtWorker`], the fake metadata it wraps (shared state), the journal and the recorder.
pub(crate) struct Rt {
    pub w: RtWorker,
    pub fake: FakeMetadata,
    pub journal: Journal,
    pub publisher: Arc<RecordingPublisher>,
}

pub(crate) fn rt_worker() -> Rt {
    let (w, journal) = worker();
    let publisher = Arc::new(RecordingPublisher::new(journal.clone()));
    let dynamic: Arc<dyn RtPublisher> = publisher.clone();
    let fake = w.metadata.clone();
    let w = Worker {
        metadata: PublishingMetadata::new(w.metadata, dynamic.clone()),
        storage: w.storage,
        computes: StoppingBackend::new(w.computes, fake.clone(), dynamic),
        cache: w.cache,
        pusher: w.pusher,
        key: w.key,
        owner_superuser: w.owner_superuser,
        roles: w.roles,
        control_plane_uri: w.control_plane_uri,
        ready_poll: w.ready_poll,
        ready_timeout: w.ready_timeout,
        attach_timeout: w.attach_timeout,
        ready_cap: w.ready_cap,
        claim_refresh: w.claim_refresh,
        schema_check_cursor: w.schema_check_cursor,
        enabling_seen: w.enabling_seen,
        recreate_backoff: w.recreate_backoff,
        zitadel: w.zitadel,
        app_users_reconcile_every: w.app_users_reconcile_every,
        member_roles_sweep_every: w.member_roles_sweep_every,
        remove_orphans: w.remove_orphans,
        merge_tx_timeout: w.merge_tx_timeout,
        merge_online_step_timeout: w.merge_online_step_timeout,
        resync_requests: w.resync_requests,
        member_role_requests: w.member_role_requests,
        member_logins: w.member_logins,
        cert_sweep: w.cert_sweep,
        realtime_logins: w.realtime_logins,
        storage_token_refresh_every: w.storage_token_refresh_every,
        storage_token_failures: w.storage_token_failures,
        realtime_password: w.realtime_password,
    };
    Rt {
        w,
        fake,
        journal,
        publisher,
    }
}

impl Rt {
    /// A ready database named `app`, provisioned through the wrapped worker.
    pub(crate) async fn ready_database(&self) -> DatabaseId {
        let id = self.fake.insert_database("app", DatabaseStatus::Pending);
        self.w.handle_provision(id, "app").await.unwrap();
        assert_eq!(self.fake.database(id).status, DatabaseStatus::Ready);
        id
    }

    /// A ready branch of `db` under `parent`, built through the wrapped worker.
    pub(crate) async fn ready_branch(
        &self,
        db: DatabaseId,
        name: &str,
        parent: BranchId,
    ) -> BranchId {
        let id = BranchId::new();
        let outcome = self.fake.create_branch(&NewBranch {
            id,
            database_id: db,
            name,
            parent_branch_id: parent,
            branched_at_time: None,
            branched_at_lsn: None,
        });
        assert_eq!(outcome, CreateBranchOutcome::Created);
        self.w.handle_branch_create(id, None).await.unwrap();
        let branch = self.fake.branch(id);
        assert_eq!(branch.status, BranchStatus::Ready, "{:?}", branch.error);
        id
    }

    /// The endpoint of `branch`.
    pub(crate) fn ep(&self, branch: BranchId) -> String {
        self.fake
            .branch(branch)
            .endpoint_id
            .expect("the branch has an endpoint")
    }

    /// Forgets what was journaled and published so far.
    pub(crate) fn clear(&self) {
        self.journal.lock().unwrap().clear();
        self.publisher.published.lock().unwrap().clear();
    }
}

/// Index of the first journal entry equal to `entry` or starting with `entry` + " ".
pub(crate) fn at(journal: &Journal, entry: &str) -> Option<usize> {
    journal
        .lock()
        .unwrap()
        .iter()
        .position(|e| e == entry || e.starts_with(&format!("{entry} ")))
}

/// The status event the wrapper publishes for `branch` of `db`.
pub(crate) fn status_event(db: DatabaseId, branch: BranchId, status: BranchStatus) -> ProjectEvent {
    ProjectEvent::BranchStatus {
        database_id: db.as_uuid(),
        branch_id: branch.as_uuid(),
        status: status.as_str().to_string(),
    }
}
