//! Edge functions m3: the management API for a branch's functions. Deploy a bundle, list,
//! enable or disable, roll back, set how a function is called and the role it runs as, delete,
//! and manage its secrets (names come back, values never do).
//!
//! Every per-branch write needs Developer on an unprotected branch and Admin on a protected one
//! (`BRANCH` in `chelabase_routes`). A function's row is the database's, so deleting it and
//! changing its kind need Admin, and the metadata refuses both while a protected branch has it
//! active. Platform mode answers 403 on every route. After each write commits,
//! `chelabase.fn.changed.<branch_id>` is published (fire and forget) so the functions service
//! drops its cached resolution.
//!
//! The orchestrator holds no object storage credentials: a bundle's manifest goes to the
//! functions service's internal listener through a [`BundleSink`], byte for byte as uploaded
//! (the runtime hashes exactly those bytes), and the deployment row is written only after it
//! answered 2xx.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chelabase_metadata::{
    BranchFunction, DeploymentRecord, FunctionAuth, FunctionKind, FunctionRecord, MetadataClient,
    MetadataError, NewDeployment, RunAs,
};
use chelabase_types::{BranchId, DatabaseId};
use uuid::Uuid;

/// The metadata the functions routes read and write (`chelabase_metadata`'s functions API).
pub trait FunctionsPort: Send + Sync {
    fn create_function(
        &self,
        database: DatabaseId,
        name: &str,
        kind: FunctionKind,
    ) -> impl Future<Output = Result<FunctionRecord, MetadataError>> + Send;
    fn list_functions(
        &self,
        database: DatabaseId,
    ) -> impl Future<Output = Result<Vec<FunctionRecord>, MetadataError>> + Send;
    fn set_function_kind(
        &self,
        database: DatabaseId,
        function: Uuid,
        kind: FunctionKind,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn delete_function_unprotected(
        &self,
        database: DatabaseId,
        function: Uuid,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn create_deployment(
        &self,
        deployment: &NewDeployment<'_>,
    ) -> impl Future<Output = Result<DeploymentRecord, MetadataError>> + Send;
    fn list_deployments(
        &self,
        function: Uuid,
    ) -> impl Future<Output = Result<Vec<DeploymentRecord>, MetadataError>> + Send;
    fn set_active_deployment(
        &self,
        branch: BranchId,
        function: Uuid,
        deployment: Uuid,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn set_enabled(
        &self,
        branch: BranchId,
        function: Uuid,
        enabled: bool,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn set_function_auth(
        &self,
        branch: BranchId,
        function: Uuid,
        auth: FunctionAuth,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn set_run_as(
        &self,
        branch: BranchId,
        function: Uuid,
        run_as: Option<&RunAs>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn list_for_branch(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchFunction>, MetadataError>> + Send;
    fn put_secret(
        &self,
        branch: BranchId,
        function: Uuid,
        name: &str,
        value: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn delete_secret(
        &self,
        branch: BranchId,
        function: Uuid,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn secret_names(
        &self,
        branch: BranchId,
        function: Uuid,
    ) -> impl Future<Output = Result<Vec<String>, MetadataError>> + Send;
}

impl FunctionsPort for MetadataClient {
    async fn create_function(
        &self,
        database: DatabaseId,
        name: &str,
        kind: FunctionKind,
    ) -> Result<FunctionRecord, MetadataError> {
        MetadataClient::create_function(self, database, name, kind).await
    }
    async fn list_functions(
        &self,
        database: DatabaseId,
    ) -> Result<Vec<FunctionRecord>, MetadataError> {
        MetadataClient::list_functions(self, database).await
    }
    async fn set_function_kind(
        &self,
        database: DatabaseId,
        function: Uuid,
        kind: FunctionKind,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_function_kind(self, database, function, kind).await
    }
    async fn delete_function_unprotected(
        &self,
        database: DatabaseId,
        function: Uuid,
    ) -> Result<bool, MetadataError> {
        MetadataClient::delete_function_unprotected(self, database, function).await
    }
    async fn create_deployment(
        &self,
        deployment: &NewDeployment<'_>,
    ) -> Result<DeploymentRecord, MetadataError> {
        MetadataClient::create_deployment(self, deployment).await
    }
    async fn list_deployments(
        &self,
        function: Uuid,
    ) -> Result<Vec<DeploymentRecord>, MetadataError> {
        MetadataClient::list_deployments(self, function).await
    }
    async fn set_active_deployment(
        &self,
        branch: BranchId,
        function: Uuid,
        deployment: Uuid,
    ) -> Result<(), MetadataError> {
        MetadataClient::set_active_deployment(self, branch, function, deployment).await
    }
    async fn set_enabled(
        &self,
        branch: BranchId,
        function: Uuid,
        enabled: bool,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_enabled(self, branch, function, enabled).await
    }
    async fn set_function_auth(
        &self,
        branch: BranchId,
        function: Uuid,
        auth: FunctionAuth,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_function_auth(self, branch, function, auth).await
    }
    async fn set_run_as(
        &self,
        branch: BranchId,
        function: Uuid,
        run_as: Option<&RunAs>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_run_as(self, branch, function, run_as).await
    }
    async fn list_for_branch(
        &self,
        branch: BranchId,
    ) -> Result<Vec<BranchFunction>, MetadataError> {
        MetadataClient::list_for_branch(self, branch).await
    }
    async fn put_secret(
        &self,
        branch: BranchId,
        function: Uuid,
        name: &str,
        value: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::put_secret(self, branch, function, name, value).await
    }
    async fn delete_secret(
        &self,
        branch: BranchId,
        function: Uuid,
        name: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::delete_secret(self, branch, function, name).await
    }
    async fn secret_names(
        &self,
        branch: BranchId,
        function: Uuid,
    ) -> Result<Vec<String>, MetadataError> {
        MetadataClient::secret_names(self, branch, function).await
    }
}

/// Why the functions service didn't store a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkError {
    /// It answered this non-2xx status: 422 the manifest doesn't hash to the path's sha256, 404
    /// the database is unknown or being deleted, 413 the bundle is over 10 MiB.
    Status(u16),
    /// It couldn't be reached, or isn't configured.
    Unavailable(String),
}

pub type SinkFuture<'a> = Pin<Box<dyn Future<Output = Result<(), SinkError>> + Send + 'a>>;

/// Where a deployment's bundle goes before its row is written: the functions service's
/// `PUT /internal/bundles/{database_id}/{sha256}`, the manifest's bytes as the body.
pub trait BundleSink: Send + Sync {
    fn put<'a>(
        &'a self,
        database: DatabaseId,
        sha256: &'a str,
        manifest: bytes::Bytes,
    ) -> SinkFuture<'a>;
}

/// The functions service's internal listener (`CHELABASE_FUNCTIONS_INTERNAL_URL`), with the
/// orchestrator's bearer (`CHELABASE_ORCHESTRATOR_FUNCTIONS_TOKEN_FILE`).
pub struct HttpBundleSink {
    /// `http://host:port`, no path; `None` when unset (every upload is 503).
    url: Option<String>,
    token: String,
    http: chelabase_http::Client,
}

/// Longest wait for the functions service to store a bundle (up to 10 MiB over the stack's
/// network).
const SINK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl HttpBundleSink {
    pub fn new(url: Option<String>, token: String, http: chelabase_http::Client) -> Self {
        Self {
            url: url.map(|u| u.trim_end_matches('/').to_owned()),
            token,
            http,
        }
    }
}

impl BundleSink for HttpBundleSink {
    fn put<'a>(
        &'a self,
        database: DatabaseId,
        sha256: &'a str,
        manifest: bytes::Bytes,
    ) -> SinkFuture<'a> {
        Box::pin(async move {
            let Some(url) = &self.url else {
                return Err(SinkError::Unavailable(
                    "CHELABASE_FUNCTIONS_INTERNAL_URL is unset".into(),
                ));
            };
            let resp = self
                .http
                .put(format!("{url}/internal/bundles/{database}/{sha256}"))
                .bearer_auth(&self.token)
                .timeout(SINK_TIMEOUT)
                .body(manifest)
                .send()
                .await
                .map_err(|e| SinkError::Unavailable(e.to_string()))?;
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                Ok(())
            } else {
                Err(SinkError::Status(status))
            }
        })
    }
}

/// A shared sink.
pub type Bundles = Arc<dyn BundleSink>;

// --- The routes ------------------------------------------------------------------------------

use axum::{
    Json,
    body::Body,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_metadata::{BranchRecord, DatabaseRecord, MAX_BUNDLE_SIZE};
use chelabase_routes::orchestrator as ep;
use chelabase_types::ApiPath;
use chelabase_types::access::{Access, PlatformRole};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::data_key::{self, DataKeyKind};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::access::{Authorized, Target, authorize};
use crate::branches::{conflict, load_branch, parse_body, unprocessable};
use crate::data_keys::{checked_key_identity, present};
use crate::{ApiBytes, AppState, AuthPrincipal, JobPort, MetadataPort};

/// A deploy request's body may be at most 12 MiB (a 10 MiB manifest and its JSON around it).
pub const MAX_DEPLOY_BODY: usize = 12 * 1024 * 1024;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_functions))
        .routes(routes!(put_function, update_function, delete_function))
        .routes(routes!(create_deployment, list_deployments))
        .routes(routes!(list_secrets))
        .routes(routes!(put_secret, delete_secret))
}

/// `function` or `service` (services come with a later milestone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = FunctionKind)]
enum Kind {
    Function,
    Service,
}

impl From<FunctionKind> for Kind {
    fn from(k: FunctionKind) -> Self {
        match k {
            FunctionKind::Function => Kind::Function,
            FunctionKind::Service => Kind::Service,
        }
    }
}

impl From<Kind> for FunctionKind {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Function => FunctionKind::Function,
            Kind::Service => FunctionKind::Service,
        }
    }
}

/// Who may call the function: `key` (any data key), `app_user` (a signed-in app user, or a
/// secret key), `public` (anyone, on `/functions/v1/public/{branch_id}/{name}` only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = FunctionAuth)]
enum Auth {
    Key,
    AppUser,
    Public,
}

impl From<FunctionAuth> for Auth {
    fn from(a: FunctionAuth) -> Self {
        match a {
            FunctionAuth::Key => Auth::Key,
            FunctionAuth::AppUser => Auth::AppUser,
            FunctionAuth::Public => Auth::Public,
        }
    }
}

impl From<Auth> for FunctionAuth {
    fn from(a: Auth) -> Self {
        match a {
            Auth::Key => FunctionAuth::Key,
            Auth::AppUser => FunctionAuth::AppUser,
            Auth::Public => FunctionAuth::Public,
        }
    }
}

/// A function of the database, with its state on this branch (null fields when it has no
/// deployment active here).
#[derive(Debug, Serialize, ToSchema)]
struct Function {
    id: Uuid,
    name: String,
    kind: Kind,
    created_at: DateTime<Utc>,
    /// The deployment active on the branch.
    #[schema(required = true)]
    deployment_id: Option<Uuid>,
    enabled: bool,
    #[schema(required = true)]
    auth: Option<Auth>,
    /// The role the function runs as on the branch: its live run-as key's role (which follows
    /// a rename), else the role set; null for none.
    #[schema(required = true)]
    run_as_role: Option<String>,
    /// Whether a live function-managed key backs `run_as_role` on the branch.
    run_as_active: bool,
    /// The last invocation error on the branch.
    #[schema(required = true)]
    last_error: Option<String>,
    #[schema(required = true)]
    updated_at: Option<DateTime<Utc>>,
}

/// An immutable deployment of a function.
#[derive(Debug, Serialize, ToSchema)]
#[schema(as = FunctionDeployment)]
struct Deployment {
    id: Uuid,
    /// SHA-256 of the bundle's manifest, lowercase hex.
    #[schema(required = true)]
    bundle_sha256: Option<String>,
    #[schema(required = true)]
    bundle_size: Option<i64>,
    compatibility_date: NaiveDate,
    /// The member who deployed it.
    created_by: String,
    created_at: DateTime<Utc>,
    #[schema(required = true)]
    note: Option<String>,
}

impl From<DeploymentRecord> for Deployment {
    fn from(d: DeploymentRecord) -> Self {
        Deployment {
            id: d.id,
            bundle_sha256: d.bundle_sha256,
            bundle_size: d.bundle_size,
            compatibility_date: d.compatibility_date,
            created_by: d.created_by,
            created_at: d.created_at,
            note: d.note,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct PutFunctionRequest {
    /// Defaults to `function` for a new one. Changing an existing function's kind needs Admin,
    /// and only a function without deployments may change it.
    kind: Option<Kind>,
    /// Sets how the function is called on this branch (it must have a deployment here).
    auth: Option<Auth>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = UpdateFunctionRequest)]
struct PatchFunctionRequest {
    /// Turns the function on or off on the branch.
    enabled: Option<bool>,
    /// Makes one of the function's deployments the active one (a rollback, or a deploy made
    /// with `activate: false`).
    active_deployment_id: Option<Uuid>,
    auth: Option<Auth>,
    /// The role the function runs as: a function-managed secret key for it is made (and the
    /// previous one revoked); null clears both. A role with BYPASSRLS needs Admin.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    run_as_role: Option<Option<String>>,
}

/// A deployment: the bundle manifest (the runtime's format) and its hash.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = CreateFunctionDeploymentRequest)]
struct CreateDeploymentRequest {
    /// The bundle manifest, as the runtime loads it. Stored byte for byte as sent.
    #[schema(value_type = Object)]
    manifest: Box<serde_json::value::RawValue>,
    /// SHA-256 of the manifest's bytes as sent, lowercase hex.
    sha256: String,
    /// Defaults to the manifest's `compatibilityDate`.
    compatibility_date: Option<NaiveDate>,
    note: Option<String>,
    /// Make it the active deployment on the branch (default true). The function stays enabled
    /// or disabled as it was; a new one starts disabled.
    activate: Option<bool>,
}

/// A secret's value; it never comes back.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = PutFunctionSecretRequest)]
struct PutSecretRequest {
    /// At most 64 KiB.
    value: String,
}

impl std::fmt::Debug for PutSecretRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PutSecretRequest { value: <redacted> }")
    }
}

/// The checks every functions route starts with: functions are available (not in platform
/// mode), the caller passes the route's access on the branch, and the branch is the
/// database's.
pub(crate) async fn begin<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    id: Uuid,
    branch_id: Uuid,
) -> Result<(Authorized, DatabaseRecord, BranchRecord), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    if !state.policy.functions_available() {
        return Err(ApiError::new(
            403,
            ErrorCode::NotAvailableInPlatformMode,
            "edge functions are not available in platform mode",
        ));
    }
    let authorized = match authorize(
        state,
        principal,
        access,
        Target::Branch {
            database: id,
            branch: branch_id,
        },
    )
    .await
    {
        Ok(authorized) => authorized,
        // The access check counts a branch it can't place in the database as protected, so a
        // Developer would hear "needs admin" for another database's branch: a branch that isn't
        // this database's is 404, as a missing one (the caller is a member of the database).
        Err(err)
            if err.code == ErrorCode::Forbidden && !branch_of(state, id, branch_id).await? =>
        {
            return Err(ApiError::not_found("branch not found"));
        }
        Err(err) => return Err(err),
    };
    let record = authorized.clone().into_record()?;
    let branch = load_branch(state, &record, branch_id).await?;
    Ok((authorized, record, branch))
}

/// Whether `branch` is a branch of database `id` that isn't deleted.
async fn branch_of<M, J>(state: &AppState<M, J>, id: Uuid, branch: Uuid) -> Result<bool, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.get_branch(BranchId(branch)).await {
        Ok(b) => Ok(b.is_some_and(|b| {
            b.database_id == DatabaseId(id) && b.status != chelabase_metadata::BranchStatus::Deleted
        })),
        Err(err) => {
            error!("load branch: {err}");
            Err(ApiError::internal())
        }
    }
}

fn not_found() -> ApiError {
    ApiError::not_found("function not found")
}

/// 409 `conflict`, its `reason` in the body.
fn refused(reason: &str, message: &str) -> ApiError {
    conflict(ErrorCode::Conflict, message)
        .with_extra("reason", serde_json::Value::String(reason.into()))
}

/// A metadata error as the API answers it. Never echoes a value: the metadata's messages name
/// only names, sizes and hashes.
pub(crate) fn metadata_error(what: &str, err: MetadataError) -> ApiError {
    match err {
        MetadataError::FunctionNotFound => not_found(),
        MetadataError::InvalidFunction(message) => unprocessable(&message),
        MetadataError::FunctionActiveOnProtectedBranch => refused(
            "function_active_on_protected_branch",
            "the function is active on a protected branch; remove it there first",
        ),
        MetadataError::ManagedKey => refused("managed_key", "the key is managed by a function"),
        MetadataError::Conflict => {
            conflict(ErrorCode::Conflict, "a function with this name exists")
        }
        MetadataError::EndpointLockTimeout(_) => ApiError::new(
            503,
            ErrorCode::Unavailable,
            "the branch's roles are being changed; try again",
        )
        .retry_after(1),
        err => {
            error!("{what}: {err}");
            ApiError::internal()
        }
    }
}

/// The database's function `name` (404 when there's none).
pub(crate) async fn find<M, J>(
    state: &AppState<M, J>,
    record: &DatabaseRecord,
    name: &str,
) -> Result<FunctionRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    state
        .metadata
        .list_functions(record.database_id)
        .await
        .map_err(|e| metadata_error("list functions", e))?
        .into_iter()
        .find(|f| f.name == name)
        .ok_or_else(not_found)
}

/// Publishes `chelabase.fn.changed.<branch>` after a committed write; a failure is logged and
/// never fails the request (the functions service's cache expires within 30 s anyway).
async fn changed<M, J>(state: &AppState<M, J>, branch: BranchId, function: &str)
where
    M: MetadataPort,
    J: JobPort,
{
    if let Err(err) = state.js.publish_fn_changed(branch, function).await {
        warn!(branch_id = %branch, function, "publish fn changed failed: {err}");
    }
}

/// The database's functions with their state on `branch`.
async fn views<M, J>(
    state: &AppState<M, J>,
    record: &DatabaseRecord,
    branch: BranchId,
) -> Result<Vec<Function>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let functions = state
        .metadata
        .list_functions(record.database_id)
        .await
        .map_err(|e| metadata_error("list functions", e))?;
    let states = state
        .metadata
        .list_for_branch(branch)
        .await
        .map_err(|e| metadata_error("list branch functions", e))?;
    let keys = state
        .metadata
        .list_data_keys(branch)
        .await
        .map_err(|e| metadata_error("list data keys", e))?;
    Ok(functions
        .into_iter()
        .map(|f| {
            let st = states.iter().find(|s| s.function_id == f.id);
            // The live key the function runs as: its role follows a rename.
            let key = st.and_then(|s| s.run_as_key_id).and_then(|id| {
                keys.iter().find(|k| {
                    k.id == id
                        && k.managed_by_function
                            .as_ref()
                            .is_some_and(|(m, _)| *m == f.id)
                })
            });
            Function {
                id: f.id,
                name: f.name,
                kind: f.kind.into(),
                created_at: f.created_at,
                deployment_id: st.map(|s| s.deployment_id),
                enabled: st.is_some_and(|s| s.enabled),
                auth: st.map(|s| s.auth.into()),
                run_as_role: key
                    .map(|k| k.role.clone())
                    .or_else(|| st.and_then(|s| s.run_as_role.clone())),
                run_as_active: key.is_some(),
                last_error: st.and_then(|s| s.last_error.clone()),
                updated_at: st.map(|s| s.updated_at),
            }
        })
        .collect())
}

async fn view<M, J>(
    state: &AppState<M, J>,
    record: &DatabaseRecord,
    branch: BranchId,
    id: Uuid,
) -> Result<Function, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    views(state, record, branch)
        .await?
        .into_iter()
        .find(|f| f.id == id)
        .ok_or_else(not_found)
}

pub(crate) fn respond(result: Result<Response, ApiError>) -> Response {
    result.unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/functions",
    operation_id = "listFunctions",
    tag = "functions",
    summary = "List the database's functions with their state on the branch",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 200, description = "The database's functions by name; the state fields are null where the function has no deployment active on the branch", body = Vec<Function>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_functions<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(
        async {
            let (_, record, branch) = begin(
                &state,
                &principal,
                ep::LIST_FUNCTIONS.policy.access,
                id,
                branch_id,
            )
            .await?;
            Ok(Json(views(&state, &record, branch.id).await?).into_response())
        }
        .await,
    )
}

#[utoipa::path(
    put,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}",
    operation_id = "putFunction",
    tag = "functions",
    summary = "Create a function, or change its kind or its auth on the branch",
    description = "Creates the function when the database has none of this name (201), else changes what the body names (200). A name is a lowercase letter, then up to 62 of a-z, 0-9 and `-`; `public` is reserved (the keyless invoke prefix). Changing the kind needs Admin and is refused while a protected branch has the function active.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
    ),
    request_body = PutFunctionRequest,
    responses(
        (status = 200, description = "The function, changed as asked", body = Function),
        (status = 201, description = "The function, created", body = Function),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: `reason` `function_active_on_protected_branch` (a kind change), `not_deployed` (auth for a function with no deployment on the branch), or a concurrent create took the name", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a bad or reserved name, or a kind change for a function with deployments", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn put_function<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(async {
        let (authorized, record, branch) = begin(&state, &principal, ep::PUT_FUNCTION.policy.access, id, branch_id).await?;
        let request: PutFunctionRequest = parse_body(&body)?;
        if name == "public" {
            return Err(unprocessable("`public` is reserved: it's the keyless invoke prefix"));
        }
        if !chelabase_metadata::valid_function_name(&name) {
            return Err(unprocessable("a name is a lowercase letter, then up to 62 of a-z, 0-9 and -"));
        }
        let existing = state
            .metadata
            .list_functions(record.database_id)
            .await
            .map_err(|e| metadata_error("list functions", e))?
            .into_iter()
            .find(|f| f.name == name);
        let deployed_here = match &existing {
            Some(f) => state
                .metadata
                .list_for_branch(branch.id)
                .await
                .map_err(|e| metadata_error("list branch functions", e))?
                .iter()
                .any(|s| s.function_id == f.id),
            None => false,
        };
        if request.auth.is_some() && !deployed_here {
            return Err(refused("not_deployed", "deploy the function on this branch before setting its auth"));
        }
        let (function, created) = match existing {
            None => {
                let kind = request.kind.unwrap_or(Kind::Function).into();
                let f = state
                    .metadata
                    .create_function(record.database_id, &name, kind)
                    .await
                    .map_err(|e| metadata_error("create function", e))?;
                (f, true)
            }
            Some(f) => {
                if let Some(kind) = request.kind.map(FunctionKind::from).filter(|k| *k != f.kind) {
                    if !is_admin(&authorized) {
                        return Err(admin_needed());
                    }
                    match state.metadata.set_function_kind(record.database_id, f.id, kind).await {
                        Ok(true) => {}
                        Ok(false) => return Err(not_found()),
                        Err(e) => return Err(metadata_error("set function kind", e)),
                    }
                }
                (f, false)
            }
        };
        let mut wrote = created || request.kind.is_some_and(|k| FunctionKind::from(k) != function.kind);
        if let Some(auth) = request.auth {
            match state.metadata.set_function_auth(branch.id, function.id, auth.into()).await {
                Ok(true) => wrote = true,
                Ok(false) => return Err(refused("not_deployed", "deploy the function on this branch before setting its auth")),
                Err(e) => return Err(metadata_error("set function auth", e)),
            }
        }
        if wrote {
            changed(&state, branch.id, &name).await;
            info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, created, "function written");
        }
        let status = if created { StatusCode::CREATED } else { StatusCode::OK };
        Ok((status, Json(view(&state, &record, branch.id, function.id).await?)).into_response())
    }
    .await)
}

fn is_admin(authorized: &Authorized) -> bool {
    authorized
        .grant
        .as_ref()
        .is_some_and(|g| g.platform_role == PlatformRole::Admin)
}

fn admin_needed() -> ApiError {
    crate::access::deny_to_api(chelabase_types::access::Deny::Forbidden {
        required: PlatformRole::Admin.as_str().to_string(),
    })
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}",
    operation_id = "updateFunction",
    tag = "functions",
    summary = "Enable, disable, roll back, or set the auth or run-as role on the branch",
    description = "Applies the named fields in this order: `active_deployment_id`, `enabled`, `auth`, `run_as_role`. The run-as role is checked first (as for a secret key: you must be able to use the role; BYPASSRLS needs Admin), before anything is written.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
    ),
    request_body = PatchFunctionRequest,
    responses(
        (status = 200, description = "The function on the branch", body = Function),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`conflict` with `reason` `not_deployed`: the function has no deployment on the branch; `backup_branch` or `branch_not_ready` for a run-as role", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a deployment of another function; `role_refused`: the worker refused the run-as role; `exposes_data_api`: the role reaches an \"all rows\" role", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable` or `unavailable`: the run-as role couldn't be checked; nothing was written", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn update_function<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(async {
        let (authorized, record, branch) = begin(&state, &principal, ep::UPDATE_FUNCTION.policy.access, id, branch_id).await?;
        let request: PatchFunctionRequest = parse_body(&body)?;
        let function = find(&state, &record, &name).await?;
        // The run-as role's checks and the worker's answer, before any write.
        let run_as = match &request.run_as_role {
            Some(Some(role)) if role.trim().is_empty() => {
                return Err(unprocessable("run_as_role must not be empty (null clears it)"));
            }
            Some(Some(role)) => {
                let (oid, timeline, bypass) =
                    checked_key_identity(&state, &authorized, &record, &branch, role, &format!("function {name:?}'s run-as role {role:?}")).await?;
                if bypass && !is_admin(&authorized) {
                    return Err(admin_needed());
                }
                let Some(oid) = oid else {
                    error!(branch_id = %branch.id, "the worker's role check gave no OID");
                    return Err(ApiError::new(503, ErrorCode::WorkerUnavailable, "the worker couldn't identify the role; try again"));
                };
                let generated = data_key::generate(DataKeyKind::Secret);
                Some(Some(RunAs {
                    role: role.clone(),
                    role_oid: oid,
                    role_timeline: timeline,
                    role_bypass_rls: bypass,
                    key_id: Uuid::new_v4(),
                    prefix: generated.prefix,
                    sha256: generated.sha256,
                    secret: generated.key,
                }))
            }
            Some(None) => Some(None),
            None => None,
        };
        let mut wrote = false;
        let result = apply_patch(&state, branch.id, function.id, &request, run_as.as_ref(), &mut wrote).await;
        if wrote {
            changed(&state, branch.id, &name).await;
            info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, "function updated");
        }
        result?;
        Ok(Json(view(&state, &record, branch.id, function.id).await?).into_response())
    }
    .await)
}

/// The PATCH's writes, in order; `wrote` tells which committed (a later one may fail).
async fn apply_patch<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    function: Uuid,
    request: &PatchFunctionRequest,
    run_as: Option<&Option<RunAs>>,
    wrote: &mut bool,
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let not_deployed = || {
        refused(
            "not_deployed",
            "the function has no deployment on this branch",
        )
    };
    if let Some(deployment) = request.active_deployment_id {
        state
            .metadata
            .set_active_deployment(branch, function, deployment)
            .await
            .map_err(|e| metadata_error("set active deployment", e))?;
        *wrote = true;
    }
    if let Some(enabled) = request.enabled {
        match state
            .metadata
            .set_enabled(branch, function, enabled)
            .await
            .map_err(|e| metadata_error("set enabled", e))?
        {
            true => *wrote = true,
            false => return Err(not_deployed()),
        }
    }
    if let Some(auth) = request.auth {
        match state
            .metadata
            .set_function_auth(branch, function, auth.into())
            .await
            .map_err(|e| metadata_error("set auth", e))?
        {
            true => *wrote = true,
            false => return Err(not_deployed()),
        }
    }
    if let Some(run_as) = run_as {
        match state
            .metadata
            .set_run_as(branch, function, run_as.as_ref())
            .await
            .map_err(|e| metadata_error("set run-as", e))?
        {
            true => *wrote = true,
            false => return Err(not_deployed()),
        }
    }
    Ok(())
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}",
    operation_id = "deleteFunction",
    tag = "functions",
    summary = "Delete a function from the database",
    description = "Deletes the function with its deployments and every branch's state and secrets, and revokes its run-as keys. A function is the database's, so this needs Admin, and it's refused while a protected branch has the function active.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id (any branch of the database)"),
        ("name" = String, Path, description = "Function name"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`conflict` with `reason` `function_active_on_protected_branch`: nothing was deleted", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_function<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(async {
        let (_, record, _) = begin(&state, &principal, ep::DELETE_FUNCTION.policy.access, id, branch_id).await?;
        let function = find(&state, &record, &name).await?;
        let branches = state.metadata.list_branches(record.database_id).await.map_err(|e| metadata_error("list branches", e))?;
        match state.metadata.delete_function_unprotected(record.database_id, function.id).await {
            Ok(true) => {}
            Ok(false) => return Err(not_found()),
            Err(e) => return Err(metadata_error("delete function", e)),
        }
        // Every branch may have served it.
        for b in branches {
            changed(&state, b.id, &name).await;
        }
        info!(request_id = %principal.request_id, database_id = %record.database_id, function = %name, "function deleted");
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await)
}

/// The request body, at most `cap` bytes: over it is 413, a body that breaks off is 400.
async fn read_capped(body: Body, cap: usize) -> Result<bytes::Bytes, ApiError> {
    use futures_util::StreamExt;
    let mut stream = body.into_data_stream();
    let mut out = bytes::BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|_| ApiError::invalid_request(400, "the request body couldn't be read"))?;
        if out.len() + chunk.len() > cap {
            return Err(ApiError::new(
                413,
                ErrorCode::TooLarge,
                format!("the body is over the {cap}-byte limit"),
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out.freeze())
}

/// A sink refusal as the API answers it.
fn sink_error(err: SinkError) -> ApiError {
    match err {
        SinkError::Status(422) => unprocessable("the manifest doesn't hash to sha256"),
        SinkError::Status(404) => ApiError::not_found("database not found"),
        SinkError::Status(413) => ApiError::new(
            413,
            ErrorCode::TooLarge,
            format!("a bundle is at most {MAX_BUNDLE_SIZE} bytes"),
        ),
        SinkError::Status(code) if (500..600).contains(&code) => {
            error!("the functions service answered {code} to a bundle upload");
            ApiError::new(
                code,
                ErrorCode::Unavailable,
                "the functions service couldn't store the bundle; try again",
            )
        }
        SinkError::Status(code) => {
            error!("the functions service answered {code} to a bundle upload");
            ApiError::new(
                502,
                ErrorCode::Unavailable,
                "the functions service refused the bundle",
            )
        }
        SinkError::Unavailable(err) => {
            error!("bundle upload: {err}");
            ApiError::new(
                503,
                ErrorCode::Unavailable,
                "the functions service is unavailable; try again",
            )
        }
    }
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/deployments",
    operation_id = "createFunctionDeployment",
    tag = "functions",
    summary = "Deploy a bundle",
    description = "Stores the manifest (byte for byte as sent, at most 10 MiB; the body at most 12 MiB) with the functions service, then records the deployment and, unless `activate` is false, makes it the active one on the branch. The function's enabled state doesn't change.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
    ),
    request_body = CreateDeploymentRequest,
    responses(
        (status = 201, description = "The deployment", body = Deployment),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over 12 MiB or the manifest over 10 MiB", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the manifest doesn't hash to `sha256`, no compatibility date, or the function is a service", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 502, description = "`unavailable`: the functions service refused the bundle", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the functions service is unavailable; nothing was recorded", body = ErrorEnvelope),
    ),
)]
async fn create_deployment<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
    body: Body,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(async {
        let (_, record, branch) = begin(&state, &principal, ep::CREATE_FUNCTION_DEPLOYMENT.policy.access, id, branch_id).await?;
        // The cap applies before anything is parsed.
        let body = read_capped(body, MAX_DEPLOY_BODY).await?;
        let request: CreateDeploymentRequest = parse_body(&body)?;
        let manifest = request.manifest.get().as_bytes();
        let parsed: serde_json::Value = serde_json::from_slice(manifest)
            .map_err(|_| unprocessable("the manifest isn't valid JSON"))?;
        if !parsed.is_object() {
            return Err(unprocessable("the manifest is a JSON object"));
        }
        if manifest.len() as i64 > MAX_BUNDLE_SIZE {
            return Err(ApiError::new(413, ErrorCode::TooLarge, format!("a bundle is at most {MAX_BUNDLE_SIZE} bytes")));
        }
        if request.sha256.len() != 64 || !request.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(unprocessable("sha256 is 64 lowercase hex characters"));
        }
        if hex::encode(Sha256::digest(manifest)) != request.sha256 {
            return Err(unprocessable("the manifest doesn't hash to sha256"));
        }
        let compatibility_date = match request.compatibility_date {
            Some(date) => date,
            None => parsed
                .get("compatibilityDate")
                .and_then(|d| d.as_str()?.parse::<NaiveDate>().ok())
                .ok_or_else(|| unprocessable("no compatibility_date, and the manifest has no compatibilityDate"))?,
        };
        let function = find(&state, &record, &name).await?;
        if function.kind != FunctionKind::Function {
            return Err(unprocessable("a service has no bundle"));
        }
        state
            .bundles
            .put(record.database_id, &request.sha256, bytes::Bytes::copy_from_slice(manifest))
            .await
            .map_err(sink_error)?;
        // The upload may take a while: the branch's protection is checked again before anything
        // is recorded (an Admin may have protected it meanwhile).
        begin(&state, &principal, ep::CREATE_FUNCTION_DEPLOYMENT.policy.access, id, branch_id).await?;
        let deployment = state
            .metadata
            .create_deployment(&NewDeployment {
                id: Uuid::new_v4(),
                function_id: function.id,
                bundle_sha256: Some(&request.sha256),
                bundle_size: Some(manifest.len() as i64),
                spec: &serde_json::json!({}),
                compatibility_date,
                created_by: &principal.sub,
                note: request.note.as_deref(),
            })
            .await
            .map_err(|e| metadata_error("create deployment", e))?;
        if request.activate.unwrap_or(true) {
            state
                .metadata
                .set_active_deployment(branch.id, function.id, deployment.id)
                .await
                .map_err(|e| metadata_error("activate deployment", e))?;
        }
        // A new deployment, active or not: listings change.
        changed(&state, branch.id, &name).await;
        info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, deployment_id = %deployment.id, "function deployed");
        Ok((StatusCode::CREATED, Json(Deployment::from(deployment))).into_response())
    }
    .await)
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/deployments",
    operation_id = "listFunctionDeployments",
    tag = "functions",
    summary = "List a function's deployments",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
    ),
    responses(
        (status = 200, description = "The function's deployments, newest first", body = Vec<Deployment>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_deployments<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(
        async {
            let (_, record, _) = begin(
                &state,
                &principal,
                ep::LIST_FUNCTION_DEPLOYMENTS.policy.access,
                id,
                branch_id,
            )
            .await?;
            let function = find(&state, &record, &name).await?;
            let list = state
                .metadata
                .list_deployments(function.id)
                .await
                .map_err(|e| metadata_error("list deployments", e))?;
            Ok(Json(list.into_iter().map(Deployment::from).collect::<Vec<_>>()).into_response())
        }
        .await,
    )
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/secrets",
    operation_id = "listFunctionSecrets",
    tag = "functions",
    summary = "List the names of a function's secrets on the branch",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
    ),
    responses(
        (status = 200, description = "The secrets' names, sorted; values never come back", body = Vec<String>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_secrets<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(
        async {
            let (_, record, branch) = begin(
                &state,
                &principal,
                ep::LIST_FUNCTION_SECRETS.policy.access,
                id,
                branch_id,
            )
            .await?;
            let function = find(&state, &record, &name).await?;
            let names = state
                .metadata
                .secret_names(branch.id, function.id)
                .await
                .map_err(|e| metadata_error("secret names", e))?;
            Ok(Json(names).into_response())
        }
        .await,
    )
}

#[utoipa::path(
    put,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/secrets/{secret}",
    operation_id = "putFunctionSecret",
    tag = "functions",
    summary = "Set a function's secret on the branch",
    description = "The function sees it in `env`. A name is a letter or `_`, then up to 127 of A-Z, a-z, 0-9 and `_`; a value is at most 64 KiB. The value is never returned, logged or echoed in an error.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
        ("secret" = String, Path, description = "Secret name"),
    ),
    request_body = PutSecretRequest,
    responses(
        (status = 204, description = "Set"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't `{\"value\": string}`", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a bad name, or a value over 64 KiB", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn put_secret<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name, secret)): ApiPath<(Uuid, Uuid, String, String)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(async {
        let (_, record, branch) = begin(&state, &principal, ep::PUT_FUNCTION_SECRET.policy.access, id, branch_id).await?;
        // A fixed message: serde's could quote the value.
        let request: PutSecretRequest = serde_json::from_slice(&body)
            .map_err(|_| ApiError::invalid_request(400, "the body must be {\"value\": string}"))?;
        let function = find(&state, &record, &name).await?;
        state
            .metadata
            .put_secret(branch.id, function.id, &secret, &request.value)
            .await
            .map_err(|e| metadata_error("put secret", e))?;
        changed(&state, branch.id, &name).await;
        info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, secret = %secret, "function secret set");
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await)
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/secrets/{secret}",
    operation_id = "deleteFunctionSecret",
    tag = "functions",
    summary = "Delete a function's secret on the branch",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
        ("secret" = String, Path, description = "Secret name"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the function or the secret doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_secret<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name, secret)): ApiPath<(Uuid, Uuid, String, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(
        async {
            let (_, record, branch) = begin(
                &state,
                &principal,
                ep::DELETE_FUNCTION_SECRET.policy.access,
                id,
                branch_id,
            )
            .await?;
            let function = find(&state, &record, &name).await?;
            match state
                .metadata
                .delete_secret(branch.id, function.id, &secret)
                .await
            {
                Ok(true) => {}
                Ok(false) => return Err(ApiError::not_found("secret not found")),
                Err(e) => return Err(metadata_error("delete secret", e)),
            }
            changed(&state, branch.id, &name).await;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        .await,
    )
}
