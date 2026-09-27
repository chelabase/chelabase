use axum::Router;
use cheladb_auth::internal::{
    generate_keypair_pem, AuthMethod, Identity, InternalSigner, InternalVerifier, IDENTITY_HEADER,
};
use cheladb_metadata::{DatabaseRecord, DatabaseStatus, MetadataError};
use cheladb_nats::{JobEnvelope, ProvisionDatabaseJob};
use cheladb_routes::{Service, ENDPOINTS};
use cheladb_types::{DatabaseId, ProjectId, TenantId};
use orchestrator::{build_router, AppState, JobPort, MetadataPort};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct FakeMetadata {
    tenants_by_org: Arc<Mutex<HashMap<String, TenantId>>>,
    fail_insert: Arc<Mutex<bool>>,
    databases: Arc<Mutex<HashMap<DatabaseId, DatabaseRecord>>>,
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
        container_id: None,
        host: None,
        port: None,
        admin_password: None,
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
}

#[derive(Clone, Default)]
struct FakeJobs {
    published: Arc<Mutex<Vec<JobEnvelope<ProvisionDatabaseJob>>>>,
}

impl JobPort for FakeJobs {
    async fn publish_provision(
        &self,
        job: &JobEnvelope<ProvisionDatabaseJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.published.lock().unwrap().push(job.clone());
        Ok(())
    }
}

/// A running orchestrator plus the signer standing in for the gateway.
struct TestApp {
    base: String,
    signer: InternalSigner,
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
    let state = Arc::new(AppState {
        metadata,
        js: jobs,
        identity: InternalVerifier::from_pem(public_pem.as_bytes()).unwrap(),
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
