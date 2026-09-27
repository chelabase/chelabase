use axum::{
    extract::{FromRequestParts, Path, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use cheladb_auth::internal::{InternalVerifier, IDENTITY_HEADER};
use cheladb_routes::{orchestrator as routes, Service};
use cheladb_metadata::{
    DatabaseRecord, DatabaseStatus, MetadataClient, MetadataError, ProjectOverview, TenantOverview,
};
use cheladb_nats::{JetStreamContext, JobEnvelope, ProvisionDatabaseJob};
use cheladb_types::{DatabaseId, ProjectId, TenantId};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

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
}

/// Abstraction over enqueuing a provision job, so tests can inject an
/// in-memory fake instead of a real NATS JetStream context.
pub trait JobPort: Send + Sync + 'static {
    fn publish_provision(
        &self,
        job: &JobEnvelope<ProvisionDatabaseJob>,
    ) -> impl Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send;
}

impl JobPort for JetStreamContext {
    async fn publish_provision(
        &self,
        job: &JobEnvelope<ProvisionDatabaseJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        cheladb_nats::publish_provision(self, job)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

pub struct AppState<M, J> {
    pub metadata: M,
    pub js: J,
    /// Verifies gateway-minted internal identity tokens.
    pub identity: InternalVerifier,
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
    /// Dev-only; present when ready for local poking.
    admin_password: Option<String>,
    error: Option<String>,
}

impl From<DatabaseRecord> for DatabaseResponse {
    fn from(r: DatabaseRecord) -> Self {
        Self {
            database_id: r.database_id,
            project_id: r.project_id,
            tenant_id: r.tenant_id,
            name: r.name,
            status: r.status,
            host: r.host,
            port: r.port,
            admin_password: r.admin_password,
            error: r.error,
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
}

impl From<TenantOverview> for TenantView {
    fn from(t: TenantOverview) -> Self {
        Self {
            id: t.id,
            slug: t.slug,
            projects: t.projects.into_iter().map(ProjectView::from).collect(),
        }
    }
}

impl From<ProjectOverview> for ProjectView {
    fn from(p: ProjectOverview) -> Self {
        Self {
            id: p.id,
            slug: p.slug,
            databases: p
                .databases
                .into_iter()
                .map(|d| DatabaseSummary {
                    database_id: d.database_id,
                    name: d.name,
                    status: d.status,
                    host: d.host,
                    port: d.port,
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
        .route(routes::GET_DATABASE.path, get(get_database::<M, J>))
        .route(routes::ME.path, get(me::<M, J>))
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
            Ok(overview) => overview.map(TenantView::from),
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
        Ok(Some(record)) => {
            if record.tenant_id != principal.tenant_id {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({ "error": "forbidden" })),
                )
                    .into_response();
            }
            (StatusCode::OK, Json(DatabaseResponse::from(record))).into_response()
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
