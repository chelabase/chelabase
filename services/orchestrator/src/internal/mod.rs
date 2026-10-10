//! The internal listener: machine-to-machine routes for Neon's proxy, compute_ctl and the
//! storage controller. It binds `CHELABASE_INTERNAL_ADDR` (loopback by default) and never goes
//! through the gateway. Each caller has its own bearer token and can open only its own routes.

pub mod app;
pub mod config;
mod data;
mod ddl;
mod hooks;
mod invite;
mod proxy;
mod signup;
mod spec;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use chelabase_metadata::{
    AppSession, BranchAuthSync, BranchConnectionRow, BranchRecord, DataKeyLookup, MetadataClient,
    MetadataError, NeonDatabase, NeonEndpoint, NeonRole, NewAppSession, ProjectAuth,
    ProjectRecord, RefreshNext, RefreshOutcome,
};
use chelabase_types::{DatabaseId, ProjectId};
use chrono::{DateTime, Utc};
use chelabase_secrets::SecretKey;
use chelabase_types::BranchId;
use uuid::Uuid;
use chelabase_neon::{ComputeLocator, ComputeSigner, StorconClient};
pub use ddl::{BoxFuture, CachePort, DdlPort, ResyncPort};
use chelabase_types::role_settings::RoleSettings;
use std::future::Future;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tower_http::trace::TraceLayer;

/// The DDL receiver's body limit (T5): a forwarded delta is a few hundred bytes.
pub(crate) const DDL_BODY_LIMIT: usize = 64 * 1024;

/// The metadata the internal routes read. Abstracted so tests can use an in-memory fake.
pub trait NeonMetadataPort: Send + Sync + 'static {
    /// The DDL receiver's transaction.
    type Ddl: DdlPort;

    /// Opens the DDL receiver's transaction under the endpoint's row lock (`None` for an
    /// unknown endpoint; a lock timeout is `MetadataError::EndpointLockTimeout`).
    fn begin_ddl(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Option<Self::Ddl>, MetadataError>> + Send;
    fn neon_endpoint(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<NeonEndpoint>, MetadataError>> + Send;
    /// The live branch linked to the endpoint, read without its lock.
    fn endpoint_branch(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Option<BranchId>, MetadataError>> + Send;
    fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> impl Future<Output = Result<Option<Option<String>>, MetadataError>> + Send;
    /// [`NeonMetadataPort::neon_endpoint`] for the proxy: an unroutable endpoint is `None`.
    fn routable_endpoint(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<NeonEndpoint>, MetadataError>> + Send;
    /// [`NeonMetadataPort::neon_role_secret`] for the proxy: an unroutable endpoint is `None`.
    fn routable_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> impl Future<Output = Result<Option<Option<String>>, MetadataError>> + Send;
    fn neon_roles(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Vec<NeonRole>, MetadataError>> + Send;
    fn neon_databases(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Vec<NeonDatabase>, MetadataError>> + Send;
    fn neon_endpoints_for_tenant(
        &self,
        tenant_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, MetadataError>> + Send;
    fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, MetadataError>> + Send;

    /// The live key with this hash, if its branch isn't deleting or deleted.
    fn lookup_data_key(
        &self,
        sha256: &[u8; 32],
    ) -> impl Future<Output = Result<Option<DataKeyLookup>, MetadataError>> + Send;

    /// Stamps the key's `last_used_at` (at most once a minute).
    fn touch_data_key(
        &self,
        key_id: Uuid,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    /// What the REST service needs to connect to a branch, whatever its status.
    fn branch_connection(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Option<BranchConnectionRow>, MetadataError>> + Send;

    // App users (step 6b): what the `auth` service, the gateway and Zitadel's webhook need.

    fn project_by_id(
        &self,
        id: ProjectId,
    ) -> impl Future<Output = Result<Option<ProjectRecord>, MetadataError>> + Send;
    fn project_auth(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Option<ProjectAuth>, MetadataError>> + Send;
    fn project_auth_by_org(
        &self,
        org_id: &str,
    ) -> impl Future<Output = Result<Option<ProjectAuth>, MetadataError>> + Send;
    fn get_branch(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<Option<BranchRecord>, MetadataError>> + Send;
    /// The project the database belongs to, deleted or not.
    fn database_project(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Option<ProjectId>, MetadataError>> + Send;
    /// The database's main branch, unless it's deleted.
    fn main_branch(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Option<BranchRecord>, MetadataError>> + Send;
    fn branch_auth_sync(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Option<BranchAuthSync>, MetadataError>> + Send;
    fn insert_app_session(
        &self,
        s: &NewAppSession<'_>,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn app_session(
        &self,
        id: Uuid,
    ) -> impl Future<Output = Result<Option<AppSession>, MetadataError>> + Send;
    fn refresh_app_session(
        &self,
        id: Uuid,
        project: ProjectId,
        presented: [u8; 32],
        next: &RefreshNext,
    ) -> impl Future<Output = Result<RefreshOutcome, MetadataError>> + Send;
    fn revoke_app_session(
        &self,
        id: Uuid,
        project: ProjectId,
    ) -> impl Future<Output = Result<Option<AppSession>, MetadataError>> + Send;
    fn revoke_app_session_by_hash(
        &self,
        id: Uuid,
        project: ProjectId,
        presented: [u8; 32],
    ) -> impl Future<Output = Result<Option<AppSession>, MetadataError>> + Send;
    fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
        keep: Option<Uuid>,
        created_before: Option<DateTime<Utc>>,
    ) -> impl Future<Output = Result<Vec<AppSession>, MetadataError>> + Send;

    /// What the sign-up gate needs for the tenant of this Zitadel org; `None` when no tenant
    /// has it.
    fn signup_facts(
        &self,
        zitadel_org_id: &str,
    ) -> impl Future<Output = Result<Option<SignUpFacts>, MetadataError>> + Send;

    /// What the open invite of this Zitadel user shows (`None` when it has none).
    fn open_invite_for_user(
        &self,
        zitadel_user_id: &str,
    ) -> impl Future<Output = Result<Option<chelabase_metadata::members::InviteDescription>, MetadataError>> + Send;
}

/// A tenant's sign-up state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignUpFacts {
    /// The tenant's open sign-up switch.
    pub open: bool,
    /// Whether it has an invited or active member.
    pub has_members: bool,
}

// As for `MetadataPort`: each method must call `MetadataClient`'s inherent method by its full
// path, or a rename would make it call itself.
impl NeonMetadataPort for MetadataClient {
    type Ddl = chelabase_metadata::DdlTx;

    async fn begin_ddl(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<chelabase_metadata::DdlTx>, MetadataError> {
        MetadataClient::begin_ddl(self, endpoint_id).await
    }
    async fn neon_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        MetadataClient::neon_endpoint(self, id).await
    }
    async fn endpoint_branch(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<BranchId>, MetadataError> {
        MetadataClient::endpoint_branch(self, endpoint_id).await
    }
    async fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> Result<Option<Option<String>>, MetadataError> {
        MetadataClient::neon_role_secret(self, endpoint_id, role).await
    }
    async fn routable_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        MetadataClient::routable_endpoint(self, id).await
    }
    async fn routable_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> Result<Option<Option<String>>, MetadataError> {
        MetadataClient::routable_role_secret(self, endpoint_id, role).await
    }
    async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        MetadataClient::neon_roles(self, endpoint_id).await
    }
    async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        MetadataClient::neon_databases(self, endpoint_id).await
    }
    async fn neon_endpoints_for_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        MetadataClient::neon_endpoints_for_tenant(self, tenant_id).await
    }
    async fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        MetadataClient::neon_endpoints_for_timeline(self, timeline_id).await
    }
    async fn lookup_data_key(
        &self,
        sha256: &[u8; 32],
    ) -> Result<Option<DataKeyLookup>, MetadataError> {
        MetadataClient::lookup_data_key(self, sha256).await
    }
    async fn touch_data_key(&self, key_id: Uuid) -> Result<(), MetadataError> {
        MetadataClient::touch_data_key(self, key_id).await
    }
    async fn branch_connection(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchConnectionRow>, MetadataError> {
        MetadataClient::branch_connection(self, branch).await
    }
    async fn project_by_id(&self, id: ProjectId) -> Result<Option<ProjectRecord>, MetadataError> {
        MetadataClient::project_by_id(self, id).await
    }
    async fn project_auth(&self, project: ProjectId) -> Result<Option<ProjectAuth>, MetadataError> {
        MetadataClient::project_auth(self, project).await
    }
    async fn project_auth_by_org(&self, org_id: &str) -> Result<Option<ProjectAuth>, MetadataError> {
        MetadataClient::project_auth_by_org(self, org_id).await
    }
    async fn get_branch(&self, id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        MetadataClient::get_branch(self, id).await
    }
    async fn database_project(&self, db: DatabaseId) -> Result<Option<ProjectId>, MetadataError> {
        Ok(MetadataClient::get_database(self, db)
            .await?
            .map(|d| d.project_id))
    }
    async fn main_branch(&self, db: DatabaseId) -> Result<Option<BranchRecord>, MetadataError> {
        Ok(MetadataClient::list_branches(self, db)
            .await?
            .into_iter()
            .find(|b| b.parent_branch_id.is_none()))
    }
    async fn branch_auth_sync(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchAuthSync>, MetadataError> {
        MetadataClient::branch_auth_sync(self, branch).await
    }
    async fn insert_app_session(&self, s: &NewAppSession<'_>) -> Result<(), MetadataError> {
        MetadataClient::insert_app_session(self, s).await
    }
    async fn app_session(&self, id: Uuid) -> Result<Option<AppSession>, MetadataError> {
        MetadataClient::app_session(self, id).await
    }
    async fn refresh_app_session(
        &self,
        id: Uuid,
        project: ProjectId,
        presented: [u8; 32],
        next: &RefreshNext,
    ) -> Result<RefreshOutcome, MetadataError> {
        MetadataClient::refresh_app_session(self, id, project, presented, next).await
    }
    async fn revoke_app_session(
        &self,
        id: Uuid,
        project: ProjectId,
    ) -> Result<Option<AppSession>, MetadataError> {
        MetadataClient::revoke_app_session(self, id, project).await
    }
    async fn revoke_app_session_by_hash(
        &self,
        id: Uuid,
        project: ProjectId,
        presented: [u8; 32],
    ) -> Result<Option<AppSession>, MetadataError> {
        MetadataClient::revoke_app_session_by_hash(self, id, project, presented).await
    }
    async fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
        keep: Option<Uuid>,
        created_before: Option<DateTime<Utc>>,
    ) -> Result<Vec<AppSession>, MetadataError> {
        MetadataClient::revoke_user_sessions(self, project, user, keep, created_before).await
    }
    async fn open_invite_for_user(
        &self,
        zitadel_user_id: &str,
    ) -> Result<Option<chelabase_metadata::members::InviteDescription>, MetadataError> {
        MetadataClient::open_invite_for_user(self, zitadel_user_id).await
    }
    async fn signup_facts(&self, zitadel_org_id: &str) -> Result<Option<SignUpFacts>, MetadataError> {
        let Some(tenant) = MetadataClient::get_tenant_by_zitadel_org_id(self, zitadel_org_id).await? else {
            return Ok(None);
        };
        Ok(Some(SignUpFacts {
            open: MetadataClient::open_signup(self, tenant).await?,
            has_members: MetadataClient::tenant_has_members(self, tenant).await?,
        }))
    }
}

pub struct InternalState<M, L> {
    pub metadata: M,
    pub locator: L,
    pub storcon: StorconClient,
    /// Signs the tokens compute_ctl accepts, and publishes their JWKS in each spec.
    pub signer: ComputeSigner,
    /// For compute_ctl's `/configure`.
    pub http: reqwest::Client,
    pub proxy_token: String,
    pub storcon_token: String,
    /// The gateway's token for key lookups.
    pub data_keys_token: String,
    /// The REST service's token for connection info.
    pub rest_token: String,
    /// Opens the `authenticator` passwords handed to the REST service, and re-seals an API
    /// role's password when SQL renames it.
    pub secrets: SecretKey,
    /// Caps and password rules the DDL receiver applies.
    pub roles: RoleSettings,
    /// Tells the proxies a role's secret changed.
    pub cache: Arc<dyn CachePort>,
    /// Asks the worker to resync a branch whose roles a DDL touched.
    pub resync: Arc<dyn ResyncPort>,
    /// `CHELABASE_CONTROL_PLANE_URI`: the base of `neon.console_url` in the specs served.
    pub control_plane_uri: String,
    /// The REST service's internal listener: told when a forwarded `DROP`/`RENAME DATABASE`
    /// changes which databases a branch serves.
    pub rest: crate::RestInternal,
    /// The `auth` service's bearer for the app routes.
    pub app_auth_token: String,
    /// The gateway's bearer: a session's liveness only.
    pub app_sessions_token: String,
    /// How much longer a refresh keeps an app session alive (capped at 90 days by creation).
    pub app_refresh_ttl: std::time::Duration,
    /// Checks the Zitadel Actions webhook; `None` makes it answer 503.
    pub actions_signing_key: Option<String>,
    /// Admin access to Zitadel, to delete its sessions (best effort); `None` skips that.
    pub zitadel: Option<chelabase_zitadel::client::Zitadel>,
    /// Publishes app-user sync jobs.
    pub app_jobs: Arc<dyn app::AppJobPort>,
    /// What the mode allows (the sign-up gate answers `platform` when it refuses).
    pub policy: chelabase_types::mode_policy::ModePolicy,
}

pub fn build_internal_router<M, L>(state: Arc<InternalState<M, L>>) -> Router
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let proxy_token: Arc<str> = state.proxy_token.as_str().into();
    let proxy = Router::new()
        .route(
            "/proxy/api/v1/get_endpoint_access_control",
            get(proxy::access_control::<M, L>),
        )
        .route(
            "/proxy/api/v1/wake_compute",
            get(proxy::wake_compute::<M, L>),
        )
        .route_layer(middleware::from_fn_with_state(proxy_token, require_bearer));
    let storcon_token: Arc<str> = state.storcon_token.as_str().into();
    let storcon = Router::new()
        .route("/storcon/notify-attach", put(hooks::notify_attach::<M, L>))
        .route(
            "/storcon/notify-safekeepers",
            put(hooks::notify_safekeepers::<M, L>),
        )
        .route_layer(middleware::from_fn_with_state(
            storcon_token,
            require_bearer,
        ));
    let data_keys_token: Arc<str> = state.data_keys_token.as_str().into();
    let data_keys = Router::new()
        .route("/internal/data/keys/{sha256}", get(data::key_lookup::<M, L>))
        .route_layer(middleware::from_fn_with_state(
            data_keys_token,
            require_bearer,
        ));
    let signup_token: Arc<str> = state.app_auth_token.as_str().into();
    let signup = Router::new()
        .route("/internal/signup/allowed", get(signup::allowed::<M, L>))
        .route("/internal/invites/describe", post(invite::describe::<M, L>))
        .route_layer(middleware::from_fn_with_state(signup_token, require_bearer));
    let rest_token: Arc<str> = state.rest_token.as_str().into();
    let rest = Router::new()
        .route(
            "/internal/data/branches/{id}/connection",
            get(data::branch_connection::<M, L>),
        )
        .route_layer(middleware::from_fn_with_state(rest_token, require_bearer));
    Router::new()
        .route("/health", get(health))
        .route(
            "/compute/api/v2/computes/{id}/spec",
            get(spec::compute_spec::<M, L>),
        )
        .route(
            "/compute/api/v2/computes/{id}/ddl",
            // Reachable from every compute's network; the handler buffers the body (up to
            // `DDL_BODY_LIMIT`) only after the compute token checks out.
            patch(ddl::receive::<M, L>),
        )
        .merge(proxy)
        .merge(storcon)
        .merge(data_keys)
        .merge(rest)
        .merge(signup)
        .merge(app::routes(&state))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "orchestrator-internal" }))
}

/// The bearer token from `Authorization`, if any.
pub(crate) fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Runs before extractors, so a caller without the token learns nothing about the body format.
async fn require_bearer(State(token): State<Arc<str>>, request: Request, next: Next) -> Response {
    match bearer(request.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), token.as_bytes()) => next.run(request).await,
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Like [`require_bearer`], for routes two callers share.
async fn require_any_bearer(
    State(tokens): State<Arc<[Arc<str>]>>,
    request: Request,
    next: Next,
) -> Response {
    match bearer(request.headers()) {
        // Every token is compared, so timing doesn't show which one matched.
        Some(t) if tokens
            .iter()
            .fold(false, |hit, token| constant_time_eq(t.as_bytes(), token.as_bytes()) | hit) =>
        {
            next.run(request).await
        }
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Compare secrets without an early exit, so timing doesn't show how much matched.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn compares_secrets() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
