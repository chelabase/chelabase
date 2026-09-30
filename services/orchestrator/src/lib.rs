use axum::{
    extract::{FromRequestParts, Path, State},
    http::{header::CACHE_CONTROL, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use cheladb_auth::internal::{InternalVerifier, IDENTITY_HEADER};
use cheladb_routes::{orchestrator as routes, Service};
use cheladb_metadata::{
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, CreateBranchOutcome, DatabaseRecord, DatabaseStatus,
    MetadataClient, MetadataError, NewBranch, ProjectOverview, TenantOverview,
};
use cheladb_nats::{
    BranchJob, DatabaseSettingsJob, DeleteDatabaseJob, JetStreamContext, JobEnvelope,
    ProvisionDatabaseJob, ResetPasswordReply, ResetPasswordRequest, RESET_NOT_APPLIED,
    SUBJECT_RESET_PASSWORD,
};
use cheladb_secrets::{open_credential, Purpose, SecretKey};
use cheladb_types::{BranchId, DatabaseId, ProjectId, TenantId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

mod branches;
pub mod internal;

/// Port the Neon proxy serves the Postgres protocol on.
pub const PROXY_PG_PORT: u16 = 4432;
/// Port the Neon proxy serves SQL-over-HTTP on.
pub const PROXY_HTTP_PORT: u16 = 4444;

/// The proxy's host name for an endpoint.
fn proxy_host(endpoint_id: &str, domain: &str) -> String {
    format!("{endpoint_id}.{domain}")
}

/// Abstraction over tenant/database metadata lookups, so tests can inject an
/// in-memory fake instead of a real Postgres-backed `MetadataClient`.
pub trait MetadataPort: Send + Sync + 'static {
    fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> impl Future<Output = Result<Option<TenantId>, MetadataError>> + Send;

    fn insert_database_pending(
        &self,
        database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        name: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    fn set_failed(
        &self,
        database_id: DatabaseId,
        error: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> impl Future<Output = Result<Option<DatabaseRecord>, MetadataError>> + Send;

    fn tenant_overview(
        &self,
        tenant_id: TenantId,
    ) -> impl Future<Output = Result<Option<TenantOverview>, MetadataError>> + Send;

    /// A branch; a database's main has the database's id and holds its endpoint and password.
    fn get_branch(
        &self,
        branch_id: BranchId,
    ) -> impl Future<Output = Result<Option<BranchRecord>, MetadataError>> + Send;

    /// Move to `deleting`; false when already deleting, deleted, or missing.
    fn begin_delete(
        &self,
        database_id: DatabaseId,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// The database's branches that aren't deleted, oldest first (backups included).
    fn list_branches(
        &self,
        database_id: DatabaseId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;

    /// Main of each of the tenant's databases with the database's live `normal` branch count
    /// (main included), in one query.
    fn main_branches_with_counts(
        &self,
        tenant_id: TenantId,
    ) -> impl Future<Output = Result<Vec<(BranchRecord, i64)>, MetadataError>> + Send;

    /// Inserts a pending branch, enforcing the name, the cap and a ready parent.
    fn create_branch(
        &self,
        branch: &NewBranch<'_>,
    ) -> impl Future<Output = Result<CreateBranchOutcome, MetadataError>> + Send;

    /// Live `normal` children (backups excluded).
    fn live_children(
        &self,
        branch_id: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;

    /// Guarded status change; true when it happened.
    fn set_branch_status(
        &self,
        branch_id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// ready → resetting, clearing the last error; true when it claimed the branch.
    fn claim_branch_for_switch(
        &self,
        branch_id: BranchId,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    fn set_branch_failed(
        &self,
        branch_id: BranchId,
        error: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    /// Move the branch and its backups to `deleting`; refused while the branch or one of its
    /// backups has live `normal` children.
    fn begin_branch_delete(
        &self,
        branch_id: BranchId,
    ) -> impl Future<Output = Result<BranchDeleteOutcome, MetadataError>> + Send;

    /// Changes the given settings; false once the database is deleting, or missing.
    fn update_database_settings(
        &self,
        database_id: DatabaseId,
        history_window: Option<std::time::Duration>,
        max_branches: Option<i32>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
}

// Each method below must delegate to `MetadataClient`'s inherent method of the
// same name (not to itself): renaming an inherent method here without updating
// the call site would silently make this resolve to the trait method instead,
// recursing forever.
impl MetadataPort for MetadataClient {
    async fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<TenantId>, MetadataError> {
        MetadataClient::get_tenant_by_zitadel_org_id(self, zitadel_org_id).await
    }

    async fn insert_database_pending(
        &self,
        database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        name: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::insert_database_pending(self, database_id, tenant_id, project_id, name)
            .await
    }

    async fn set_failed(&self, database_id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_failed(self, database_id, error).await
    }

    async fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Option<DatabaseRecord>, MetadataError> {
        MetadataClient::get_database(self, database_id).await
    }

    async fn tenant_overview(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<TenantOverview>, MetadataError> {
        MetadataClient::tenant_overview(self, tenant_id).await
    }

    async fn get_branch(&self, branch_id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        MetadataClient::get_branch(self, branch_id).await
    }

    async fn begin_delete(&self, database_id: DatabaseId) -> Result<bool, MetadataError> {
        MetadataClient::begin_delete(self, database_id).await
    }

    async fn list_branches(&self, database_id: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::list_branches(self, database_id).await
    }

    async fn main_branches_with_counts(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<(BranchRecord, i64)>, MetadataError> {
        MetadataClient::main_branches_with_counts(self, tenant_id).await
    }

    async fn create_branch(&self, branch: &NewBranch<'_>) -> Result<CreateBranchOutcome, MetadataError> {
        MetadataClient::create_branch(self, branch).await
    }

    async fn live_children(&self, branch_id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::live_children(self, branch_id).await
    }

    async fn set_branch_status(
        &self,
        branch_id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_branch_status(self, branch_id, from, to).await
    }

    async fn claim_branch_for_switch(&self, branch_id: BranchId) -> Result<bool, MetadataError> {
        MetadataClient::claim_branch_for_switch(self, branch_id).await
    }

    async fn set_branch_failed(&self, branch_id: BranchId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_branch_failed(self, branch_id, error).await
    }

    async fn begin_branch_delete(
        &self,
        branch_id: BranchId,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        MetadataClient::begin_branch_delete(self, branch_id).await
    }

    async fn update_database_settings(
        &self,
        database_id: DatabaseId,
        history_window: Option<std::time::Duration>,
        max_branches: Option<i32>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::update_database_settings(self, database_id, history_window, max_branches)
            .await
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Why a reset-password request got no answer from the worker.
#[derive(Debug)]
pub enum ResetError {
    /// A worker is listening but didn't answer in time.
    Timeout,
    /// No worker listening, or the request couldn't be sent or parsed.
    Transport(String),
}

/// Talking to the worker: durable jobs and the reset-password RPC. Abstracted
/// so tests can inject an in-memory fake instead of NATS.
pub trait JobPort: Send + Sync + 'static {
    fn publish_provision(
        &self,
        job: &JobEnvelope<ProvisionDatabaseJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    fn publish_delete(
        &self,
        job: &JobEnvelope<DeleteDatabaseJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    fn publish_branch(
        &self,
        job: &JobEnvelope<BranchJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    fn publish_database_settings(
        &self,
        job: &JobEnvelope<DatabaseSettingsJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    /// Resets the owner password of one branch (main's id is the database's).
    fn reset_password(
        &self,
        database_id: DatabaseId,
        branch_id: BranchId,
    ) -> impl Future<Output = Result<ResetPasswordReply, ResetError>> + Send;
}

/// How long the orchestrator waits for the worker to reset a password.
const RESET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// NATS-backed [`JobPort`]: JetStream for jobs, core request/reply for resets.
#[derive(Clone)]
pub struct NatsPorts {
    pub js: JetStreamContext,
    pub client: async_nats::Client,
}

impl JobPort for NatsPorts {
    async fn publish_provision(&self, job: &JobEnvelope<ProvisionDatabaseJob>) -> Result<(), BoxError> {
        cheladb_nats::publish_provision(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn publish_delete(&self, job: &JobEnvelope<DeleteDatabaseJob>) -> Result<(), BoxError> {
        cheladb_nats::publish_delete(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn publish_branch(&self, job: &JobEnvelope<BranchJob>) -> Result<(), BoxError> {
        cheladb_nats::publish_branch(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn publish_database_settings(
        &self,
        job: &JobEnvelope<DatabaseSettingsJob>,
    ) -> Result<(), BoxError> {
        cheladb_nats::publish_database_settings(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn reset_password(
        &self,
        database_id: DatabaseId,
        branch_id: BranchId,
    ) -> Result<ResetPasswordReply, ResetError> {
        use async_nats::RequestErrorKind;
        let body = serde_json::to_vec(&ResetPasswordRequest { database_id, branch_id })
            .map_err(|e| ResetError::Transport(e.to_string()))?;
        // A per-request timeout: the client's default (10 s) would otherwise
        // cut the documented 15 s short.
        let request = async_nats::Request::new()
            .payload(body.into())
            .timeout(Some(RESET_TIMEOUT));
        let reply = self
            .client
            .send_request(SUBJECT_RESET_PASSWORD, request)
            .await
            .map_err(|e| match e.kind() {
            RequestErrorKind::TimedOut => ResetError::Timeout,
            RequestErrorKind::NoResponders => ResetError::Transport("no worker listening".into()),
            _ => ResetError::Transport(e.to_string()),
        })?;
        serde_json::from_slice(&reply.payload).map_err(|e| ResetError::Transport(e.to_string()))
    }
}

pub struct AppState<M, J> {
    pub metadata: M,
    pub js: J,
    /// Verifies gateway-minted internal identity tokens.
    pub identity: InternalVerifier,
    /// Decrypts stored database credentials for reveal.
    pub secrets: SecretKey,
    /// Domain the Neon proxy serves endpoints under: `<ep>.<proxy_domain>`.
    pub proxy_domain: String,
}

/// Caller verified from the gateway-minted internal identity token. Does not
/// require the caller's org to be bound to a tenant.
#[derive(Debug, Clone)]
struct Caller {
    sub: String,
    org_id: String,
    request_id: String,
}

impl<M, J> FromRequestParts<Arc<AppState<M, J>>> for Caller
where
    M: MetadataPort,
    J: JobPort,
{
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState<M, J>>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(IDENTITY_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                unauthorized("missing internal identity; call the API through the gateway")
            })?;

        let claims = state
            .identity
            .verify(token, Service::Orchestrator.name())
            .map_err(|err| unauthorized(&err.to_string()))?;

        Ok(Caller {
            sub: claims.sub,
            org_id: claims.org_id,
            request_id: claims.rid,
        })
    }
}

/// Caller whose org is bound to a Cheladb tenant (soft-bind).
#[derive(Debug, Clone)]
struct AuthPrincipal {
    #[allow(dead_code)]
    sub: String,
    #[allow(dead_code)]
    org_id: String,
    tenant_id: TenantId,
    request_id: String,
}

impl<M, J> FromRequestParts<Arc<AppState<M, J>>> for AuthPrincipal
where
    M: MetadataPort,
    J: JobPort,
{
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState<M, J>>,
    ) -> Result<Self, Self::Rejection> {
        let caller = Caller::from_request_parts(parts, state).await?;
        let tenant_id = lookup_tenant(state, &caller.org_id)
            .await
            .map_err(|_| internal_error())?
            .ok_or_else(|| {
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "no Cheladb tenant bound to this Zitadel org; run just zitadel-bootstrap"
                })),
            )
                .into_response()
        })?;

        Ok(AuthPrincipal {
            sub: caller.sub,
            org_id: caller.org_id,
            tenant_id,
            request_id: caller.request_id,
        })
    }
}

/// The tenant bound to `org_id`, if any (lookup failures are logged).
async fn lookup_tenant<M: MetadataPort, J>(
    state: &AppState<M, J>,
    org_id: &str,
) -> Result<Option<TenantId>, MetadataError> {
    state
        .metadata
        .get_tenant_by_zitadel_org_id(org_id)
        .await
        .inspect_err(|err| error!("tenant lookup by zitadel org: {err}"))
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": "internal error" })),
    )
        .into_response()
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct CreateDatabaseRequest {
    pub tenant_id: TenantId,
    pub project_id: ProjectId,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CreateDatabaseAccepted {
    pub job_id: Uuid,
    pub database_id: DatabaseId,
    pub status: DatabaseStatus,
}

#[derive(Debug, Serialize)]
struct DatabaseResponse {
    database_id: DatabaseId,
    project_id: ProjectId,
    tenant_id: TenantId,
    name: String,
    status: DatabaseStatus,
    host: Option<String>,
    port: Option<u16>,
    error: Option<String>,
    /// Live `normal` branches, main included (backups and deleted branches don't count).
    branch_count: i64,
    /// Seconds of history the database keeps.
    history_window: u64,
    max_branches: i32,
}

/// The proxy's host and Postgres port, once the database is ready: until then its endpoint
/// may have no compute yet, or be torn down.
fn proxy_address(
    status: DatabaseStatus,
    endpoint_id: Option<&str>,
    proxy_domain: &str,
) -> Option<(String, u16)> {
    match endpoint_id {
        Some(ep) if status == DatabaseStatus::Ready => {
            Some((proxy_host(ep, proxy_domain), PROXY_PG_PORT))
        }
        _ => None,
    }
}

/// Main's endpoint, if main has one.
fn main_endpoint(main: Option<&BranchRecord>) -> Option<&str> {
    main.and_then(|b| b.endpoint_id.as_deref())
}

/// Each database's main branch and branch count, for the summaries: one query for the tenant.
async fn main_branches<M: MetadataPort>(
    metadata: &M,
    tenant_id: TenantId,
) -> Result<HashMap<DatabaseId, (BranchRecord, i64)>, MetadataError> {
    Ok(metadata
        .main_branches_with_counts(tenant_id)
        .await?
        .into_iter()
        .map(|(main, count)| (main.database_id, (main, count)))
        .collect())
}

impl DatabaseResponse {
    fn new(
        r: DatabaseRecord,
        main: Option<&BranchRecord>,
        branch_count: i64,
        proxy_domain: &str,
    ) -> Self {
        let (host, port) = proxy_address(r.status, main_endpoint(main), proxy_domain).unzip();
        Self {
            database_id: r.database_id,
            project_id: r.project_id,
            tenant_id: r.tenant_id,
            name: r.name,
            status: r.status,
            host,
            port,
            error: r.error,
            branch_count,
            history_window: r.history_window.as_secs(),
            max_branches: r.max_branches,
        }
    }
}

#[derive(Debug, Serialize)]
struct MeResponse {
    user: MeUser,
    /// `None` when the caller's org isn't bound to a Cheladb tenant yet.
    tenant: Option<TenantView>,
}

#[derive(Debug, Serialize)]
struct MeUser {
    sub: String,
    org_id: String,
}

#[derive(Debug, Serialize)]
struct TenantView {
    id: TenantId,
    slug: String,
    projects: Vec<ProjectView>,
}

#[derive(Debug, Serialize)]
struct ProjectView {
    id: ProjectId,
    slug: String,
    databases: Vec<DatabaseSummary>,
}

/// Public database fields only: never credentials or container ids.
#[derive(Debug, Serialize)]
struct DatabaseSummary {
    database_id: DatabaseId,
    name: String,
    status: DatabaseStatus,
    host: Option<String>,
    port: Option<u16>,
    /// Live `normal` branches, main included.
    branch_count: i64,
}

impl TenantView {
    fn new(
        t: TenantOverview,
        mains: &HashMap<DatabaseId, (BranchRecord, i64)>,
        proxy_domain: &str,
    ) -> Self {
        Self {
            id: t.id,
            slug: t.slug,
            projects: t
                .projects
                .into_iter()
                .map(|p| ProjectView::new(p, mains, proxy_domain))
                .collect(),
        }
    }
}

impl ProjectView {
    fn new(
        p: ProjectOverview,
        mains: &HashMap<DatabaseId, (BranchRecord, i64)>,
        proxy_domain: &str,
    ) -> Self {
        Self {
            id: p.id,
            slug: p.slug,
            databases: p
                .databases
                .into_iter()
                .map(|d| {
                    let main = mains.get(&d.database_id);
                    let (host, port) =
                        proxy_address(d.status, main_endpoint(main.map(|m| &m.0)), proxy_domain)
                            .unzip();
                    DatabaseSummary {
                        database_id: d.database_id,
                        name: d.name,
                        status: d.status,
                        host,
                        port,
                        branch_count: main.map_or(0, |m| m.1),
                    }
                })
                .collect(),
        }
    }
}

/// Build the orchestrator's axum router over the given (possibly-fake) state.
pub fn build_router<M, J>(state: Arc<AppState<M, J>>) -> Router
where
    M: MetadataPort,
    J: JobPort,
{
    Router::new()
        .route(routes::HEALTH.path, get(health))
        .route(routes::CREATE_DATABASE.path, post(create_database::<M, J>))
        // GET and DELETE share one path (cheladb-routes GET_DATABASE / DELETE_DATABASE).
        .route(
            routes::GET_DATABASE.path,
            get(get_database::<M, J>)
                .delete(delete_database::<M, J>)
                .patch(branches::update_database::<M, J>),
        )
        .route(routes::DATABASE_CONNECTION.path, get(database_connection::<M, J>))
        .route(routes::RESET_PASSWORD.path, post(reset_password::<M, J>))
        .route(routes::ME.path, get(me::<M, J>))
        .merge(branches::routes::<M, J>())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "orchestrator" }))
}

async fn me<M, J>(State(state): State<Arc<AppState<M, J>>>, caller: Caller) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let tenant = match lookup_tenant(&state, &caller.org_id).await {
        Err(_) => return internal_error(),
        Ok(None) => None,
        Ok(Some(tenant_id)) => match state.metadata.tenant_overview(tenant_id).await {
            Ok(None) => None,
            Ok(Some(overview)) => match main_branches(&state.metadata, overview.id).await {
                Ok(mains) => Some(TenantView::new(overview, &mains, &state.proxy_domain)),
                Err(err) => {
                    error!("main branches: {err}");
                    return internal_error();
                }
            },
            Err(err) => {
                error!("tenant overview: {err}");
                return internal_error();
            }
        },
    };
    Json(MeResponse {
        user: MeUser {
            sub: caller.sub,
            org_id: caller.org_id,
        },
        tenant,
    })
    .into_response()
}

async fn create_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Json(body): Json<CreateDatabaseRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    if body.tenant_id != principal.tenant_id {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "tenant_id does not match authenticated Zitadel org binding"
            })),
        )
            .into_response();
    }

    if body.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name is required" })),
        )
            .into_response();
    }
    if let Err(err) = cheladb_metadata::postgres_database_name(&body.name) {
        return json_error(StatusCode::BAD_REQUEST, &err.to_string());
    }

    let job_id = Uuid::new_v4();
    let database_id = DatabaseId::new();

    if let Err(err) = state
        .metadata
        .insert_database_pending(database_id, body.tenant_id, body.project_id, &body.name)
        .await
    {
        return map_metadata_error(err);
    }

    let job = JobEnvelope {
        job_id,
        payload: ProvisionDatabaseJob {
            database_id,
            project_id: body.project_id,
            tenant_id: body.tenant_id,
            name: body.name.clone(),
        },
    };

    if let Err(err) = state.js.publish_provision(&job).await {
        error!("publish failed: {err}");
        let _ = state
            .metadata
            .set_failed(database_id, &format!("publish failed: {err}"))
            .await;
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "failed to enqueue provision job" })),
        )
            .into_response();
    }

    info!(
        %job_id,
        %database_id,
        name = %body.name,
        request_id = %principal.request_id,
        "accepted create database"
    );

    (
        StatusCode::ACCEPTED,
        Json(CreateDatabaseAccepted {
            job_id,
            database_id,
            status: DatabaseStatus::Pending,
        }),
    )
        .into_response()
}

async fn get_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let database_id = DatabaseId(id);
    match state.metadata.get_database(database_id).await {
        // Tombstones are invisible through the API.
        Ok(Some(record)) if record.status == DatabaseStatus::Deleted => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "database not found" })),
        )
            .into_response(),
        Ok(Some(record)) => {
            if record.tenant_id != principal.tenant_id {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({ "error": "forbidden" })),
                )
                    .into_response();
            }
            let branches = match state.metadata.list_branches(database_id).await {
                Ok(branches) => branches,
                Err(err) => {
                    error!("list branches: {err}");
                    return internal_error();
                }
            };
            let count = branches.iter().filter(|b| b.kind == BranchKind::Normal).count() as i64;
            let main = branches.iter().find(|b| b.id == BranchId::from(database_id));
            (
                StatusCode::OK,
                Json(DatabaseResponse::new(record, main, count, &state.proxy_domain)),
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "database not found" })),
        )
            .into_response(),
        Err(err) => {
            error!("get_database: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
                .into_response()
        }
    }
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// The caller's own, non-deleted database, or the response to send instead.
async fn load_owned<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
) -> Result<DatabaseRecord, Box<Response>>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.get_database(DatabaseId(id)).await {
        Ok(Some(record)) if record.status == DatabaseStatus::Deleted => {
            Err(Box::new(json_error(StatusCode::NOT_FOUND, "database not found")))
        }
        Ok(Some(record)) if record.tenant_id != principal.tenant_id => {
            Err(Box::new(json_error(StatusCode::FORBIDDEN, "forbidden")))
        }
        Ok(Some(record)) => Ok(record),
        Ok(None) => Err(Box::new(json_error(StatusCode::NOT_FOUND, "database not found"))),
        Err(err) => {
            error!("load database: {err}");
            Err(Box::new(internal_error()))
        }
    }
}

/// The database's main branch, if it has one.
async fn load_main<M, J>(
    state: &AppState<M, J>,
    database_id: DatabaseId,
) -> Result<Option<BranchRecord>, Box<Response>>
where
    M: MetadataPort,
    J: JobPort,
{
    state
        .metadata
        .get_branch(BranchId::from(database_id))
        .await
        .map_err(|err| {
            error!("load main branch: {err}");
            Box::new(internal_error())
        })
}

#[derive(Debug, Serialize)]
struct ConnectionDetails {
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    uri: String,
    http_url: String,
}

/// What a backup answers to reveal, reset-password, reset-to-parent and restore.
const BACKUP_REFUSAL: &str =
    "this is a restore backup with no compute; create a branch from it to connect";

/// Reveal the owner credentials of a ready database (main's). Never cached, never logged.
async fn database_connection<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    if record.status != DatabaseStatus::Ready {
        return json_error(
            StatusCode::CONFLICT,
            &format!(
                "database is {}; connection details are available once it is ready",
                record.status.as_str()
            ),
        );
    }
    // Main holds the database's endpoint and sealed password (AAD = its id = the database's).
    let main = match load_main(&state, record.database_id).await {
        Ok(main) => main,
        Err(resp) => return *resp,
    };
    reveal(&state, &principal, &record, main).await
}

/// The owner credentials of `branch`, a branch of `record`'s database.
async fn reveal<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: &DatabaseRecord,
    branch: Option<BranchRecord>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let Some(branch) = branch else {
        return json_error(
            StatusCode::CONFLICT,
            "credentials unavailable; recreate this database",
        );
    };
    if branch.kind == BranchKind::Backup {
        return json_error(StatusCode::CONFLICT, BACKUP_REFUSAL);
    }
    if branch.status != BranchStatus::Ready {
        return json_error(
            StatusCode::CONFLICT,
            &format!(
                "branch is {}; connection details are available once it is ready",
                branch.status.as_str()
            ),
        );
    }
    let sealed = branch.owner_password_enc.filter(|s| !s.is_empty());
    let (Some(endpoint_id), Some(database), Some(sealed)) =
        (branch.endpoint_id, record.database_name.clone(), sealed)
    else {
        return json_error(
            StatusCode::CONFLICT,
            "credentials unavailable; recreate this database",
        );
    };
    let password = match open_credential(
        &state.secrets,
        &branch.id.to_string(),
        Purpose::Owner,
        &sealed,
    ) {
        Ok(password) => password,
        Err(err) => {
            error!(database_id = %record.database_id, branch_id = %branch.id, "decrypt owner password: {err}");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stored credentials can't be decrypted with the current secrets key; \
                 restore the original CHELADB_SECRETS_KEY file",
            );
        }
    };
    let user = record.owner_role.clone().unwrap_or_else(|| "postgres".to_string());
    info!(
        request_id = %principal.request_id,
        sub = %principal.sub,
        database_id = %record.database_id,
        branch_id = %branch.id,
        "revealed connection details"
    );
    let host = proxy_host(&endpoint_id, &state.proxy_domain);
    let port = PROXY_PG_PORT;
    let uri = format!(
        "postgresql://{user}:{password}@{host}:{port}/{database}?sslmode=verify-full"
    );
    let http_url = format!("https://{host}:{PROXY_HTTP_PORT}/sql");
    (
        [(CACHE_CONTROL, "no-store")],
        Json(ConnectionDetails {
            host,
            port,
            database,
            user,
            password,
            uri,
            http_url,
        }),
    )
        .into_response()
}

async fn reset_password<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    if record.status != DatabaseStatus::Ready {
        return json_error(StatusCode::CONFLICT, "database is not ready");
    }
    let main = match load_main(&state, record.database_id).await {
        Ok(main) => main,
        Err(resp) => return *resp,
    };
    reset_branch_password(&state, &principal, &record, main).await
}

/// Resets the owner password of `branch`, a branch of `record`'s database, through the worker.
async fn reset_branch_password<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: &DatabaseRecord,
    branch: Option<BranchRecord>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    // A branch without an endpoint has nothing to reset; say so instead of failing inside
    // the worker.
    let Some(branch) = branch else {
        return json_error(
            StatusCode::CONFLICT,
            "credentials unavailable; recreate this database",
        );
    };
    if branch.kind == BranchKind::Backup {
        return json_error(StatusCode::CONFLICT, BACKUP_REFUSAL);
    }
    if branch.status != BranchStatus::Ready {
        return json_error(StatusCode::CONFLICT, "branch is not ready");
    }
    if branch.endpoint_id.is_none() {
        return json_error(
            StatusCode::CONFLICT,
            "credentials unavailable; recreate this database",
        );
    }
    let (database_id, branch_id) = (record.database_id, branch.id);
    match state.js.reset_password(database_id, branch_id).await {
        Ok(reply) if reply.ok => {
            info!(request_id = %principal.request_id, %database_id, %branch_id, "owner password reset");
            (StatusCode::OK, Json(serde_json::json!({}))).into_response()
        }
        Ok(reply) if reply.error.as_deref() == Some("not ready") => {
            json_error(StatusCode::CONFLICT, "branch is not ready")
        }
        Ok(reply) if reply.error.as_deref() == Some(RESET_NOT_APPLIED) => {
            json_error(StatusCode::SERVICE_UNAVAILABLE, RESET_NOT_APPLIED)
        }
        Ok(reply) if reply.error.as_deref() == Some("timed out") => json_error(
            StatusCode::GATEWAY_TIMEOUT,
            "the database did not respond; try again",
        ),
        Ok(reply) => {
            error!(%database_id, %branch_id, "reset password failed: {:?}", reply.error);
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "password reset failed")
        }
        Err(ResetError::Timeout) => json_error(
            StatusCode::GATEWAY_TIMEOUT,
            "worker did not respond; try again",
        ),
        Err(ResetError::Transport(err)) => {
            error!(%database_id, %branch_id, "reset password: {err}");
            json_error(StatusCode::SERVICE_UNAVAILABLE, "worker unavailable")
        }
    }
}

/// Start deleting a database (tombstone). The worker removes the container and
/// its data; a lost job is retried by the worker's reconcile loop.
async fn delete_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    match state.metadata.begin_delete(record.database_id).await {
        Ok(true) => {}
        Ok(false) => {
            return json_error(StatusCode::CONFLICT, "database is already being deleted")
        }
        Err(err) => {
            error!("begin delete: {err}");
            return internal_error();
        }
    }
    let job = JobEnvelope {
        job_id: Uuid::new_v4(),
        payload: DeleteDatabaseJob {
            database_id: record.database_id,
        },
    };
    if let Err(err) = state.js.publish_delete(&job).await {
        error!(database_id = %record.database_id, "publish delete failed (reconcile will retry): {err}");
    }
    info!(request_id = %principal.request_id, database_id = %record.database_id, "delete requested");
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "deleting" })),
    )
        .into_response()
}

fn map_metadata_error(err: MetadataError) -> Response {
    match err {
        MetadataError::TenantNotFound(_) | MetadataError::ProjectNotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
        MetadataError::ProjectTenantMismatch { .. } => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
        MetadataError::DuplicateDatabaseName => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "database name already exists in project" })),
        )
            .into_response(),
        other => {
            error!("metadata error: {other}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
                .into_response()
        }
    }
}
