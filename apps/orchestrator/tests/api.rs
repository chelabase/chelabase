use axum::Router;
use cheladb_auth::test_support::{BrokenJwks, MockJwks};
use cheladb_auth::JwtVerifier;
use cheladb_metadata::{DatabaseRecord, DatabaseStatus, MetadataError};
use cheladb_nats::{JobEnvelope, ProvisionDatabaseJob};
use cheladb_types::{DatabaseId, ProjectId, TenantId};
use orchestrator::{build_router, AppState, JobPort, MetadataPort};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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

/// Boots the real orchestrator router over the given (possibly-fake) state on
/// an ephemeral port and returns its base URL.
async fn spawn_app_with_verifier(
    metadata: FakeMetadata,
    jobs: FakeJobs,
    verifier: JwtVerifier,
) -> String {
    let state = Arc::new(AppState {
        metadata,
        js: jobs,
        verifier,
    });
    let app: Router = build_router(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://{addr}")
}

/// Boots the real orchestrator router (fake metadata/jobs, mock JWKS) on an
/// ephemeral port and returns its base URL plus the mock JWKS handle for signing.
async fn spawn_app(metadata: FakeMetadata, jobs: FakeJobs) -> (String, MockJwks) {
    let mock = MockJwks::start().await;
    let verifier = JwtVerifier::new(mock.config(vec![]));
    let base = spawn_app_with_verifier(metadata, jobs, verifier).await;
    (base, mock)
}

#[tokio::test]
async fn health_is_public() {
    let (base, _mock) = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn missing_bearer_token_is_401() {
    let (base, _mock) = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .json(&serde_json::json!({
            "tenant_id": TenantId::new(),
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn invalid_bearer_token_is_401() {
    let (base, _mock) = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth("not-a-real-jwt")
        .json(&serde_json::json!({
            "tenant_id": TenantId::new(),
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn expired_bearer_token_is_401() {
    let metadata = FakeMetadata::default();
    let (base, mock) = spawn_app(metadata.clone(), FakeJobs::default()).await;
    let token = mock.sign(serde_json::json!({
        "sub": "user-1",
        "iss": mock.issuer(),
        "exp": 1,
        cheladb_auth::CLAIM_ORG_ID: "org-1",
    }));

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": TenantId::new(),
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn unbound_org_is_403_with_bootstrap_hint() {
    let (base, mock) = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-not-bound", 300);

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": TenantId::new(),
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("zitadel-bootstrap"));
}

#[tokio::test]
async fn body_tenant_mismatch_is_403() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let (base, mock) = spawn_app(metadata, FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let other_tenant = TenantId::new();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": other_tenant,
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn valid_request_is_202_accepted_and_publishes_job() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let jobs = FakeJobs::default();
    let (base, mock) = spawn_app(metadata, jobs.clone()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let project_id = ProjectId::new();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": bound_tenant,
            "project_id": project_id,
            "name": "demo-db",
        }))
        .send()
        .await
        .unwrap();

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
    let (base, mock) = spawn_app(metadata, FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": bound_tenant,
            "project_id": ProjectId::new(),
            "name": "   ",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn duplicate_database_name_is_409() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    metadata.fail_next_insert();
    let (base, mock) = spawn_app(metadata, FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": bound_tenant,
            "project_id": ProjectId::new(),
            "name": "demo-db",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn jwks_unreachable_is_503() {
    let broken = BrokenJwks::start().await;
    let verifier = JwtVerifier::new(cheladb_auth::ZitadelAuthConfig {
        issuer: "http://irrelevant-issuer".into(),
        jwks_url: broken.jwks_url().to_string(),
        audiences: vec![],
    });
    let base = spawn_app_with_verifier(FakeMetadata::default(), FakeJobs::default(), verifier).await;

    // Any syntactically valid bearer with a kid triggers a JWKS fetch attempt,
    // which fails against `broken` before the mismatched issuer above matters.
    let mock = MockJwks::start().await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/databases"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "tenant_id": TenantId::new(),
            "project_id": ProjectId::new(),
            "name": "demo",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn get_database_own_tenant_is_200() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let record = sample_database(bound_tenant);
    let database_id = record.database_id;
    metadata.insert_database(record);
    let (base, mock) = spawn_app(metadata, FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/databases/{}", database_id.as_uuid()))
        .bearer_auth(token)
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
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let other_tenant = TenantId::new();
    let record = sample_database(other_tenant);
    let database_id = record.database_id;
    metadata.insert_database(record);
    let (base, mock) = spawn_app(metadata, FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/databases/{}", database_id.as_uuid()))
        .bearer_auth(token)
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
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let (base, mock) = spawn_app(metadata, FakeJobs::default()).await;
    let token = mock.token_for_org("user-1", "org-1", 300);

    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/databases/{}", uuid::Uuid::new_v4()))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}
