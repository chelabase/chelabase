use axum::{
    extract::{FromRequestParts, State},
    http::{header::CACHE_CONTROL, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use cheladb_auth::internal::{InternalVerifier, IDENTITY_HEADER};
use cheladb_routes::{orchestrator as routes, Service};
use cheladb_metadata::{
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, CreateBranchOutcome, DatabaseRecord, DatabaseStatus,
    DataKeyRecord, MetadataClient, MetadataError, NewBranch, NewDataKey, ProjectOverview,
    SettingsPatch, TenantOverview,
};
use cheladb_nats::{
    BranchJob, CheckDataRoleReply, CheckDataRoleRequest, DatabaseSettingsJob, DeleteDatabaseJob, JetStreamContext, JobEnvelope,
    ProvisionDatabaseJob, ResetPasswordReply, ResetPasswordRequest, RESET_NOT_APPLIED,
    SUBJECT_CHECK_DATA_ROLE, SUBJECT_RESET_PASSWORD,
};
use cheladb_secrets::{open_credential, Purpose, SecretKey};
use cheladb_types::api_error::{ApiError, ErrorCode};
use cheladb_types::{ApiJson, ApiPath, BranchId, DatabaseId, ProjectId, TenantId};
use cheladb_types::api_error::ErrorEnvelope;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

mod branches;
mod data_keys;
mod docs;
pub mod internal;
mod openapi;
pub use openapi::openapi;

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
        patch: &SettingsPatch,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// Stores a key; a second live public key on the branch is `MetadataError::Conflict`.
    fn insert_data_key(
        &self,
        key: &NewDataKey,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    /// The branch's live keys, newest first.
    fn list_data_keys(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<DataKeyRecord>, MetadataError>> + Send;

    /// Revokes a live secret key; false for an unknown or revoked key, and for the public key.
    fn revoke_data_key(
        &self,
        branch: BranchId,
        key_id: Uuid,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// Revokes `old` and stores `new` in one step; false when `old` isn't a live key of the
    /// branch.
    fn rotate_data_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
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
        patch: &SettingsPatch,
    ) -> Result<bool, MetadataError> {
        MetadataClient::update_database_settings(self, database_id, patch).await
    }

    async fn insert_data_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        MetadataClient::insert_data_key(self, key).await
    }

    async fn list_data_keys(&self, branch: BranchId) -> Result<Vec<DataKeyRecord>, MetadataError> {
        MetadataClient::list_data_keys(self, branch).await
    }

    async fn revoke_data_key(&self, branch: BranchId, key_id: Uuid) -> Result<bool, MetadataError> {
        MetadataClient::revoke_data_key(self, branch, key_id).await
    }

    async fn rotate_data_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        MetadataClient::rotate_data_key(self, branch, old, new).await
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Why a reset-password request got no answer from the worker.
#[derive(Debug, Clone)]
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

    /// Asks the worker whether a secret key may use `role` on the branch (and grants it to
    /// `authenticator`).
    fn check_data_role(
        &self,
        branch_id: BranchId,
        role: &str,
    ) -> impl Future<Output = Result<CheckDataRoleReply, ResetError>> + Send;

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

    async fn check_data_role(
        &self,
        branch_id: BranchId,
        role: &str,
    ) -> Result<CheckDataRoleReply, ResetError> {
        use async_nats::RequestErrorKind;
        let body = serde_json::to_vec(&CheckDataRoleRequest {
            branch_id,
            role: role.to_string(),
        })
        .map_err(|e| ResetError::Transport(e.to_string()))?;
        let request = async_nats::Request::new()
            .payload(body.into())
            .timeout(Some(RESET_TIMEOUT));
        let reply = self
            .client
            .send_request(SUBJECT_CHECK_DATA_ROLE, request)
            .await
            .map_err(|e| match e.kind() {
                RequestErrorKind::TimedOut => ResetError::Timeout,
                RequestErrorKind::NoResponders => ResetError::Transport("no worker listening".into()),
                _ => ResetError::Transport(e.to_string()),
            })?;
        serde_json::from_slice(&reply.payload).map_err(|e| ResetError::Transport(e.to_string()))
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
    /// The REST service's internal listener (the data API's schema).
    pub rest: RestInternal,
}

/// Where the REST service's internal listener is, and the token it takes.
pub struct RestInternal {
    /// `http://host:port`, no path.
    pub url: String,
    pub token: String,
    pub http: reqwest::Client,
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
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState<M, J>>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(IDENTITY_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                ApiError::unauthorized("missing internal identity; call the API through the gateway")
            })?;

        let claims = state
            .identity
            .verify(token, Service::Orchestrator.name())
            .map_err(|err| ApiError::unauthorized(err.to_string()))?;

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
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState<M, J>>,
    ) -> Result<Self, Self::Rejection> {
        let caller = Caller::from_request_parts(parts, state).await?;
        let tenant_id = lookup_tenant(state, &caller.org_id)
            .await
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| {
                ApiError::new(
                    403,
                    ErrorCode::TenantNotBound,
                    "no Cheladb tenant bound to this Zitadel org; run just zitadel-bootstrap",
                )
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

/// The body of a JSON route that parses it by hand: the extractor only reads the bytes, so
/// a body over the limit answers in our shape instead of axum's plain text.
pub(crate) struct ApiBytes(pub(crate) axum::body::Bytes);

impl<S: Send + Sync> axum::extract::FromRequest<S> for ApiBytes {
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, ApiError> {
        match axum::body::Bytes::from_request(req, state).await {
            Ok(bytes) => Ok(Self(bytes)),
            Err(rej) => {
                let status = rej.status();
                let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
                    ErrorCode::TooLarge
                } else {
                    ErrorCode::InvalidRequest
                };
                Err(ApiError::new(status.as_u16(), code, rej.body_text()))
            }
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateDatabaseRequest {
    pub tenant_id: TenantId,
    pub project_id: ProjectId,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreateDatabaseAccepted {
    pub job_id: Uuid,
    pub database_id: DatabaseId,
    pub status: DatabaseStatus,
}

#[derive(Debug, Serialize, ToSchema)]
struct Database {
    database_id: DatabaseId,
    project_id: ProjectId,
    tenant_id: TenantId,
    name: String,
    status: DatabaseStatus,
    #[schema(required = true)]
    host: Option<String>,
    #[schema(required = true)]
    port: Option<u16>,
    #[schema(required = true)]
    error: Option<String>,
    /// Live `normal` branches, main included (backups and deleted branches don't count).
    branch_count: i64,
    /// Seconds of history the database keeps.
    history_window: u64,
    max_branches: i32,
    max_rows: i32,
    statement_timeout_ms: i32,
    max_connections: i32,
    default_schema: String,
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

impl Database {
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
            max_rows: r.max_rows,
            statement_timeout_ms: r.statement_timeout_ms,
            max_connections: r.max_connections,
            default_schema: r.default_schema,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
struct Me {
    user: MeUser,
    /// `None` when the caller's org isn't bound to a Cheladb tenant yet.
    #[schema(required = true)]
    tenant: Option<Tenant>,
}

#[derive(Debug, Serialize, ToSchema)]
struct MeUser {
    sub: String,
    org_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct Tenant {
    id: TenantId,
    slug: String,
    projects: Vec<Project>,
}

#[derive(Debug, Serialize, ToSchema)]
struct Project {
    id: ProjectId,
    slug: String,
    databases: Vec<DatabaseSummary>,
}

/// Public database fields only: never credentials or container ids.
#[derive(Debug, Serialize, ToSchema)]
struct DatabaseSummary {
    database_id: DatabaseId,
    name: String,
    status: DatabaseStatus,
    #[schema(required = true)]
    host: Option<String>,
    #[schema(required = true)]
    port: Option<u16>,
    /// Live `normal` branches, main included.
    branch_count: i64,
}

impl Tenant {
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
                .map(|p| Project::new(p, mains, proxy_domain))
                .collect(),
        }
    }
}

impl Project {
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

/// The documented routes: everything but `/health`. `build_router` serves them and
/// `openapi()` describes them.
pub(crate) fn api_router<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(create_database))
        // GET, PATCH and DELETE share one path (cheladb-routes GET_DATABASE / UPDATE_DATABASE /
        // DELETE_DATABASE).
        .routes(routes!(get_database, branches::update_database, delete_database))
        .routes(routes!(database_connection))
        .routes(routes!(reset_password))
        .routes(routes!(me))
        .merge(branches::routes())
        .merge(data_keys::routes())
}

/// Build the orchestrator's axum router over the given (possibly-fake) state.
pub fn build_router<M, J>(state: Arc<AppState<M, J>>) -> Router
where
    M: MetadataPort,
    J: JobPort,
{
    let (router, _) = api_router().split_for_parts();
    router
        .route(routes::HEALTH.path, get(health))
        .merge(docs::router())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "orchestrator" }))
}

#[utoipa::path(
    get,
    path = "/v1/me",
    operation_id = "getMe",
    tag = "me",
    summary = "The caller and their tenant",
    responses(
        (status = 200, description = "The signed-in user and, once their org is bound, the tenant with its projects and databases", body = Me),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn me<M, J>(State(state): State<Arc<AppState<M, J>>>, caller: Caller) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let tenant = match lookup_tenant(&state, &caller.org_id).await {
        Err(_) => return ApiError::internal().into_response(),
        Ok(None) => None,
        Ok(Some(tenant_id)) => match state.metadata.tenant_overview(tenant_id).await {
            Ok(None) => None,
            Ok(Some(overview)) => match main_branches(&state.metadata, overview.id).await {
                Ok(mains) => Some(Tenant::new(overview, &mains, &state.proxy_domain)),
                Err(err) => {
                    error!("main branches: {err}");
                    return ApiError::internal().into_response();
                }
            },
            Err(err) => {
                error!("tenant overview: {err}");
                return ApiError::internal().into_response();
            }
        },
    };
    Json(Me {
        user: MeUser {
            sub: caller.sub,
            org_id: caller.org_id,
        },
        tenant,
    })
    .into_response()
}

#[utoipa::path(
    post,
    path = "/v1/databases",
    operation_id = "createDatabase",
    tag = "databases",
    summary = "Create a database",
    description = "Returns 202 at once. The database is provisioned by a background job.",
    request_body = CreateDatabaseRequest,
    responses(
        (status = 202, description = "Accepted; the database is provisioned in the background (poll it until `ready`)", body = CreateDatabaseAccepted),
        (status = 400, description = "`invalid_request`: the name is missing or invalid, or the JSON is malformed", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant; `forbidden`: `tenant_id` isn't the caller's tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the tenant or project doesn't exist (another tenant's project answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`name_taken`: a database with this name exists in the project", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but a field is missing or has the wrong type", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker or job queue can't be reached", body = ErrorEnvelope),
    ),
)]
async fn create_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiJson(body): ApiJson<CreateDatabaseRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    if body.tenant_id != principal.tenant_id {
        return ApiError::new(
            403,
            ErrorCode::Forbidden,
            "tenant_id does not match authenticated Zitadel org binding",
        )
        .into_response();
    }

    if body.name.trim().is_empty() {
        return ApiError::invalid_request(400, "name is required").into_response();
    }
    if let Err(err) = cheladb_metadata::postgres_database_name(&body.name) {
        return ApiError::invalid_request(400, err.to_string()).into_response();
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
        return ApiError::new(
            503,
            ErrorCode::WorkerUnavailable,
            "failed to enqueue provision job",
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

#[utoipa::path(
    get,
    path = "/v1/databases/{id}",
    operation_id = "getDatabase",
    tag = "databases",
    summary = "Get a database",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The database with its settings and main endpoint", body = Database),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let database_id = DatabaseId(id);
    match state.metadata.get_database(database_id).await {
        // Tombstones are invisible through the API.
        Ok(Some(record)) if record.status == DatabaseStatus::Deleted => {
            ApiError::not_found("database not found").into_response()
        }
        Ok(Some(record)) => {
            // Another tenant's database answers exactly what a missing one does.
            if record.tenant_id != principal.tenant_id {
                return ApiError::not_found("database not found").into_response();
            }
            let branches = match state.metadata.list_branches(database_id).await {
                Ok(branches) => branches,
                Err(err) => {
                    error!("list branches: {err}");
                    return ApiError::internal().into_response();
                }
            };
            let count = branches.iter().filter(|b| b.kind == BranchKind::Normal).count() as i64;
            let main = branches.iter().find(|b| b.id == BranchId::from(database_id));
            (
                StatusCode::OK,
                Json(Database::new(record, main, count, &state.proxy_domain)),
            )
                .into_response()
        }
        Ok(None) => ApiError::not_found("database not found").into_response(),
        Err(err) => {
            error!("get_database: {err}");
            ApiError::internal().into_response()
        }
    }
}

/// The caller's own, non-deleted database, or the response to send instead.
async fn load_owned<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
) -> Result<DatabaseRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.get_database(DatabaseId(id)).await {
        Ok(Some(record)) if record.status == DatabaseStatus::Deleted => {
            Err(ApiError::not_found("database not found"))
        }
        Ok(Some(record)) if record.tenant_id != principal.tenant_id => {
            Err(ApiError::not_found("database not found"))
        }
        Ok(Some(record)) => Ok(record),
        Ok(None) => Err(ApiError::not_found("database not found")),
        Err(err) => {
            error!("load database: {err}");
            Err(ApiError::internal())
        }
    }
}

/// The database's main branch, if it has one.
async fn load_main<M, J>(
    state: &AppState<M, J>,
    database_id: DatabaseId,
) -> Result<Option<BranchRecord>, ApiError>
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
            ApiError::internal()
        })
}

/// The 202 body of the routes that start background work: the status the row moved to.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct OperationAccepted {
    pub(crate) status: &'static str,
}

/// The body of a successful password reset (nothing to return; the new password is read from
/// the connection details).
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct PasswordResetResult {}

/// The owner credentials of a branch.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ConnectionDetails {
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
#[utoipa::path(
    get,
    path = "/v1/databases/{id}/connection",
    operation_id = "getDatabaseConnection",
    tag = "databases",
    summary = "Reveal a database's connection details",
    description = "The owner credentials of the database's main branch. Never cached (`Cache-Control: no-store`) and never logged.",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The owner credentials; never cached", body = ConnectionDetails),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`database_not_ready`: the database isn't ready; `branch_not_ready`: its main isn't ready; `credentials_unavailable`: the owner credentials are missing", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored credentials can't be opened with the current secrets key; `internal`: unexpected", body = ErrorEnvelope),
    ),
)]
async fn database_connection<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    if record.status != DatabaseStatus::Ready {
        return ApiError::new(
            409,
            ErrorCode::DatabaseNotReady,
            format!(
                "database is {}; connection details are available once it is ready",
                record.status.as_str()
            ),
        )
        .into_response();
    }
    // Main holds the database's endpoint and sealed password (AAD = its id = the database's).
    let main = match load_main(&state, record.database_id).await {
        Ok(main) => main,
        Err(resp) => return resp.into_response(),
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
        return ApiError::new(
            409,
            ErrorCode::CredentialsUnavailable,
            "credentials unavailable; recreate this database",
        )
        .into_response();
    };
    if branch.kind == BranchKind::Backup {
        return ApiError::new(409, ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return ApiError::new(
            409,
            ErrorCode::BranchNotReady,
            format!(
                "branch is {}; connection details are available once it is ready",
                branch.status.as_str()
            ),
        )
        .into_response();
    }
    let sealed = branch.owner_password_enc.filter(|s| !s.is_empty());
    let (Some(endpoint_id), Some(database), Some(sealed)) =
        (branch.endpoint_id, record.database_name.clone(), sealed)
    else {
        return ApiError::new(
            409,
            ErrorCode::CredentialsUnavailable,
            "credentials unavailable; recreate this database",
        )
        .into_response();
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
            return ApiError::new(
                500,
                ErrorCode::CredentialsUnavailable,
                "stored credentials can't be decrypted with the current secrets key; \
                 restore the original CHELADB_SECRETS_KEY file",
            )
            .into_response();
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

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/reset-password",
    operation_id = "resetDatabasePassword",
    tag = "databases",
    summary = "Reset a database's owner password",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The password was reset; read the new one from the connection details", body = PasswordResetResult),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`database_not_ready`: the database isn't ready; `branch_not_ready`: the branch isn't ready; `credentials_unavailable`: the owner credentials are missing", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed to reset the password", body = ErrorEnvelope),
        (status = 503, description = "`not_applied`: the new password is stored but not applied yet and will be retried; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn reset_password<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    if record.status != DatabaseStatus::Ready {
        return ApiError::new(409, ErrorCode::DatabaseNotReady, "database is not ready")
            .into_response();
    }
    let main = match load_main(&state, record.database_id).await {
        Ok(main) => main,
        Err(resp) => return resp.into_response(),
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
        return ApiError::new(
            409,
            ErrorCode::CredentialsUnavailable,
            "credentials unavailable; recreate this database",
        )
        .into_response();
    };
    if branch.kind == BranchKind::Backup {
        return ApiError::new(409, ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return ApiError::new(409, ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    if branch.endpoint_id.is_none() {
        return ApiError::new(
            409,
            ErrorCode::CredentialsUnavailable,
            "credentials unavailable; recreate this database",
        )
        .into_response();
    }
    let (database_id, branch_id) = (record.database_id, branch.id);
    match state.js.reset_password(database_id, branch_id).await {
        Ok(reply) if reply.ok => {
            info!(request_id = %principal.request_id, %database_id, %branch_id, "owner password reset");
            (StatusCode::OK, Json(PasswordResetResult {})).into_response()
        }
        Ok(reply) if reply.error.as_deref() == Some("not ready") => {
            ApiError::new(409, ErrorCode::BranchNotReady, "branch is not ready").into_response()
        }
        Ok(reply) if reply.error.as_deref() == Some(RESET_NOT_APPLIED) => {
            ApiError::new(503, ErrorCode::NotApplied, RESET_NOT_APPLIED).into_response()
        }
        Ok(reply) if reply.error.as_deref() == Some("timed out") => ApiError::new(
            504,
            ErrorCode::Timeout,
            "the database did not respond; try again",
        )
        .into_response(),
        Ok(reply) => {
            error!(%database_id, %branch_id, "reset password failed: {:?}", reply.error);
            ApiError::new(500, ErrorCode::Internal, "password reset failed").into_response()
        }
        Err(ResetError::Timeout) => {
            ApiError::new(504, ErrorCode::Timeout, "worker did not respond; try again")
                .into_response()
        }
        Err(ResetError::Transport(err)) => {
            error!(%database_id, %branch_id, "reset password: {err}");
            ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable").into_response()
        }
    }
}

/// Start deleting a database (tombstone). The worker removes the container and
/// its data; a lost job is retried by the worker's reconcile loop.
#[utoipa::path(
    delete,
    path = "/v1/databases/{id}",
    operation_id = "deleteDatabase",
    tag = "databases",
    summary = "Delete a database",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 202, description = "Accepted; the database moves to `deleting` and is removed in the background", body = OperationAccepted),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`already_deleting`: the database is already being deleted", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    match state.metadata.begin_delete(record.database_id).await {
        Ok(true) => {}
        Ok(false) => {
            return ApiError::new(409, ErrorCode::AlreadyDeleting, "database is already being deleted")
                .into_response()
        }
        Err(err) => {
            error!("begin delete: {err}");
            return ApiError::internal().into_response();
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
        Json(OperationAccepted { status: "deleting" }),
    )
        .into_response()
}

fn map_metadata_error(err: MetadataError) -> Response {
    match err {
        MetadataError::TenantNotFound(_) | MetadataError::ProjectNotFound(_) => {
            ApiError::not_found(err.to_string()).into_response()
        }
        // Another tenant's project looks exactly like a missing one.
        MetadataError::ProjectTenantMismatch { project_id, .. } => {
            ApiError::not_found(MetadataError::ProjectNotFound(project_id).to_string())
                .into_response()
        }
        MetadataError::DuplicateDatabaseName => ApiError::new(
            409,
            ErrorCode::NameTaken,
            "database name already exists in project",
        )
        .into_response(),
        other => {
            error!("metadata error: {other}");
            ApiError::internal().into_response()
        }
    }
}
