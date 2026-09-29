use axum::Router;
use cheladb_auth::internal::{
    generate_keypair_pem, AuthMethod, Identity, InternalSigner, InternalVerifier, IDENTITY_HEADER,
};
use cheladb_metadata::{
    DatabaseRecord, DatabaseStatus, MetadataError, ProjectOverview, TenantOverview,
};
use cheladb_nats::{
    DeleteDatabaseJob, JobEnvelope, ProvisionDatabaseJob, ResetPasswordReply, RESET_NOT_APPLIED,
};
use cheladb_routes::{Service, ENDPOINTS};
use cheladb_types::{DatabaseId, ProjectId, TenantId};
use cheladb_secrets::{seal_credential, Purpose, SecretKey};
use orchestrator::{build_router, AppState, JobPort, MetadataPort, ResetError};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct FakeMetadata {
    tenants_by_org: Arc<Mutex<HashMap<String, TenantId>>>,
    fail_insert: Arc<Mutex<bool>>,
    databases: Arc<Mutex<HashMap<DatabaseId, DatabaseRecord>>>,
    overviews: Arc<Mutex<HashMap<TenantId, TenantOverview>>>,
}

impl FakeMetadata {
    fn bind_org(&self, org_id: &str, tenant_id: TenantId) {
        self.tenants_by_org
            .lock()
            .unwrap()
            .insert(org_id.to_string(), tenant_id);
    }

    fn insert_database(&self, record: DatabaseRecord) {
        self.databases
            .lock()
            .unwrap()
            .insert(record.database_id, record);
    }

    fn fail_next_insert(&self) {
        *self.fail_insert.lock().unwrap() = true;
    }

    fn set_overview(&self, overview: TenantOverview) {
        self.overviews.lock().unwrap().insert(overview.id, overview);
    }
}

/// A minimal `Ready` database record for the given tenant, for GET/soft-bind tests
/// that don't care about provisioning details.
fn sample_database(tenant_id: TenantId) -> DatabaseRecord {
    DatabaseRecord {
        database_id: DatabaseId::new(),
        project_id: ProjectId::new(),
        tenant_id,
        name: "demo-db".into(),
        status: DatabaseStatus::Ready,
        endpoint_id: Some("ep-0123456789ab".into()),
        database_name: None,
        owner_role: None,
        owner_password_enc: None,
        error: None,
    }
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
    resets: Arc<Mutex<Vec<DatabaseId>>>,
    reset: Arc<Mutex<FakeReset>>,
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

    async fn reset_password(&self, database_id: DatabaseId) -> Result<ResetPasswordReply, ResetError> {
        self.resets.lock().unwrap().push(database_id);
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
    db.owner_password_enc = Some("v1:secret-owner-sealed".into());
    TenantOverview {
        id: tenant_id,
        slug: "demo".into(),
        projects: vec![ProjectOverview {
            id: project_id,
            slug: "default".into(),
            databases: vec![db],
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
fn ready_database(tenant: TenantId, key: &SecretKey) -> DatabaseRecord {
    let mut db = sample_database(tenant);
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    db.owner_password_enc =
        Some(seal_credential(key, &db.database_id.to_string(), Purpose::Owner, "s3cret"));
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
        db.endpoint_id = None;
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
    bare.endpoint_id = None;
    bare.project_id = overview.projects[0].id;
    let (linked_id, bare_id) = (overview.projects[0].databases[0].database_id, bare.database_id);
    metadata.insert_database(overview.projects[0].databases[0].clone());
    metadata.insert_database(bare.clone());
    let mut overview = overview;
    overview.projects[0].databases.push(bare);
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
    assert!(db.endpoint_id.is_some());
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
        db.owner_password_enc = None;
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
    assert_eq!(*jobs.resets.lock().unwrap(), vec![id.unwrap()]);
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
        db.endpoint_id = None;
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
