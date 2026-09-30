use axum::Router;
use cheladb_auth::internal::{
    generate_keypair_pem, AuthMethod, Identity, InternalSigner, InternalVerifier, IDENTITY_HEADER,
};
use cheladb_metadata::{
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, CreateBranchOutcome, DatabaseRecord, DatabaseStatus,
    MetadataError, NewBranch, ProjectOverview, TenantOverview,
};
use cheladb_nats::{
    BranchJob, DatabaseSettingsJob, DeleteDatabaseJob, JobEnvelope, PointInTime,
    ProvisionDatabaseJob, ResetPasswordReply, RESET_NOT_APPLIED,
};
use cheladb_routes::{Service, ENDPOINTS};
use cheladb_types::{BranchId, DatabaseId, ProjectId, TenantId};
use cheladb_secrets::{seal_credential, Purpose, SecretKey};
use orchestrator::{build_router, AppState, JobPort, MetadataPort, ResetError};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct FakeMetadata {
    tenants_by_org: Arc<Mutex<HashMap<String, TenantId>>>,
    fail_insert: Arc<Mutex<bool>>,
    databases: Arc<Mutex<HashMap<DatabaseId, DatabaseRecord>>>,
    branches: Arc<Mutex<HashMap<BranchId, BranchRecord>>>,
    overviews: Arc<Mutex<HashMap<TenantId, TenantOverview>>>,
    /// Calls to `main_branches_with_counts`: /v1/me makes exactly one per request.
    main_count_queries: Arc<AtomicUsize>,
}

/// A database row and its main branch, which holds the endpoint and the sealed password.
/// Derefs to the database row, so tests set its fields directly.
#[derive(Clone)]
struct Db {
    record: DatabaseRecord,
    main: BranchRecord,
}

impl Deref for Db {
    type Target = DatabaseRecord;
    fn deref(&self) -> &DatabaseRecord {
        &self.record
    }
}

impl DerefMut for Db {
    fn deref_mut(&mut self) -> &mut DatabaseRecord {
        &mut self.record
    }
}

/// A database with its default main: endpoint `ep-0123456789ab`, and a sealed password
/// no key opens.
impl From<DatabaseRecord> for Db {
    fn from(record: DatabaseRecord) -> Self {
        let main = main_for(&record);
        Self { record, main }
    }
}

fn main_for(record: &DatabaseRecord) -> BranchRecord {
    let now = chrono::Utc::now();
    BranchRecord {
        id: BranchId::from(record.database_id),
        database_id: record.database_id,
        name: "main".into(),
        parent_branch_id: None,
        kind: BranchKind::Normal,
        status: BranchStatus::from(record.status),
        timeline_id: Some("0123456789abcdef0123456789abcdef".into()),
        pending_timeline_id: None,
        pending_timeline_at: None,
        superseded_timeline_id: None,
        endpoint_id: Some("ep-0123456789ab".into()),
        owner_password_enc: Some("v1:secret-owner-sealed".into()),
        branched_at_time: None,
        branched_at_lsn: None,
        error: None,
        created_at: now,
        updated_at: now,
        history_start: None,
    }
}

impl FakeMetadata {
    fn bind_org(&self, org_id: &str, tenant_id: TenantId) {
        self.tenants_by_org
            .lock()
            .unwrap()
            .insert(org_id.to_string(), tenant_id);
    }

    fn insert_database(&self, db: impl Into<Db>) {
        let Db { record, main } = db.into();
        self.branches.lock().unwrap().insert(main.id, main);
        self.databases
            .lock()
            .unwrap()
            .insert(record.database_id, record);
    }

    /// Adds a branch of `db` under `parent`; a ready normal branch gets an endpoint.
    fn add_branch(
        &self,
        db: DatabaseId,
        name: &str,
        parent: BranchId,
        kind: BranchKind,
        status: BranchStatus,
    ) -> BranchId {
        let now = chrono::Utc::now();
        let id = BranchId::new();
        let has_endpoint = kind == BranchKind::Normal && status == BranchStatus::Ready;
        self.branches.lock().unwrap().insert(
            id,
            BranchRecord {
                id,
                database_id: db,
                name: name.into(),
                parent_branch_id: Some(parent),
                kind,
                status,
                timeline_id: Some("fedcba9876543210fedcba9876543210".into()),
                pending_timeline_id: None,
                pending_timeline_at: None,
                superseded_timeline_id: None,
                endpoint_id: has_endpoint.then(|| format!("ep-{:012x}", id.as_uuid().as_u128() & 0xffff_ffff_ffff)),
                owner_password_enc: None,
                branched_at_time: None,
                branched_at_lsn: None,
                error: None,
                created_at: now,
                updated_at: now,
                history_start: None,
            },
        );
        id
    }

    fn branch(&self, id: BranchId) -> BranchRecord {
        self.branches.lock().unwrap().get(&id).cloned().expect("branch row")
    }

    fn database(&self, id: DatabaseId) -> DatabaseRecord {
        self.databases.lock().unwrap().get(&id).cloned().expect("database row")
    }

    fn fail_next_insert(&self) {
        *self.fail_insert.lock().unwrap() = true;
    }

    /// Databases without a main branch yet get the default one.
    fn set_overview(&self, overview: TenantOverview) {
        let mut branches = self.branches.lock().unwrap();
        for db in overview.projects.iter().flat_map(|p| &p.databases) {
            branches
                .entry(BranchId::from(db.database_id))
                .or_insert_with(|| main_for(db));
        }
        self.overviews.lock().unwrap().insert(overview.id, overview);
    }
}

/// A minimal `Ready` database record for the given tenant, for GET/soft-bind tests
/// that don't care about provisioning details.
fn sample_database(tenant_id: TenantId) -> Db {
    Db::from(DatabaseRecord {
        database_id: DatabaseId::new(),
        project_id: ProjectId::new(),
        tenant_id,
        name: "demo-db".into(),
        status: DatabaseStatus::Ready,
        database_name: None,
        owner_role: None,
        error: None,
        neon_tenant_id: None,
        history_window: std::time::Duration::from_secs(7 * 24 * 3600),
        max_branches: 5,
    })
}

impl MetadataPort for FakeMetadata {
    async fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<TenantId>, MetadataError> {
        Ok(self.tenants_by_org.lock().unwrap().get(zitadel_org_id).copied())
    }

    async fn insert_database_pending(
        &self,
        _database_id: DatabaseId,
        _tenant_id: TenantId,
        _project_id: ProjectId,
        _name: &str,
    ) -> Result<(), MetadataError> {
        if *self.fail_insert.lock().unwrap() {
            return Err(MetadataError::DuplicateDatabaseName);
        }
        Ok(())
    }

    async fn set_failed(&self, _database_id: DatabaseId, _error: &str) -> Result<(), MetadataError> {
        Ok(())
    }

    async fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Option<DatabaseRecord>, MetadataError> {
        Ok(self.databases.lock().unwrap().get(&database_id).cloned())
    }

    async fn tenant_overview(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<TenantOverview>, MetadataError> {
        Ok(self.overviews.lock().unwrap().get(&tenant_id).cloned())
    }

    async fn get_branch(&self, branch_id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        Ok(self.branches.lock().unwrap().get(&branch_id).cloned())
    }

    async fn begin_delete(&self, database_id: DatabaseId) -> Result<bool, MetadataError> {
        let mut dbs = self.databases.lock().unwrap();
        match dbs.get_mut(&database_id) {
            Some(db) if !matches!(db.status, DatabaseStatus::Deleting | DatabaseStatus::Deleted) => {
                db.status = DatabaseStatus::Deleting;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn list_branches(&self, database_id: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError> {
        let mut list: Vec<_> = self
            .branches
            .lock()
            .unwrap()
            .values()
            .filter(|b| b.database_id == database_id && b.status != BranchStatus::Deleted)
            .cloned()
            .collect();
        // Main first, as `created_at` would put it; then by name for a stable order.
        list.sort_by_key(|b| (b.parent_branch_id.is_some(), b.name.clone()));
        Ok(list)
    }

    async fn main_branches_with_counts(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<(BranchRecord, i64)>, MetadataError> {
        self.main_count_queries.fetch_add(1, Ordering::SeqCst);
        let overview = self.overviews.lock().unwrap().get(&tenant_id).cloned();
        let branches = self.branches.lock().unwrap();
        Ok(overview
            .iter()
            .flat_map(|o| o.projects.iter().flat_map(|p| &p.databases))
            .filter_map(|db| {
                let main = branches.get(&BranchId::from(db.database_id))?.clone();
                let count = branches
                    .values()
                    .filter(|b| {
                        b.database_id == db.database_id
                            && b.kind == BranchKind::Normal
                            && b.status != BranchStatus::Deleted
                    })
                    .count();
                Some((main, count as i64))
            })
            .collect())
    }

    async fn create_branch(&self, b: &NewBranch<'_>) -> Result<CreateBranchOutcome, MetadataError> {
        let max = match self.databases.lock().unwrap().get(&b.database_id) {
            Some(db) if !matches!(db.status, DatabaseStatus::Deleting | DatabaseStatus::Deleted) => {
                db.max_branches
            }
            _ => return Ok(CreateBranchOutcome::DatabaseNotLive),
        };
        let mut branches = self.branches.lock().unwrap();
        let parent_ready = branches
            .get(&b.parent_branch_id)
            .is_some_and(|p| p.database_id == b.database_id && p.status == BranchStatus::Ready);
        if !parent_ready {
            return Ok(CreateBranchOutcome::ParentNotReady);
        }
        let live = |x: &&BranchRecord| x.database_id == b.database_id && x.status != BranchStatus::Deleted;
        if branches.values().filter(live).any(|x| x.name == b.name) {
            return Ok(CreateBranchOutcome::NameTaken);
        }
        let count = branches
            .values()
            .filter(live)
            .filter(|x| x.kind == BranchKind::Normal && x.parent_branch_id.is_some())
            .count();
        if count as i64 >= i64::from(max) {
            return Ok(CreateBranchOutcome::CapReached { max });
        }
        let now = chrono::Utc::now();
        branches.insert(
            b.id,
            BranchRecord {
                id: b.id,
                database_id: b.database_id,
                name: b.name.into(),
                parent_branch_id: Some(b.parent_branch_id),
                kind: BranchKind::Normal,
                status: BranchStatus::Pending,
                timeline_id: None,
                pending_timeline_id: None,
                pending_timeline_at: None,
                superseded_timeline_id: None,
                endpoint_id: None,
                owner_password_enc: None,
                branched_at_time: b.branched_at_time,
                branched_at_lsn: b.branched_at_lsn.map(str::to_string),
                error: None,
                created_at: now,
                updated_at: now,
                history_start: None,
            },
        );
        Ok(CreateBranchOutcome::Created)
    }

    async fn live_children(&self, branch_id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        let mut children: Vec<_> = self
            .branches
            .lock()
            .unwrap()
            .values()
            .filter(|b| {
                b.parent_branch_id == Some(branch_id)
                    && b.kind == BranchKind::Normal
                    && b.status != BranchStatus::Deleted
            })
            .cloned()
            .collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(children)
    }

    async fn set_branch_status(
        &self,
        branch_id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        let mut branches = self.branches.lock().unwrap();
        match branches.get_mut(&branch_id) {
            Some(b)
                if from.contains(&b.status)
                    && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) =>
            {
                b.status = to;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn claim_branch_for_switch(&self, branch_id: BranchId) -> Result<bool, MetadataError> {
        match self.branches.lock().unwrap().get_mut(&branch_id) {
            Some(b) if b.status == BranchStatus::Ready => {
                b.status = BranchStatus::Resetting;
                b.error = None;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn set_branch_failed(&self, branch_id: BranchId, error: &str) -> Result<(), MetadataError> {
        if let Some(b) = self.branches.lock().unwrap().get_mut(&branch_id) {
            b.status = BranchStatus::Failed;
            b.error = Some(error.into());
        }
        Ok(())
    }

    async fn begin_branch_delete(&self, branch_id: BranchId) -> Result<BranchDeleteOutcome, MetadataError> {
        let mut branches = self.branches.lock().unwrap();
        // Like the real guard: refused while the branch or a live backup of it has live
        // normal children (the branch's own first, then each backup's, oldest first).
        let live = |b: &BranchRecord| !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted);
        if branches.get(&branch_id).is_some_and(live) {
            let children_of = |parent: BranchId| {
                let mut names: Vec<String> = branches
                    .values()
                    .filter(|c| {
                        c.parent_branch_id == Some(parent)
                            && c.kind == BranchKind::Normal
                            && c.status != BranchStatus::Deleted
                    })
                    .map(|c| c.name.clone())
                    .collect();
                names.sort();
                names
            };
            let own = children_of(branch_id);
            if !own.is_empty() {
                return Ok(BranchDeleteOutcome::HasChildren { backup: None, children: own });
            }
            let mut backups: Vec<&BranchRecord> = branches
                .values()
                .filter(|b| {
                    b.parent_branch_id == Some(branch_id)
                        && b.kind == BranchKind::Backup
                        && b.status != BranchStatus::Deleted
                })
                .collect();
            backups.sort_by_key(|b| (b.created_at, b.id.to_string()));
            for backup in backups {
                let children = children_of(backup.id);
                if !children.is_empty() {
                    return Ok(BranchDeleteOutcome::HasChildren { backup: Some(backup.name.clone()), children });
                }
            }
        }
        let moved = match branches.get_mut(&branch_id) {
            Some(b) if !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) => {
                b.status = BranchStatus::Deleting;
                b.error = None;
                true
            }
            _ => false,
        };
        for b in branches.values_mut() {
            if b.parent_branch_id == Some(branch_id)
                && b.kind == BranchKind::Backup
                && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted)
            {
                b.status = BranchStatus::Deleting;
            }
        }
        Ok(if moved { BranchDeleteOutcome::Started } else { BranchDeleteOutcome::NotMoved })
    }

    async fn update_database_settings(
        &self,
        database_id: DatabaseId,
        history_window: Option<std::time::Duration>,
        max_branches: Option<i32>,
    ) -> Result<bool, MetadataError> {
        let mut dbs = self.databases.lock().unwrap();
        match dbs.get_mut(&database_id) {
            Some(db) if !matches!(db.status, DatabaseStatus::Deleting | DatabaseStatus::Deleted) => {
                if let Some(w) = history_window {
                    db.history_window = w;
                }
                if let Some(m) = max_branches {
                    db.max_branches = m;
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[derive(Clone, Copy, Default)]
enum FakeReset {
    #[default]
    Ok,
    NotReady,
    /// The worker stored the new password but couldn't apply it yet.
    NotApplied,
    Timeout,
    /// The worker gave up on the database (its own deadline).
    WorkerTimedOut,
    /// No worker is listening (NATS no-responders).
    Unavailable,
}

#[derive(Clone, Default)]
struct FakeJobs {
    published: Arc<Mutex<Vec<JobEnvelope<ProvisionDatabaseJob>>>>,
    deleted: Arc<Mutex<Vec<DatabaseId>>>,
    resets: Arc<Mutex<Vec<(DatabaseId, BranchId)>>>,
    reset: Arc<Mutex<FakeReset>>,
    branch_jobs: Arc<Mutex<Vec<JobEnvelope<BranchJob>>>>,
    settings_jobs: Arc<Mutex<Vec<DatabaseId>>>,
    /// Branch jobs can't be published.
    fail_branch_publish: Arc<Mutex<bool>>,
}

impl FakeJobs {
    fn reset_answers(&self, answer: FakeReset) {
        *self.reset.lock().unwrap() = answer;
    }
}

impl JobPort for FakeJobs {
    async fn publish_provision(
        &self,
        job: &JobEnvelope<ProvisionDatabaseJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.published.lock().unwrap().push(job.clone());
        Ok(())
    }

    async fn publish_delete(
        &self,
        job: &JobEnvelope<DeleteDatabaseJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.deleted.lock().unwrap().push(job.payload.database_id);
        Ok(())
    }

    async fn publish_branch(
        &self,
        job: &JobEnvelope<BranchJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if *self.fail_branch_publish.lock().unwrap() {
            return Err("nats is down".into());
        }
        self.branch_jobs.lock().unwrap().push(job.clone());
        Ok(())
    }

    async fn publish_database_settings(
        &self,
        job: &JobEnvelope<DatabaseSettingsJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.settings_jobs.lock().unwrap().push(job.payload.database_id);
        Ok(())
    }

    async fn reset_password(
        &self,
        database_id: DatabaseId,
        branch_id: BranchId,
    ) -> Result<ResetPasswordReply, ResetError> {
        self.resets.lock().unwrap().push((database_id, branch_id));
        let answer = *self.reset.lock().unwrap();
        match answer {
            FakeReset::Ok => Ok(ResetPasswordReply { ok: true, error: None }),
            FakeReset::NotApplied => Ok(ResetPasswordReply { ok: false, error: Some(RESET_NOT_APPLIED.into()) }),
            FakeReset::NotReady => Ok(ResetPasswordReply { ok: false, error: Some("not ready".into()) }),
            FakeReset::Timeout => Err(ResetError::Timeout),
            FakeReset::WorkerTimedOut => Ok(ResetPasswordReply { ok: false, error: Some("timed out".into()) }),
            FakeReset::Unavailable => Err(ResetError::Transport("no worker listening".into())),
        }
    }
}

/// A running orchestrator plus the signer standing in for the gateway.
struct TestApp {
    base: String,
    signer: InternalSigner,
    /// The secrets key the orchestrator decrypts with.
    key: SecretKey,
}

impl TestApp {
    /// An internal identity token for `org_id`, as the gateway would mint it.
    fn token_for_org(&self, org_id: &str) -> String {
        self.signer
            .sign(Service::Orchestrator.name(), &identity(org_id))
            .unwrap()
    }
}

fn identity(org_id: &str) -> Identity {
    Identity {
        sub: "user-1".into(),
        org_id: org_id.into(),
        auth: AuthMethod::PlatformJwt,
        request_id: "test-rid".into(),
    }
}

/// Boots the real orchestrator router (fake metadata/jobs, fresh internal
/// keypair) on an ephemeral port.
async fn spawn_app(metadata: FakeMetadata, jobs: FakeJobs) -> TestApp {
    let (private_pem, public_pem) = generate_keypair_pem();
    let key = SecretKey::generate();
    let state = Arc::new(AppState {
        metadata,
        js: jobs,
        identity: InternalVerifier::from_pem(public_pem.as_bytes()).unwrap(),
        secrets: key.clone(),
        proxy_domain: "localtest.me".into(),
    });
    let app: Router = build_router(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestApp {
        base: format!("http://{addr}"),
        signer: InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
        key,
    }
}

fn create_body(tenant_id: TenantId) -> serde_json::Value {
    serde_json::json!({
        "tenant_id": tenant_id,
        "project_id": ProjectId::new(),
        "name": "demo",
    })
}

async fn post_create(app: &TestApp, identity_header: Option<&str>, body: serde_json::Value) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{}/v1/databases", app.base))
        .json(&body);
    if let Some(token) = identity_header {
        req = req.header(IDENTITY_HEADER, token);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn health_is_public() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::get(format!("{}/health", app.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn missing_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = post_create(&app, None, create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("gateway"));
}

#[tokio::test]
async fn bearer_token_without_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases", app.base))
        .bearer_auth("some-zitadel-jwt")
        .json(&create_body(TenantId::new()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn garbage_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = post_create(&app, Some("not-a-jwt"), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn expired_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let token = app
        .signer
        .sign_at(Service::Orchestrator.name(), &identity("org-1"), now - 3600)
        .unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn identity_minted_for_another_service_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = app.signer.sign("some-other-service", &identity("org-1")).unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn identity_from_untrusted_key_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let (other_private, _) = generate_keypair_pem();
    let forger = InternalSigner::from_pem(other_private.as_bytes()).unwrap();
    let token = forger
        .sign(Service::Orchestrator.name(), &identity("org-1"))
        .unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn unbound_org_is_403_with_bootstrap_hint() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = app.token_for_org("org-not-bound");
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("zitadel-bootstrap"));
}

#[tokio::test]
async fn body_tenant_mismatch_is_403() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn valid_request_is_202_accepted_and_publishes_job() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app(metadata, jobs.clone()).await;
    let token = app.token_for_org("org-1");

    let project_id = ProjectId::new();
    let resp = post_create(
        &app,
        Some(&token),
        serde_json::json!({
            "tenant_id": bound_tenant,
            "project_id": project_id,
            "name": "demo-db",
        }),
    )
    .await;

    assert_eq!(resp.status(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "pending");
    assert!(body["job_id"].is_string());
    assert!(body["database_id"].is_string());

    let published = jobs.published.lock().unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].payload.tenant_id, bound_tenant);
    assert_eq!(published[0].payload.project_id, project_id);
    assert_eq!(published[0].payload.name, "demo-db");
}

#[tokio::test]
async fn empty_name_is_400() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");
    let resp = post_create(
        &app,
        Some(&token),
        serde_json::json!({
            "tenant_id": bound_tenant,
            "project_id": ProjectId::new(),
            "name": "   ",
        }),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

// Review fix: reserved or over-long names would hand the owner a system
// database or fail provisioning with a confusing timeout.
#[tokio::test]
async fn reserved_or_too_long_database_names_are_400() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app(metadata, jobs.clone()).await;
    let token = app.token_for_org("org-1");
    for name in ["postgres".to_string(), "template1".to_string(), "x".repeat(64)] {
        let resp = post_create(
            &app,
            Some(&token),
            serde_json::json!({ "tenant_id": bound_tenant, "project_id": ProjectId::new(), "name": name }),
        )
        .await;
        assert_eq!(resp.status(), 400, "{name}");
    }
    assert!(jobs.published.lock().unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_database_name_is_409() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    metadata.fail_next_insert();
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");
    let resp = post_create(&app, Some(&token), create_body(bound_tenant)).await;
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn get_database_own_tenant_is_200() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let record = sample_database(bound_tenant);
    let database_id = record.database_id;
    metadata.insert_database(record);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, database_id.as_uuid()))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["tenant_id"], serde_json::json!(bound_tenant));
}

#[tokio::test]
async fn get_database_other_tenant_is_403_forbidden() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let record = sample_database(TenantId::new());
    let database_id = record.database_id;
    metadata.insert_database(record);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, database_id.as_uuid()))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "forbidden");
}

#[tokio::test]
async fn get_database_unknown_id_is_404() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, uuid::Uuid::new_v4()))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

fn overview_with_database(tenant_id: TenantId) -> TenantOverview {
    let project_id = ProjectId::new();
    let mut db = sample_database(tenant_id);
    db.project_id = project_id;
    db.name = "web1".into();
    TenantOverview {
        id: tenant_id,
        slug: "demo".into(),
        projects: vec![ProjectOverview {
            id: project_id,
            slug: "default".into(),
            databases: vec![db.record],
        }],
    }
}

async fn get_me(app: &TestApp, identity_header: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new().get(format!("{}/v1/me", app.base));
    if let Some(token) = identity_header {
        req = req.header(IDENTITY_HEADER, token);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn me_for_bound_org_returns_tenant_projects_and_databases() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.set_overview(overview_with_database(tenant));
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let resp = get_me(&app, Some(&app.token_for_org("org-1"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["sub"], "user-1");
    assert_eq!(body["user"]["org_id"], "org-1");
    assert_eq!(body["tenant"]["id"], serde_json::json!(tenant));
    assert_eq!(body["tenant"]["slug"], "demo");
    let project = &body["tenant"]["projects"][0];
    assert_eq!(project["slug"], "default");
    let db = &project["databases"][0];
    assert_eq!(db["name"], "web1");
    assert_eq!(db["status"], "ready");
    assert_eq!(db["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(db["port"], 4432);
    assert!(db["database_id"].is_string());
}

#[tokio::test]
async fn me_for_unbound_org_returns_null_tenant() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = get_me(&app, Some(&app.token_for_org("org-unbound"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["org_id"], "org-unbound");
    assert!(body["tenant"].is_null());
}

// Review Focus 2.
#[tokio::test]
async fn me_for_bound_org_with_missing_tenant_row_returns_null_tenant() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let resp = get_me(&app, Some(&app.token_for_org("org-1"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["tenant"].is_null());
}

#[tokio::test]
async fn me_without_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    assert_eq!(get_me(&app, None).await.status(), 401);
}

#[tokio::test]
async fn me_never_exposes_database_secrets() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.set_overview(overview_with_database(tenant));
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let text = get_me(&app, Some(&app.token_for_org("org-1")))
        .await
        .text()
        .await
        .unwrap();
    for secret in [
        "secret-owner-sealed",
        "owner_password_enc",
        "endpoint_id",
    ] {
        assert!(!text.contains(secret), "/v1/me leaked {secret}: {text}");
    }
}

#[tokio::test]
async fn get_deleted_database_is_404() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let mut record = sample_database(tenant);
    record.status = DatabaseStatus::Deleted;
    let id = record.database_id;
    metadata.insert_database(record);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, id.as_uuid()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// GET /v1/databases/{id} never returns credentials or container ids.
#[tokio::test]
async fn get_database_never_exposes_secrets() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let db = overview_with_database(tenant).projects[0].databases[0].clone();
    let id = db.database_id;
    metadata.insert_database(db);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let text = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, id.as_uuid()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for secret in ["secret-owner-sealed", "password", "endpoint_id"] {
        assert!(!text.contains(secret), "GET leaked {secret}: {text}");
    }
}

// --- Lifecycle: reveal, reset, delete ----------------------------------------

/// A ready database with sealed credentials (`s3cret`) under `app.key`.
fn ready_database(tenant: TenantId, key: &SecretKey) -> Db {
    let mut db = sample_database(tenant);
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    db.main.owner_password_enc =
        Some(seal_credential(key, &db.main.id.to_string(), Purpose::Owner, "s3cret"));
    db
}

async fn call(app: &TestApp, method: reqwest::Method, path: &str, org: &str) -> reqwest::Response {
    reqwest::Client::new()
        .request(method, format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, app.token_for_org(org))
        .send()
        .await
        .unwrap()
}

/// App with org-1 bound to a tenant; `setup` gets the tenant and key to insert rows.
async fn lifecycle_app(jobs: FakeJobs, setup: impl FnOnce(&FakeMetadata, TenantId, &SecretKey)) -> TestApp {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app(metadata.clone(), jobs).await;
    setup(&metadata, tenant, &app.key);
    app
}

#[tokio::test]
async fn reveal_ready_database_returns_decrypted_owner_credentials() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"], "postgres");
    assert_eq!(body["password"], "s3cret");
    assert_eq!(body["database"], "web1");
    assert_eq!(body["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(body["port"], 4432);
    assert_eq!(
        body["uri"],
        "postgresql://postgres:s3cret@ep-0123456789ab.localtest.me:4432/web1?sslmode=verify-full"
    );
    assert_eq!(body["http_url"], "https://ep-0123456789ab.localtest.me:4444/sql");
}

// Review fix: a replaced secrets key gets an explicit message, not a bare 500.
#[tokio::test]
async fn reveal_with_a_different_secrets_key_explains_the_key_mismatch() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, _k| {
        let db = ready_database(t, &SecretKey::generate());
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 500);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("secrets key"), "{body}");
}

#[tokio::test]
async fn reveal_without_an_endpoint_is_409() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.endpoint_id = None;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("recreate"), "{body}");
}

// /v1/me and GET show the proxy host and port when an endpoint is linked, nothing otherwise.
#[tokio::test]
async fn summaries_show_the_proxy_host() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let overview = overview_with_database(tenant);
    let mut bare = sample_database(tenant);
    bare.main.endpoint_id = None;
    bare.project_id = overview.projects[0].id;
    let (linked_id, bare_id) = (overview.projects[0].databases[0].database_id, bare.database_id);
    metadata.insert_database(overview.projects[0].databases[0].clone());
    metadata.insert_database(bare.clone());
    let mut overview = overview;
    overview.projects[0].databases.push(bare.record);
    metadata.set_overview(overview);
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let me: serde_json::Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let dbs = &me["tenant"]["projects"][0]["databases"];
    assert_eq!(dbs[0]["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(dbs[0]["port"], 4432);
    assert!(dbs[1]["host"].is_null() && dbs[1]["port"].is_null());

    for (id, host, port) in [(linked_id, serde_json::json!("ep-0123456789ab.localtest.me"), serde_json::json!(4432)), (bare_id, serde_json::Value::Null, serde_json::Value::Null)] {
        let body: serde_json::Value = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
        assert_eq!(body["host"], host);
        assert_eq!(body["port"], port);
    }
}

// A linked endpoint isn't reachable until the database is ready: no host or port before.
#[tokio::test]
async fn summaries_hide_the_proxy_host_until_ready() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let mut overview = overview_with_database(tenant);
    let db = &mut overview.projects[0].databases[0];
    db.status = DatabaseStatus::Provisioning;
    assert!(main_for(db).endpoint_id.is_some());
    let id = db.database_id;
    metadata.insert_database(db.clone());
    metadata.set_overview(overview);
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let me: serde_json::Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let summary = &me["tenant"]["projects"][0]["databases"][0];
    assert_eq!(summary["status"], "provisioning");
    assert!(summary["host"].is_null() && summary["port"].is_null(), "{summary}");

    let body: serde_json::Value = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
    assert_eq!(body["status"], "provisioning");
    assert!(body["host"].is_null() && body["port"].is_null(), "{body}");
}

// Main's branch row holds the database's endpoint and sealed password now: reveal reads both
// from it, and the password opens with main's id (the database's) as AAD.
#[tokio::test]
async fn reveal_uses_main_branch_row() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.endpoint_id = Some("ep-aaaaaaaaaaaa".into());
        db.main.owner_password_enc = Some(seal_credential(
            k,
            &BranchId::from(db.database_id).to_string(),
            Purpose::Owner,
            "from-main",
        ));
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let id = id.unwrap().as_uuid();
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{id}/connection"), "org-1").await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["password"], "from-main");
    assert_eq!(body["host"], "ep-aaaaaaaaaaaa.localtest.me");
    assert_eq!(
        body["uri"],
        "postgresql://postgres:from-main@ep-aaaaaaaaaaaa.localtest.me:4432/web1?sslmode=verify-full"
    );
    // GET shows main's host too.
    let body: serde_json::Value = call(&app, reqwest::Method::GET, &format!("/v1/databases/{id}"), "org-1").await.json().await.unwrap();
    assert_eq!(body["host"], "ep-aaaaaaaaaaaa.localtest.me");
}

// No main branch row at all: nothing to reveal or reset.
#[tokio::test]
async fn reveal_and_reset_without_a_main_branch_are_409() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
        m.branches.lock().unwrap().clear();
    })
    .await;
    let id = id.unwrap().as_uuid();
    for (method, path) in [
        (reqwest::Method::GET, format!("/v1/databases/{id}/connection")),
        (reqwest::Method::POST, format!("/v1/databases/{id}/reset-password")),
    ] {
        let resp = call(&app, method, &path, "org-1").await;
        assert_eq!(resp.status(), 409, "{path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("recreate"), "{body}");
    }
    assert!(jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reveal_not_ready_is_409() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Provisioning;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
}

// Review Focus 2.
#[tokio::test]
async fn reveal_without_stored_credentials_is_409_with_recreate_hint() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.owner_password_enc = None;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("recreate"), "{body}");
}

#[tokio::test]
async fn reveal_other_tenant_is_403() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn reveal_deleted_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Deleted;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn reset_password_ok_is_200() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(*jobs.resets.lock().unwrap(), vec![(id.unwrap(), BranchId::from(id.unwrap()))]);
}

#[tokio::test]
async fn reset_password_not_ready_is_409_without_asking_the_worker() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Pending;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    assert!(jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reset_password_worker_says_not_ready_is_409() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::NotReady);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
}

// Review Focus 5.
#[tokio::test]
async fn reset_password_worker_timeout_is_504() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::Timeout);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 504);
}

#[tokio::test]
async fn reset_password_worker_deadline_is_504() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::WorkerTimedOut);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 504);
}

#[tokio::test]
async fn reset_password_with_no_worker_is_503() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::Unavailable);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 503);
}

// Databases with no endpoint (never provisioned on Neon) can't be reset.
#[tokio::test]
async fn reset_without_an_endpoint_is_409() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.endpoint_id = None;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("recreate"), "{body}");
    assert!(jobs.resets.lock().unwrap().is_empty(), "worker must not be asked");
}

#[tokio::test]
async fn reset_not_applied_is_503_with_the_message() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::NotApplied);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], RESET_NOT_APPLIED);
}

#[tokio::test]
async fn delete_ready_database_is_202_and_publishes_job() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::DELETE, &format!("/v1/databases/{}", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
    assert_eq!(*jobs.deleted.lock().unwrap(), vec![id.unwrap()]);
}

#[tokio::test]
async fn delete_twice_is_409() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let path = format!("/v1/databases/{}", id.unwrap().as_uuid());
    assert_eq!(call(&app, reqwest::Method::DELETE, &path, "org-1").await.status(), 202);
    assert_eq!(call(&app, reqwest::Method::DELETE, &path, "org-1").await.status(), 409);
}

#[tokio::test]
async fn delete_deleted_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Deleted;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::DELETE, &format!("/v1/databases/{}", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn delete_other_tenant_is_403() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::DELETE, &format!("/v1/databases/{}", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 403);
    assert!(jobs.deleted.lock().unwrap().is_empty());
}

/// Every endpoint `cheladb-routes` assigns to the orchestrator must be routed
/// by the orchestrator's own router. A 401 still proves the route exists; only
/// 404 (unknown path) and 405 (wrong method) mean drift.
#[tokio::test]
async fn every_declared_orchestrator_endpoint_is_routed() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let client = reqwest::Client::new();
    let declared: Vec<_> = ENDPOINTS
        .iter()
        .filter(|ep| ep.service == Service::Orchestrator)
        .collect();
    assert!(!declared.is_empty());

    for ep in declared {
        let method = reqwest::Method::from_bytes(ep.method.as_str().as_bytes()).unwrap();
        let url = format!("{}{}", app.base, fill_params(ep.path));
        let status = client.request(method, &url).send().await.unwrap().status();
        assert!(
            status != 404 && status != 405,
            "{} {} is declared in cheladb-routes but the orchestrator answered {status}",
            ep.method.as_str(),
            ep.path
        );
    }
}

fn fill_params(template: &str) -> String {
    template
        .split('/')
        .map(|segment| {
            if segment.starts_with('{') {
                uuid::Uuid::nil().to_string()
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

// --- Branches ------------------------------------------------------------------

use reqwest::Method;
use serde_json::{json, Value};

struct Branching {
    app: TestApp,
    metadata: FakeMetadata,
    jobs: FakeJobs,
    db: DatabaseId,
}

impl Branching {
    fn main(&self) -> BranchId {
        BranchId::from(self.db)
    }

    fn add(&self, name: &str, parent: BranchId, kind: BranchKind, status: BranchStatus) -> BranchId {
        self.metadata.add_branch(self.db, name, parent, kind, status)
    }

    fn ready(&self, name: &str) -> BranchId {
        self.add(name, self.main(), BranchKind::Normal, BranchStatus::Ready)
    }

    fn backup(&self, of: BranchId) -> BranchId {
        self.add("dev-before-restore-20260101T000000Z", of, BranchKind::Backup, BranchStatus::Ready)
    }

    fn branches(&self) -> String {
        format!("/v1/databases/{}/branches", self.db.as_uuid())
    }

    fn branch(&self, id: BranchId) -> String {
        format!("{}/{}", self.branches(), id.as_uuid())
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .request(method, format!("{}{path}", self.app.base))
            .header(IDENTITY_HEADER, self.app.token_for_org("org-1"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    fn published(&self) -> Vec<BranchJob> {
        self.jobs.branch_jobs.lock().unwrap().iter().map(|j| j.payload.clone()).collect()
    }
}

/// A ready database `web1` for org-1 (max 5 branches, 7 day window) with sealed password `s3cret`.
async fn branching() -> Branching {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app(metadata.clone(), jobs.clone()).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    Branching { app, metadata, jobs, db: id }
}

async fn error_of(resp: reqwest::Response, status: u16) -> String {
    assert_eq!(resp.status(), status);
    let body: Value = resp.json().await.unwrap();
    body["error"].as_str().unwrap_or_else(|| panic!("no error in {body}")).to_string()
}

// ---- list

#[tokio::test]
async fn list_branches_returns_the_tree_with_hosts_only_for_ready_normal_branches() {
    let t = branching().await;
    let dev = t.ready("dev");
    let pending = t.add("wip", dev, BranchKind::Normal, BranchStatus::Pending);
    let backup = t.backup(dev);
    let resp = t.send(Method::GET, &t.branches(), None).await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 4);
    let by = |id: BranchId| list.iter().find(|b| b["id"] == json!(id)).unwrap_or_else(|| panic!("{id} missing"));
    let main = by(t.main());
    assert_eq!(main["name"], "main");
    assert!(main["parent_branch_id"].is_null());
    assert_eq!(main["kind"], "normal");
    assert_eq!(main["status"], "ready");
    assert_eq!(main["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(main["port"], 4432);
    let dev_json = by(dev);
    assert_eq!(dev_json["parent_branch_id"], json!(t.main()));
    assert!(dev_json["host"].as_str().unwrap().ends_with(".localtest.me"));
    assert_eq!(dev_json["port"], 4432);
    for id in [pending, backup] {
        assert!(by(id)["host"].is_null() && by(id)["port"].is_null(), "{}", by(id));
    }
    assert_eq!(by(backup)["kind"], "backup");
    assert_eq!(by(pending)["status"], "pending");
    for key in ["id", "name", "parent_branch_id", "kind", "status", "host", "port", "branched_at_time", "branched_at_lsn", "created_at", "error"] {
        assert!(main.get(key).is_some(), "missing {key}");
    }
    let text = serde_json::to_string(&list).unwrap();
    for secret in ["secret-owner-sealed", "owner_password", "endpoint_id", "timeline"] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
}

#[tokio::test]
async fn list_branches_carries_where_each_history_begins() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let start: chrono::DateTime<chrono::Utc> = "2026-09-20T09:00:00Z".parse().unwrap();
    for id in [dev, backup] {
        t.metadata.branches.lock().unwrap().get_mut(&id).unwrap().history_start = Some(start);
    }
    let resp = t.send(Method::GET, &t.branches(), None).await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    let by = |id: BranchId| list.iter().find(|b| b["id"] == json!(id)).unwrap();
    assert_eq!(by(dev)["history_start"], "2026-09-20T09:00:00Z");
    assert_eq!(by(backup)["history_start"], "2026-09-20T09:00:00Z");
    // Unknown (a row from before it was recorded): null, never missing.
    let main = by(t.main());
    assert!(main.get("history_start").is_some_and(Value::is_null), "{main}");
    assert!(!serde_json::to_string(&list).unwrap().contains("pending_timeline_at"));
}

#[tokio::test]
async fn list_branches_hides_deleted_and_refuses_strangers() {
    let t = branching().await;
    t.add("gone", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    assert_eq!(list.len(), 1);
    let unknown = format!("/v1/databases/{}/branches", uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::GET, &unknown, None).await.status(), 404);

    let mut other_id = None;
    let other = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        other_id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let path = format!("/v1/databases/{}/branches", other_id.unwrap().as_uuid());
    assert_eq!(call(&other, Method::GET, &path, "org-1").await.status(), 403);
}


// ---- create

#[tokio::test]
async fn create_branch_is_202_with_the_pending_branch_and_a_job() {
    let t = branching().await;
    let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "dev");
    assert_eq!(body["status"], "pending");
    assert_eq!(body["kind"], "normal");
    assert_eq!(body["parent_branch_id"], json!(t.main()));
    assert!(body["host"].is_null());
    let id = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(t.metadata.branch(id).status, BranchStatus::Pending);
    assert_eq!(t.published(), vec![BranchJob::Create { branch_id: id, at: None }]);
}

#[tokio::test]
async fn create_branch_takes_a_parent_and_a_point() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t
        .send(Method::POST, &t.branches(), Some(json!({ "name": "feat", "parent_branch_id": dev, "at": { "lsn": "0/16B3748" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let id = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(body["parent_branch_id"], json!(dev));
    assert_eq!(body["branched_at_lsn"], "0/16B3748");
    assert_eq!(t.published(), vec![BranchJob::Create { branch_id: id, at: Some(PointInTime::Lsn("0/16B3748".into())) }]);

    let resp = t
        .send(Method::POST, &t.branches(), Some(json!({ "name": "old", "at": { "timestamp": "2026-09-30T12:00:00Z" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let at: chrono::DateTime<chrono::Utc> = "2026-09-30T12:00:00Z".parse().unwrap();
    assert_eq!(t.metadata.branch(BranchId(body["id"].as_str().unwrap().parse().unwrap())).branched_at_time, Some(at));
    assert!(matches!(t.published().last(), Some(BranchJob::Create { at: Some(PointInTime::Timestamp(x)), .. }) if *x == at));
}

#[tokio::test]
async fn create_branch_from_a_restore_backup_is_202() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let resp = t
        .send(Method::POST, &t.branches(), Some(json!({ "name": "from-backup", "parent_branch_id": backup })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let id = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(body["parent_branch_id"], json!(backup));
    assert_eq!(body["kind"], "normal");
    assert_eq!(t.metadata.branch(id).parent_branch_id, Some(backup));
    assert_eq!(t.published(), vec![BranchJob::Create { branch_id: id, at: None }]);
}

#[tokio::test]
async fn create_branch_with_a_taken_name_is_409() {
    let t = branching().await;
    t.ready("dev");
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await, 409).await;
    assert!(err.contains("dev"), "{err}");
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "main" }))).await, 422).await;
    assert!(err.contains("main"), "{err}");
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn create_branch_at_the_cap_is_409_naming_the_cap_and_the_patch() {
    let t = branching().await;
    t.metadata.databases.lock().unwrap().get_mut(&t.db).unwrap().max_branches = 1;
    t.ready("dev");
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "b" }))).await, 409).await;
    assert!(err.contains('1') && err.contains("max_branches") && err.contains("PATCH"), "{err}");
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn create_branch_needs_a_ready_parent_that_exists() {
    let t = branching().await;
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Pending);
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x", "parent_branch_id": pending }))).await, 409).await;
    assert!(err.contains("not ready"), "{err}");
    let unknown = BranchId::new();
    assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x", "parent_branch_id": unknown }))).await.status(), 404);
    let deleted = t.add("d", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x", "parent_branch_id": deleted }))).await.status(), 404);
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn create_branch_on_a_database_that_is_not_ready_is_409() {
    let t = branching().await;
    t.metadata.branches.lock().unwrap().get_mut(&t.main()).unwrap().status = BranchStatus::Provisioning;
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x" }))).await, 409).await;
    assert!(err.contains("not ready"), "{err}");
}

#[tokio::test]
async fn create_branch_rejects_bad_input_with_422() {
    let t = branching().await;
    let long = "a".repeat(64);
    for name in ["", "Dev", "-x", "_x", "a b", "a.b", long.as_str(), "main"] {
        let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": name }))).await;
        assert_eq!(resp.status(), 422, "name {name:?}");
    }
    for at in [
        json!({ "lsn": "zzz" }),
        json!({ "lsn": "0/" }),
        json!({ "lsn": "1234" }),
        json!({ "lsn": "" }),
        json!({ "timestamp": "yesterday" }),
        json!({ "timestamp": "2026-09-30" }),
        json!({}),
        json!({ "lsn": "0/1", "timestamp": "2026-09-30T12:00:00Z" }),
    ] {
        let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": "ok", "at": at }))).await;
        assert_eq!(resp.status(), 422, "at {at}");
    }
    // Longest allowed name and the character set.
    for name in ["a".repeat(63), "0-a_b".to_string()] {
        assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": name }))).await.status(), 202, "{name}");
    }
    assert_eq!(t.published().len(), 2);
}

#[tokio::test]
async fn create_branch_publish_failure_is_502_and_fails_the_branch() {
    let t = branching().await;
    *t.jobs.fail_branch_publish.lock().unwrap() = true;
    assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await.status(), 502);
    let branches = t.metadata.branches.lock().unwrap();
    let dev = branches.values().find(|b| b.name == "dev").unwrap();
    assert_eq!(dev.status, BranchStatus::Failed);
    assert!(dev.error.as_deref().unwrap().contains("publish"));
}

#[tokio::test]
async fn create_branch_on_another_tenants_database_is_403_and_unknown_is_404() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let path = format!("/v1/databases/{}/branches", id.unwrap().as_uuid());
    let resp = reqwest::Client::new()
        .post(format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .json(&json!({ "name": "dev" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases/{}/branches", app.base, uuid::Uuid::new_v4()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .json(&json!({ "name": "dev" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(jobs.branch_jobs.lock().unwrap().is_empty());
}

// ---- connection and reset-password

#[tokio::test]
async fn branch_connection_reveals_the_branchs_own_password() {
    let t = branching().await;
    let dev = t.ready("dev");
    let sealed = seal_credential(&t.app.key, &dev.to_string(), Purpose::Owner, "dev-pw");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().owner_password_enc = Some(sealed);
    let ep = t.metadata.branch(dev).endpoint_id.unwrap();
    let resp = t.send(Method::GET, &format!("{}/connection", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["password"], "dev-pw");
    assert_eq!(body["host"], format!("{ep}.localtest.me"));
    assert_eq!(body["port"], 4432);
    assert_eq!(body["database"], "web1");
    assert_eq!(body["user"], "postgres");
    assert_eq!(body["uri"], format!("postgresql://postgres:dev-pw@{ep}.localtest.me:4432/web1?sslmode=verify-full"));
    assert_eq!(body["http_url"], format!("https://{ep}.localtest.me:4444/sql"));
}

#[tokio::test]
async fn branch_connection_and_reset_refuse_backups_unready_and_unknown_branches() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Pending);
    let resetting = t.add("r", t.main(), BranchKind::Normal, BranchStatus::Resetting);
    let gone = t.add("g", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    for (method, suffix) in [(Method::GET, "connection"), (Method::POST, "reset-password")] {
        let err = error_of(t.send(method.clone(), &format!("{}/{suffix}", t.branch(backup)), None).await, 409).await;
        assert!(err.contains("restore backup") && err.contains("no compute"), "{err}");
        for id in [pending, resetting] {
            let err = error_of(t.send(method.clone(), &format!("{}/{suffix}", t.branch(id)), None).await, 409).await;
            assert!(err.contains("not ready") || err.contains("ready"), "{err}");
        }
        for id in [BranchId::new(), gone] {
            assert_eq!(t.send(method.clone(), &format!("{}/{suffix}", t.branch(id)), None).await.status(), 404);
        }
    }
    assert!(t.jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_branch_of_another_database_is_404_under_this_one() {
    let t = branching().await;
    let other = DatabaseId::new();
    let foreign = t.metadata.add_branch(other, "x", BranchId::from(other), BranchKind::Normal, BranchStatus::Ready);
    for (method, suffix) in [
        (Method::GET, "/connection"),
        (Method::POST, "/reset-password"),
        (Method::POST, "/reset-to-parent"),
        (Method::POST, "/restore"),
        (Method::DELETE, ""),
    ] {
        let resp = t.send(method, &format!("{}{suffix}", t.branch(foreign)), Some(json!({ "at": { "lsn": "0/1" } }))).await;
        assert_eq!(resp.status(), 404, "{suffix}");
    }
}

#[tokio::test]
async fn branch_reset_password_asks_the_worker_for_that_branch() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(*t.jobs.resets.lock().unwrap(), vec![(t.db, dev)]);
    t.jobs.reset_answers(FakeReset::NotApplied);
    let resp = t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await;
    assert_eq!(error_of(resp, 503).await, RESET_NOT_APPLIED);
    t.jobs.reset_answers(FakeReset::Timeout);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await.status(), 504);
    t.jobs.reset_answers(FakeReset::Unavailable);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await.status(), 503);
}

#[tokio::test]
async fn old_routes_are_aliases_for_main() {
    let t = branching().await;
    let main = t.main();
    let old: Value = t.send(Method::GET, &format!("/v1/databases/{}/connection", t.db.as_uuid()), None).await.json().await.unwrap();
    let new: Value = t.send(Method::GET, &format!("{}/connection", t.branch(main)), None).await.json().await.unwrap();
    assert_eq!(old, new);
    assert_eq!(old["password"], "s3cret");
    assert_eq!(t.send(Method::POST, &format!("/v1/databases/{}/reset-password", t.db.as_uuid()), None).await.status(), 200);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-password", t.branch(main)), None).await.status(), 200);
    assert_eq!(*t.jobs.resets.lock().unwrap(), vec![(t.db, main), (t.db, main)]);
}

// ---- reset to parent

#[tokio::test]
async fn reset_to_parent_is_202_and_publishes_the_job() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "resetting");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Resetting);
    assert_eq!(t.published(), vec![BranchJob::ResetToParent { branch_id: dev }]);
    // Twice: no longer ready.
    let err = error_of(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await, 409).await;
    assert!(err.contains("not ready"), "{err}");
    assert_eq!(t.published().len(), 1);
}

#[tokio::test]
async fn reset_and_restore_claims_clear_the_last_error() {
    let t = branching().await;
    let dev = t.ready("dev");
    let set_error = |id| t.metadata.branches.lock().unwrap().get_mut(&id).unwrap().error = Some("an old failure".into());
    set_error(dev);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 202);
    assert_eq!(t.metadata.branch(dev).error, None, "each attempt starts clean");

    let qa = t.ready("qa");
    set_error(qa);
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(qa)), Some(json!({ "at": { "lsn": "0/16B3748" } })))
        .await;
    assert_eq!(resp.status(), 202);
    assert_eq!(t.metadata.branch(qa).error, None);
}

#[tokio::test]
async fn reset_to_parent_refusals() {
    let t = branching().await;
    let dev = t.ready("dev");
    let child = t.add("child", dev, BranchKind::Normal, BranchStatus::Ready);
    let other_child = t.add("child2", dev, BranchKind::Normal, BranchStatus::Pending);
    let backup = t.backup(child);
    let path = |id| format!("{}/reset-to-parent", t.branch(id));

    let err = error_of(t.send(Method::POST, &path(t.main()), None).await, 409).await;
    assert!(err.contains("main"), "{err}");
    let err = error_of(t.send(Method::POST, &path(backup), None).await, 409).await;
    assert!(err.contains("restore backup"), "{err}");
    let err = error_of(t.send(Method::POST, &path(other_child), None).await, 409).await;
    assert!(err.contains("not ready"), "{err}");
    let err = error_of(t.send(Method::POST, &path(dev), None).await, 409).await;
    assert!(err.contains("child") && err.contains("child2"), "{err}");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready, "a refused reset leaves the branch ready");
    assert_eq!(t.send(Method::POST, &path(BranchId::new()), None).await.status(), 404);
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn backups_do_not_block_reset_to_parent() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.backup(dev);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 202);
}

#[tokio::test]
async fn reset_to_parent_publish_failure_is_502_and_the_branch_stays_ready() {
    let t = branching().await;
    let dev = t.ready("dev");
    *t.jobs.fail_branch_publish.lock().unwrap() = true;
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 502);
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
}

// ---- restore

#[tokio::test]
async fn restore_returns_the_backup_name() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "lsn": "0/16B3748" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let name = body["backup_name"].as_str().unwrap();
    let (prefix, stamp) = name.split_once("-before-restore-").expect(name);
    assert_eq!(prefix, "dev");
    assert!(stamp.len() == 16 && stamp.ends_with('Z') && stamp.as_bytes()[8] == b'T', "{stamp}");
    assert!(stamp[..8].bytes().all(|b| b.is_ascii_digit()) && stamp[9..15].bytes().all(|b| b.is_ascii_digit()), "{stamp}");
    let backup_id = BranchId(body["backup_branch_id"].as_str().unwrap().parse().unwrap());
    assert_eq!(body["status"], "resetting");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Resetting);
    assert_eq!(
        t.published(),
        vec![BranchJob::Restore {
            branch_id: dev,
            backup_id,
            backup_name: name.to_string(),
            at: PointInTime::Lsn("0/16B3748".into()),
        }]
    );
}

#[tokio::test]
async fn restore_truncates_a_long_name_to_fit_63_characters() {
    let t = branching().await;
    let long = format!("a{}", "b".repeat(62));
    assert_eq!(long.len(), 63);
    let dev = t.ready(&long);
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "timestamp": "2026-09-30T12:00:00Z" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let name = body["backup_name"].as_str().unwrap();
    assert_eq!(name.len(), 63, "{name}");
    assert!(name.starts_with("abbb") && name.contains("-before-restore-2"), "{name}");
}

#[tokio::test]
async fn restore_works_on_main() {
    let t = branching().await;
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(t.main())), Some(json!({ "at": { "lsn": "0/1" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert!(body["backup_name"].as_str().unwrap().starts_with("main-before-restore-"));
}

#[tokio::test]
async fn restore_refusals() {
    let t = branching().await;
    let dev = t.ready("dev");
    let child = t.add("child", dev, BranchKind::Normal, BranchStatus::Ready);
    let backup = t.backup(child);
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Provisioning);
    let path = |id| format!("{}/restore", t.branch(id));
    let at = || Some(json!({ "at": { "lsn": "0/1" } }));

    let err = error_of(t.send(Method::POST, &path(backup), at()).await, 409).await;
    assert!(err.contains("restore backup"), "{err}");
    let err = error_of(t.send(Method::POST, &path(pending), at()).await, 409).await;
    assert!(err.contains("not ready"), "{err}");
    let err = error_of(t.send(Method::POST, &path(dev), at()).await, 409).await;
    assert!(err.contains("child"), "{err}");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
    assert_eq!(t.send(Method::POST, &path(BranchId::new()), at()).await.status(), 404);
    // Bad bodies: 422 before anything changes.
    for body in [
        None,
        Some(json!({})),
        Some(json!({ "at": {} })),
        Some(json!({ "at": { "lsn": "nope" } })),
        Some(json!({ "at": { "timestamp": "nope" } })),
    ] {
        assert_eq!(t.send(Method::POST, &path(child), body.clone()).await.status(), 422, "{body:?}");
    }
    assert_eq!(t.metadata.branch(child).status, BranchStatus::Ready);
    assert!(t.published().is_empty());
}

// ---- delete

#[tokio::test]
async fn delete_branch_is_202_and_takes_its_backups_along() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let resp = t.send(Method::DELETE, &t.branch(dev), None).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Deleting);
    assert_eq!(t.metadata.branch(backup).status, BranchStatus::Deleting);
    assert_eq!(t.published(), vec![BranchJob::Delete { branch_id: dev }]);
    // Twice.
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert!(err.contains("already"), "{err}");
}

#[tokio::test]
async fn delete_branch_refusals() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.add("kid", dev, BranchKind::Normal, BranchStatus::Provisioning);
    t.add("kid2", dev, BranchKind::Normal, BranchStatus::Failed);
    let err = error_of(t.send(Method::DELETE, &t.branch(t.main()), None).await, 409).await;
    assert!(err.contains("main"), "{err}");
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert!(err.contains("kid") && err.contains("kid2"), "{err}");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready, "a refused delete leaves the branch ready");
    let gone = t.add("g", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    assert_eq!(t.send(Method::DELETE, &t.branch(gone), None).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &t.branch(BranchId::new()), None).await.status(), 404);
    assert!(t.published().is_empty());
}

// Fix round 1, I1: a backup can be a parent. Deleting its branch would take the backup
// along, and Neon can't delete a timeline that others branch from.
#[tokio::test]
async fn delete_branch_refuses_while_a_backup_has_children() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let backup_name = t.metadata.branch(backup).name;
    t.add("x", backup, BranchKind::Normal, BranchStatus::Ready);
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert_eq!(
        err,
        format!("cannot delete dev: its backup {backup_name} has child branches (x); delete them first")
    );
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready, "released");
    assert_eq!(t.metadata.branch(backup).status, BranchStatus::Ready);
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn delete_of_a_backup_or_a_branch_with_only_backups_is_allowed() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    assert_eq!(t.send(Method::DELETE, &t.branch(backup), None).await.status(), 202);
    let other = t.ready("other");
    t.backup(other);
    assert_eq!(t.send(Method::DELETE, &t.branch(other), None).await.status(), 202);
}

#[tokio::test]
async fn delete_of_a_failed_branch_is_allowed() {
    let t = branching().await;
    let bad = t.add("bad", t.main(), BranchKind::Normal, BranchStatus::Failed);
    assert_eq!(t.send(Method::DELETE, &t.branch(bad), None).await.status(), 202);
}

// ---- settings

fn database_path(t: &Branching) -> String {
    format!("/v1/databases/{}", t.db.as_uuid())
}

#[tokio::test]
async fn patch_updates_the_settings_and_returns_them() {
    let t = branching().await;
    let resp = t
        .send(Method::PATCH, &database_path(&t), Some(json!({ "history_window": 86400, "max_branches": 10 })))
        .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, json!({ "history_window": 86400, "max_branches": 10 }));
    let db = t.metadata.database(t.db);
    assert_eq!(db.history_window, std::time::Duration::from_secs(86400));
    assert_eq!(db.max_branches, 10);
    // GET shows the settings too.
    let get: Value = t.send(Method::GET, &database_path(&t), None).await.json().await.unwrap();
    assert_eq!(get["history_window"], 86400);
    assert_eq!(get["max_branches"], 10);
}

#[tokio::test]
async fn patch_publishes_settings_only_when_the_window_changed() {
    let t = branching().await;
    let path = database_path(&t);
    let published = || t.jobs.settings_jobs.lock().unwrap().clone();
    // Only the cap.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "max_branches": 3 }))).await.status(), 200);
    assert!(published().is_empty());
    // The same window as now (7 days).
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "history_window": 7 * 24 * 3600 }))).await.status(), 200);
    assert!(published().is_empty());
    // Nothing at all.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({}))).await.status(), 200);
    assert!(published().is_empty());
    // A different window.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "history_window": 3600 }))).await.status(), 200);
    assert_eq!(published(), vec![t.db]);
}

#[tokio::test]
async fn patch_validates_the_ranges() {
    let t = branching().await;
    let path = database_path(&t);
    for body in [
        json!({ "history_window": 3599 }),
        json!({ "history_window": 30 * 24 * 3600 + 1 }),
        json!({ "history_window": 0 }),
        json!({ "history_window": -5 }),
        json!({ "history_window": "7d" }),
        json!({ "max_branches": -1 }),
        json!({ "max_branches": 21 }),
        json!({ "max_branches": "many" }),
        json!({ "surprise": 1 }),
    ] {
        let resp = t.send(Method::PATCH, &path, Some(body.clone())).await;
        assert_eq!(resp.status(), 422, "{body}");
    }
    assert_eq!(t.metadata.database(t.db).max_branches, 5);
    assert!(t.jobs.settings_jobs.lock().unwrap().is_empty());
    for body in [
        json!({ "history_window": 3600, "max_branches": 0 }),
        json!({ "history_window": 30 * 24 * 3600, "max_branches": 20 }),
    ] {
        assert_eq!(t.send(Method::PATCH, &path, Some(body.clone())).await.status(), 200, "{body}");
    }
}

#[tokio::test]
async fn patch_refusals() {
    let t = branching().await;
    let unknown = format!("/v1/databases/{}", uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::PATCH, &unknown, Some(json!({ "max_branches": 1 }))).await.status(), 404);
    t.metadata.databases.lock().unwrap().get_mut(&t.db).unwrap().status = DatabaseStatus::Deleting;
    let err = error_of(t.send(Method::PATCH, &database_path(&t), Some(json!({ "max_branches": 1 }))).await, 409).await;
    assert!(err.contains("deleting") || err.contains("deleted"), "{err}");
    assert!(t.jobs.settings_jobs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn patch_on_another_tenants_database_is_403() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = reqwest::Client::new()
        .patch(format!("{}/v1/databases/{}", app.base, id.unwrap().as_uuid()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .json(&json!({ "max_branches": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

// ---- branch_count

#[tokio::test]
async fn me_and_get_database_show_branch_count_with_one_lookup_for_me() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let overview = overview_with_database(tenant);
    let id = overview.projects[0].databases[0].database_id;
    metadata.insert_database(overview.projects[0].databases[0].clone());
    metadata.set_overview(overview);
    let dev = metadata.add_branch(id, "dev", BranchId::from(id), BranchKind::Normal, BranchStatus::Ready);
    metadata.add_branch(id, "backup", dev, BranchKind::Backup, BranchStatus::Ready);
    metadata.add_branch(id, "gone", BranchId::from(id), BranchKind::Normal, BranchStatus::Deleted);
    let app = spawn_app(metadata.clone(), FakeJobs::default()).await;

    let me: Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let db = &me["tenant"]["projects"][0]["databases"][0];
    assert_eq!(db["branch_count"], 2, "main and dev; backups and deleted branches don't count");
    assert_eq!(db["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(metadata.main_count_queries.load(Ordering::SeqCst), 1);

    let get: Value = call(&app, Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
    assert_eq!(get["branch_count"], 2);
    assert_eq!(get["host"], "ep-0123456789ab.localtest.me");
}
