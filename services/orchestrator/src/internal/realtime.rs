//! Realtime's orchestrator side (RT1 T6): the `/internal/realtime/*` routes realtime calls
//! (on the public listener's state, behind realtime's bearer; never on the internal listener
//! computes reach, S15), and the events the orchestrator publishes after its metadata writes
//! (P10).
//!
//! - `POST /internal/realtime/authorize {token, target}`: may this member watch this table
//!   target? The member's raw OIDC token is verified here (R10); the member is **read**, never
//!   made (no admission, no first-member admin claim, no tenant binding); then the console's
//!   own access check and gates (R3), mapped to S13's reasons. Always 200 past the bearer and
//!   the body's shape.
//! - `POST /internal/realtime/projects {token}`: the projects (and their live databases) the
//!   member may see, for realtime's project channels.
//! - `POST /internal/realtime/member {sub, org_id}` (T8-1, D7): may this person get a Studio
//!   ticket now? `{allowed}` (with `reason: "forbidden"` when not).
//! - `POST /internal/realtime/visible {…}`: the owner's privilege probe and key conversion
//!   (R19, W2, W4, P7, S12) on the console's owner pool, by relation OID with the keys bound.
//!   With `convert: true` (T8, owner deletes) it only prints `keys` in the owner session's form
//!   (`{keys}`, in order; no probe, `xid` ignored, rows needn't exist). With `convert_keys`
//!   beside a check (RT1 final fix round), the same owner session and transaction also prints
//!   those keys (`converted`, in order), so the check's rows and the converted keys can't
//!   disagree. The body may hold [`checks::MAX_KEYS`] long keys in each list
//!   ([`VISIBLE_BODY_LIMIT`]).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router, middleware};
use chelabase_auth::{AuthError, JwtVerifier, VerifiedClaims};

use chelabase_metadata::{BranchDatabase, BranchKind, BranchRecord, BranchStatus, DataApiState, DatabaseRecord};
use chelabase_nats::rt::{AccessEvent, ProjectEvent};
use chelabase_query::realtime as checks;
use chelabase_query::{Lease, QueryFailure, RunAs, RunSettings};
use chelabase_types::access::Requested;
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::{ProjectId, TenantId};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{error, warn};
use uuid::Uuid;

use super::BoxFuture;
use crate::access::{Target, authorize};
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort};
use chelabase_routes::orchestrator as ep;

/// Publishes realtime's platform and access events (`chelabase.rt.project.<project_id>`,
/// `chelabase.rt.access.<tenant_id>`). NATS in production, a fake in tests. A failed publish is
/// logged by the caller and never fails the write it reports.
pub trait RtEvents: Send + Sync + 'static {
    fn project(&self, project_id: Uuid, event: ProjectEvent) -> BoxFuture<'_, anyhow::Result<()>>;
    fn access(&self, event: AccessEvent) -> BoxFuture<'_, anyhow::Result<()>>;
}

impl RtEvents for async_nats::Client {
    fn project(&self, project_id: Uuid, event: ProjectEvent) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            chelabase_nats::rt::publish_project(self, project_id, &event).await?;
            Ok(())
        })
    }

    fn access(&self, event: AccessEvent) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            chelabase_nats::rt::publish_access(self, &event).await?;
            Ok(())
        })
    }
}

/// What the `/internal/realtime/*` routes check: realtime's bearer
/// (`CHELABASE_REALTIME_INTERNAL_TOKEN[_FILE]`) and the members' OIDC tokens (the gateway's
/// issuer and JWKS settings).
#[derive(Clone)]
pub struct RealtimeAuth {
    pub token: String,
    pub verifier: JwtVerifier,
}

/// Shared by both states.
pub type SharedRtEvents = Arc<dyn RtEvents>;

/// Publishes `events` on the project's subject, in order, off the caller's path. Called only
/// after the metadata write they report committed; a failure is logged (Studio's slow
/// fallback poll covers a lost event, P10).
pub fn spawn_project(events: SharedRtEvents, project_id: Uuid, list: Vec<ProjectEvent>) {
    if list.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for event in list {
            if let Err(err) = events.project(project_id, event).await {
                warn!(%project_id, "realtime: project event not published: {err:#}");
            }
        }
    });
}

/// Publishes one access event off the caller's path (see [`spawn_project`]).
pub fn spawn_access(events: SharedRtEvents, event: AccessEvent) {
    tokio::spawn(async move {
        let tenant_id = event.tenant_id;
        if let Err(err) = events.access(event).await {
            warn!(%tenant_id, "realtime: access event not published: {err:#}");
        }
    });
}

/// [`spawn_access`] for a write that changed `member_sub`'s access in `tenant` (`None`: the
/// writer doesn't know whose, so the whole tenant re-checks, S28).
pub(crate) fn access_changed(events: &SharedRtEvents, tenant: TenantId, member_sub: Option<&str>) {
    spawn_access(
        events.clone(),
        AccessEvent { tenant_id: tenant.as_uuid(), member_sub: member_sub.map(str::to_string) },
    );
}

/// [`spawn_project`] for one event.
pub(crate) fn project_changed(events: &SharedRtEvents, project: ProjectId, event: ProjectEvent) {
    spawn_project(events.clone(), project.as_uuid(), vec![event]);
}

// --- Routes ------------------------------------------------------------------------------------

pub const AUTHORIZE_PATH: &str = "/internal/realtime/authorize";
pub const PROJECTS_PATH: &str = "/internal/realtime/projects";
pub const VISIBLE_PATH: &str = "/internal/realtime/visible";
/// T8-1, D7: may this person get a Studio ticket now?
pub const MEMBER_PATH: &str = "/internal/realtime/member";

/// `/visible`'s body limit: [`checks::MAX_KEYS`] keys and as many to convert, each of long
/// texts (a composite text key near the btree limit is about 2.7 kB), past axum's 2 MB default.
pub const VISIBLE_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// The routes, behind realtime's bearer (checked before the body is read). Mounted on the
/// public listener's router only (the access check needs its state).
pub fn routes<M, J>(state: &Arc<AppState<M, J>>) -> Router<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    let token: Arc<str> = state.realtime.token.as_str().into();
    Router::new()
        .route(AUTHORIZE_PATH, post(authorize_route::<M, J>))
        .route(PROJECTS_PATH, post(projects_route::<M, J>))
        .route(MEMBER_PATH, post(member_route::<M, J>))
        .route(
            VISIBLE_PATH,
            post(visible_route::<M, J>).layer(DefaultBodyLimit::max(VISIBLE_BODY_LIMIT)),
        )
        .route_layer(middleware::from_fn_with_state(token, super::require_bearer))
}

/// The body as `T`, or 400 for anything else (not JSON, the wrong shape, unknown fields).
fn body<T: serde::de::DeserializeOwned>(bytes: &Bytes) -> Result<T, BadBody> {
    serde_json::from_slice(bytes).map_err(|_| BadBody)
}

/// A body that isn't the route's JSON (400).
struct BadBody;

impl IntoResponse for BadBody {
    fn into_response(self) -> Response {
        ApiError::invalid_request(400, "the body isn't this route's JSON").into_response()
    }
}

/// Why a member may not (or can't now) watch a target (P13/S13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// Leaves the channel.
    Forbidden,
    /// Leaves the channel.
    NotFound,
    /// The renewal is awaited (4001 at `exp`).
    TokenExpired,
    /// Degraded; retried.
    Unavailable,
}

impl Refusal {
    fn reason(self) -> &'static str {
        match self {
            Refusal::Forbidden => "forbidden",
            Refusal::NotFound => "not_found",
            Refusal::TokenExpired => "token_expired",
            Refusal::Unavailable => "unavailable",
        }
    }

    /// The HTTP status of the routes that answer a refusal as one (`/projects`, `/visible`).
    fn status(self) -> StatusCode {
        match self {
            Refusal::Forbidden => StatusCode::FORBIDDEN,
            Refusal::NotFound => StatusCode::NOT_FOUND,
            Refusal::TokenExpired => StatusCode::UNAUTHORIZED,
            Refusal::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn response(self) -> Response {
        (self.status(), Json(json!({ "error": { "code": self.reason() } }))).into_response()
    }

    /// S13 for an answer of the console's checks: a branch not ready and every 5xx are
    /// transient; 404 is an unknown target; every other refusal (a backup branch, the data API
    /// off, `role_refused`, the owner's name as a role, the access check) is final.
    fn of(err: &ApiError) -> Self {
        match (err.status, &err.code) {
            (_, ErrorCode::BranchNotReady) => Refusal::Unavailable,
            (404, _) => Refusal::NotFound,
            (400..=499, _) => Refusal::Forbidden,
            _ => Refusal::Unavailable,
        }
    }
}

/// The member's token verified (R10): expiry is `token_expired`, a JWKS or configuration
/// failure `unavailable`, anything else (a bad signature, another issuer, no org) `forbidden`.
async fn verify(verifier: &JwtVerifier, token: &str) -> Result<VerifiedClaims, Refusal> {
    verifier.verify(token).await.map_err(|err| match err {
        AuthError::Expired => Refusal::TokenExpired,
        AuthError::Jwks(e) => {
            warn!("realtime: the member token can't be checked (JWKS): {e}");
            Refusal::Unavailable
        }
        AuthError::Config(e) => {
            error!("realtime: the OIDC verifier is misconfigured: {e}");
            Refusal::Unavailable
        }
        AuthError::Unauthorized | AuthError::InvalidToken(_) => Refusal::Forbidden,
    })
}

/// The verified member, read only (R10): an org no tenant is bound to is `forbidden`; a
/// person with no member row gets `member: None`, which the access check refuses. A metadata
/// failure is `unavailable`.
async fn principal<M, J>(state: &AppState<M, J>, token: &str) -> Result<(AuthPrincipal, VerifiedClaims), Refusal>
where
    M: MetadataPort,
    J: JobPort,
{
    let claims = verify(&state.realtime.verifier, token).await?;
    match crate::read_only_principal(state, &claims).await {
        Ok(Some(principal)) => Ok((principal, claims)),
        Ok(None) => Err(Refusal::Forbidden),
        Err(err) => {
            warn!("realtime: reading the member failed: {err}");
            Err(Refusal::Unavailable)
        }
    }
}

/// What the console's gates resolved for a target.
struct Gated {
    record: DatabaseRecord,
    branch: BranchRecord,
    row: BranchDatabase,
}

/// The console's access check and gates for a target, in the console's order (R3,
/// `console.rs`): access as `ep::CONSOLE_QUERY`, the branch (not a backup, ready), the owner's
/// name refused as a role, the Postgres database (served, data API on), then the worker's role
/// check for a named role.
async fn gates<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    database: Uuid,
    branch_id: Uuid,
    pg_database: &str,
    role: Option<&str>,
) -> Result<Gated, Refusal>
where
    M: MetadataPort,
    J: JobPort,
{
    // R10: only an active member watches anything. Anyone else (no row, invited, pending,
    // removed) is refused before the target is looked at, so the answer never tells an
    // existing target from a missing one.
    if !principal.member.as_ref().is_some_and(|m| m.in_good_standing()) {
        return Err(Refusal::Forbidden);
    }
    let target = Target::Role {
        database,
        branch: branch_id,
        pg_database,
        role: role.map_or(Requested::Owner, Requested::Role),
    };
    let authorized = authorize(state, principal, ep::CONSOLE_QUERY.policy.access, target)
        .await
        .map_err(|e| Refusal::of(&e))?;
    let record = authorized.into_record().map_err(|e| Refusal::of(&e))?;
    let branch = crate::branches::load_branch(state, &record, branch_id)
        .await
        .map_err(|e| Refusal::of(&e))?;
    if branch.kind == BranchKind::Backup {
        return Err(Refusal::Forbidden);
    }
    // A reset or switch in progress: the channel stays, degraded (S13).
    if branch.status != BranchStatus::Ready {
        return Err(Refusal::Unavailable);
    }
    if role == Some(crate::roles::owner_name(&record)) {
        return Err(Refusal::Forbidden);
    }
    let rows = state.metadata.branch_databases(branch.id).await.map_err(|err| {
        warn!(branch_id = %branch.id, "realtime: listing the branch's databases failed: {err}");
        Refusal::Unavailable
    })?;
    let row = crate::console::pick_database(rows, Some(pg_database)).ok_or(Refusal::NotFound)?;
    if row.data_api != DataApiState::On {
        return Err(Refusal::Forbidden);
    }
    if let Some(role) = role {
        crate::console::check_role(state, branch.id, role)
            .await
            .map_err(|e| Refusal::of(&e))?;
    }
    Ok(Gated { record, branch, row })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizeBody {
    token: String,
    target: TargetBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetBody {
    database_id: Uuid,
    branch_id: Uuid,
    pg_database: String,
    #[serde(default)]
    role: Option<String>,
}

async fn authorize_route<M, J>(State(state): State<Arc<AppState<M, J>>>, bytes: Bytes) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let request: AuthorizeBody = match body(&bytes) {
        Ok(b) => b,
        Err(bad) => return bad.into_response(),
    };
    let t = request.target;
    // The caller's bug, not a refusal (S13).
    if t.role.as_deref() == Some("") {
        return ApiError::invalid_request(400, "role must not be empty (null is the owner)")
            .into_response();
    }
    let refused = |r: Refusal| Json(json!({ "allowed": false, "reason": r.reason() })).into_response();
    let (principal, claims) = match principal(&state, &request.token).await {
        Ok(p) => p,
        Err(r) => return refused(r),
    };
    if let Err(r) = gates(&state, &principal, t.database_id, t.branch_id, &t.pg_database, t.role.as_deref()).await {
        return refused(r);
    }
    let member = principal.member.as_ref();
    let name = claims
        .name
        .clone()
        .or_else(|| member.map(|m| m.display_name.clone()).filter(|n| !n.is_empty()));
    let email = claims.email.clone().or_else(|| member.and_then(|m| m.email.clone()));
    Json(json!({
        "allowed": true,
        "tenant_id": principal.tenant_id.as_uuid(),
        "member": { "sub": claims.sub, "name": name, "email": email },
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectsBody {
    token: String,
}

async fn projects_route<M, J>(State(state): State<Arc<AppState<M, J>>>, bytes: Bytes) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let request: ProjectsBody = match body(&bytes) {
        Ok(b) => b,
        Err(bad) => return bad.into_response(),
    };
    let (principal, _) = match principal(&state, &request.token).await {
        Ok(p) => p,
        Err(r) => return r.response(),
    };
    // Only an active member sees projects (a pending, invited or removed one, or a person with
    // no row, is refused like a non-member).
    // D7: an offboarded one too.
    let Some(member) = principal.member.as_ref().filter(|m| m.in_good_standing()) else {
        return Refusal::Forbidden.response();
    };
    let unavailable = |what: &str, err: &dyn std::fmt::Display| {
        warn!("realtime projects: {what}: {err}");
        Refusal::Unavailable.response()
    };
    let mut projects = match state.metadata.list_projects(principal.tenant_id).await {
        Ok(p) => p,
        Err(err) => return unavailable("list projects", &err),
    };
    if !member.tenant_admin {
        // T2-4: every access source, not the own grants alone.
        let granted = match state
            .metadata
            .member_grant_sources(principal.tenant_id, member.id)
            .await
        {
            Ok(g) => g,
            Err(err) => return unavailable("member grant sources", &err),
        };
        projects.retain(|p| granted.iter().any(|(id, _)| *id == p.id.as_uuid()));
    }
    let mut out = Vec::with_capacity(projects.len());
    for project in projects {
        let databases = match state.metadata.project_databases(project.id).await {
            Ok(d) => d,
            Err(err) => return unavailable("project databases", &err),
        };
        out.push(json!({
            "project_id": project.id.as_uuid(),
            "database_ids": databases.into_iter().map(|(id, _, _)| id.as_uuid()).collect::<Vec<_>>(),
        }));
    }
    Json(json!({ "tenant_id": principal.tenant_id.as_uuid(), "projects": out })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberBody {
    sub: String,
    org_id: String,
}

/// T8-1, D7: realtime asks before it issues a Studio ticket, with the gateway's identity
/// (`sub`, `org_id`). `{allowed: true}` only for a member of the org's tenant in good standing
/// (active, not offboarded); a suspended, offboarded, pending, invited or removed member, a
/// person with no row, or an org no tenant is bound to is `{allowed: false, reason:
/// "forbidden"}`. The member is read, never made. A metadata failure is 503.
async fn member_route<M, J>(State(state): State<Arc<AppState<M, J>>>, bytes: Bytes) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let request: MemberBody = match body(&bytes) {
        Ok(b) => b,
        Err(bad) => return bad.into_response(),
    };
    let refused = || {
        Json(json!({ "allowed": false, "reason": Refusal::Forbidden.reason() })).into_response()
    };
    let tenant = match state.metadata.get_tenant_by_zitadel_org_id(&request.org_id).await {
        Ok(Some(tenant)) => tenant,
        Ok(None) => return refused(),
        Err(err) => {
            warn!("realtime member check: the tenant lookup failed: {err}");
            return Refusal::Unavailable.response();
        }
    };
    match state.metadata.member_by_zitadel(tenant, &request.sub).await {
        Ok(Some(member)) if member.in_good_standing() => {
            Json(json!({ "allowed": true })).into_response()
        }
        Ok(_) => refused(),
        Err(err) => {
            warn!("realtime member check: reading the member failed: {err}");
            Refusal::Unavailable.response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VisibleBody {
    token: String,
    database_id: Uuid,
    branch_id: Uuid,
    pg_database: String,
    rel_oid: u32,
    /// An xid8 in decimal; `None` = no visibility clause.
    #[serde(default)]
    xid: Option<String>,
    /// Rows of key texts in key order; `None` = the probe only.
    #[serde(default)]
    keys: Option<Vec<Vec<String>>>,
    /// RT1 T8 (owner deletes): print `keys` as the owner's session does, without reading the
    /// relation and without a probe; `keys` required, `xid` ignored. Answers `{keys}`.
    #[serde(default)]
    convert: bool,
    /// RT1 final fix round: beside a check (not the convert mode), keys to print in the same
    /// owner session and transaction as the check. Answers `converted` (in order) besides the
    /// check's fields; nothing for them when the probe or the check failed.
    #[serde(default)]
    convert_keys: Option<Vec<Vec<String>>>,
}

/// A decimal xid8: digits only, at most `u64::MAX`.
fn parse_xid(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The statement timeout of the owner's checks (the realtime check session's, S10).
const CHECK_STATEMENT_TIMEOUT_MS: i64 = 2000;
/// The longest one `/visible` call takes, from its token check to its answer (realtime gives
/// it 5 s, S14): past it the call answers 503 and lets go of its owner connection.
pub const VISIBLE_BUDGET: Duration = Duration::from_secs(4);

/// What one owner session answered.
enum Outcome {
    /// The relation is gone, the probe failed, or the check failed in Postgres: realtime
    /// treats the window as `changed` (or the join as refused).
    Failed,
    Probed(checks::Probe),
    Checked(checks::Probe, checks::Checked),
    /// A key row of another arity than the key, or keys that can't be checked (the caller's
    /// bug: realtime holds the same allowlist).
    BadKeys(&'static str),
    /// The convert mode's key strings, in the request's order.
    Converted(Vec<String>),
    /// A check, and the `convert_keys` printed in its session (`None`: the conversion failed in
    /// Postgres; the check's answer still stands, as read before it).
    CheckedConverted(checks::Probe, Option<checks::Checked>, Option<Vec<String>>),
}

async fn visible_route<M, J>(State(state): State<Arc<AppState<M, J>>>, bytes: Bytes) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let request: VisibleBody = match body(&bytes) {
        Ok(b) => b,
        Err(bad) => return bad.into_response(),
    };
    // Everything that can be refused without a database first.
    let bad = |message: &str| ApiError::invalid_request(400, message).into_response();
    // The convert mode ignores the xid (it reads no rows) and needs keys.
    let xid = match request.xid.as_deref().filter(|_| !request.convert).map(parse_xid) {
        None => None,
        Some(Some(x)) => Some(x),
        Some(None) => return bad("xid is an xid8 in decimal"),
    };
    if request.convert && request.keys.is_none() {
        return bad("convert takes keys");
    }
    if request.convert && request.convert_keys.is_some() {
        return bad("convert_keys goes with a check, not the convert mode");
    }
    for keys in [&request.keys, &request.convert_keys].into_iter().flatten() {
        if keys.len() > checks::MAX_KEYS {
            return bad("at most 1000 keys");
        }
        let width = keys.first().map_or(1, Vec::len);
        if width == 0 || keys.iter().any(|k| k.len() != width) {
            return bad("every key row has one text per key column");
        }
    }
    // The whole call, the token check and the gates included, within the budget (S14).
    match tokio::time::timeout(VISIBLE_BUDGET, visible(&state, request, xid)).await {
        Ok(response) => response,
        Err(_) => Refusal::Unavailable.response(),
    }
}

/// `/visible` past its input checks (see [`visible_route`]).
async fn visible<M, J>(state: &AppState<M, J>, request: VisibleBody, xid: Option<u64>) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let bad = |message: &str| ApiError::invalid_request(400, message).into_response();
    let (principal, _) = match principal(state, &request.token).await {
        Ok(p) => p,
        Err(r) => return r.response(),
    };
    // R10: only the owner's probe runs here, re-authorized as the owner (Admin).
    let gated = match gates(state, &principal, request.database_id, request.branch_id, &request.pg_database, None).await {
        Ok(g) => g,
        Err(r) => return r.response(),
    };
    let owner = crate::roles::owner_name(&gated.record).to_owned();
    let (pool, tls) = match crate::console::owner_pool_for(state, &gated.branch, &gated.row, &owner).await {
        Ok(p) => p,
        Err(err) => return Refusal::of(&err).response(),
    };
    let settings = RunSettings {
        max_rows: i64::from(gated.record.max_rows),
        statement_timeout_ms: CHECK_STATEMENT_TIMEOUT_MS,
        default_schema: gated.row.default_schema.clone(),
        reset_sql: crate::console::OWNER_RESET_SQL,
    };
    let rid = principal.request_id.clone();
    let keys = request.keys;
    let convert_keys = request.convert_keys;
    let rel_oid = request.rel_oid;
    let convert = request.convert;
    let pg_database = gated.row.name.clone();
    // Realtime's share of the owner pool: the console always keeps a connection of its own.
    let slot = state.console.realtime_slot(gated.branch.id, &gated.row.name);
    let work = async {
        let _slot = slot
            .acquire()
            .await
            .map_err(|_| QueryFailure::unavailable("the owner pool's realtime share is closed"))?;
        let mut lease = Lease::new(pool.get().await.map_err(|e| crate::console::pool_failure(e, &rid, &tls))?);
        let out = chelabase_query::engine::console_session(
            &lease,
            &pg_database,
            RunAs::Owner { user: &owner },
            &settings,
            &rid,
            async |client, _version| {
                Ok(match (convert, keys.as_deref()) {
                    (true, Some(keys)) => owner_convert(client, rel_oid, keys).await,
                    _ => owner_checks(client, rel_oid, xid, keys.as_deref(), convert_keys.as_deref()).await,
                })
            },
        )
        .await;
        lease.done();
        out
    };
    let outcome: Result<Outcome, QueryFailure> = work.await;
    let answer = |probe: Value, visible: bool, keys: Vec<String>| {
        Json(json!({ "probe": probe, "visible": visible, "keys": keys })).into_response()
    };
    match outcome {
        Ok(Outcome::Converted(keys)) => Json(json!({ "keys": keys })).into_response(),
        // A conversion that failed in Postgres has no keys to give: realtime sends keyless
        // deletes.
        Ok(Outcome::Failed) if convert => Refusal::Unavailable.response(),
        Ok(Outcome::Failed) => answer(json!({ "ok": false, "bypass": false }), true, Vec::new()),
        Ok(Outcome::Probed(p)) => answer(json!({ "ok": true, "bypass": p.bypass }), true, Vec::new()),
        Ok(Outcome::Checked(p, c)) => answer(json!({ "ok": true, "bypass": p.bypass }), c.visible, c.keys),
        Ok(Outcome::CheckedConverted(p, c, converted)) => {
            let (visible, keys) = c.map_or((true, Vec::new()), |c| (c.visible, c.keys));
            let mut body = json!({ "probe": { "ok": true, "bypass": p.bypass }, "visible": visible, "keys": keys });
            if let Some(converted) = converted {
                body["converted"] = json!(converted);
            }
            Json(body).into_response()
        }
        Ok(Outcome::BadKeys(message)) => bad(message),
        Err(failure) => {
            warn!(request_id = %rid, code = %failure.code, "realtime visible: the owner session failed: {}", failure.message);
            Refusal::Unavailable.response()
        }
    }
}

/// The convert mode (owner deletes): the relation's key read from the catalog, then each key's
/// texts cast through its column's type and printed in the owner's session (the rows needn't
/// exist; no probe). P6 and the arity are checked as for a check (400); a Postgres error (a
/// text the type can't read, a relation that's gone) is [`Outcome::Failed`].
async fn owner_convert(client: &tokio_postgres::Client, rel_oid: u32, keys: &[Vec<String>]) -> Outcome {
    let rel = match checks::load_relation(client, rel_oid).await {
        Ok(Some(rel)) => rel,
        Ok(None) => return Outcome::Failed,
        Err(err) => {
            warn!(rel_oid, "realtime visible: reading the relation failed: {err}");
            return Outcome::Failed;
        }
    };
    if let Some(refused) = keys_refused(&rel, keys, "converted") {
        return Outcome::BadKeys(refused);
    }
    match convert_in_session(client, &rel, keys).await {
        Some(converted) => Outcome::Converted(converted),
        None => Outcome::Failed,
    }
}

/// `keys` printed in the session's form, in order; `None` for a Postgres error or a count that
/// differs.
async fn convert_in_session(
    client: &tokio_postgres::Client,
    rel: &checks::Relation,
    keys: &[Vec<String>],
) -> Option<Vec<String>> {
    if keys.is_empty() {
        return Some(Vec::new());
    }
    match checks::convert(client, rel, keys).await {
        Ok(converted) if converted.len() == keys.len() => Some(converted),
        Ok(_) | Err(_) => None,
    }
}

/// Why `keys` can't be checked or converted (`what`) against `rel`'s key (P6, the arity), if so.
fn keys_refused(rel: &checks::Relation, keys: &[Vec<String>], what: &'static str) -> Option<&'static str> {
    let checked = what == "checked";
    if !checks::keys_supported(rel) {
        return Some(if checked {
            "the relation's key can't be checked (P6)"
        } else {
            "the relation's key can't be converted (P6)"
        });
    }
    if keys.iter().any(|k| k.len() != rel.key.len()) {
        return Some("every key row has one text per key column");
    }
    if keys.iter().any(|k| k.iter().zip(&rel.key).any(|(t, c)| !checks::key_text_allowed(c, t))) {
        return Some(if checked {
            "a key text can't be checked (P6: a float NaN)"
        } else {
            "a key text can't be converted (P6: a float NaN)"
        });
    }
    None
}

/// The owner's catalog read, probe and (with keys) check, in the console's owner session. A
/// Postgres error in any of them is [`Outcome::Failed`] (the session's transaction is then
/// aborted and its `COMMIT` rolls back; nothing was written).
async fn owner_checks(
    client: &tokio_postgres::Client,
    rel_oid: u32,
    xid: Option<u64>,
    keys: Option<&[Vec<String>]>,
    convert_keys: Option<&[Vec<String>]>,
) -> Outcome {
    let rel = match checks::load_relation(client, rel_oid).await {
        Ok(Some(rel)) => rel,
        Ok(None) => return Outcome::Failed,
        Err(err) => {
            warn!(rel_oid, "realtime visible: reading the relation failed: {err}");
            return Outcome::Failed;
        }
    };
    let probe = match checks::probe(client, &rel).await {
        Ok(p) => p,
        // W2: the owner can't read the key columns (no schema USAGE, no SELECT).
        Err(_) => return Outcome::Failed,
    };
    for list in [keys, convert_keys].into_iter().flatten() {
        if let Some(refused) = keys_refused(&rel, list, "checked") {
            return Outcome::BadKeys(refused);
        }
    }
    let checked = match keys {
        None => None,
        Some(keys) => match checks::check(client, &rel, xid, keys).await {
            Ok(c) => Some(c),
            Err(_) => return Outcome::Failed,
        },
    };
    let Some(convert_keys) = convert_keys else {
        return match checked {
            Some(c) => Outcome::Checked(probe, c),
            None => Outcome::Probed(probe),
        };
    };
    // After the check, in its session and transaction. A failure aborts the transaction (its
    // COMMIT rolls back; nothing was written) but the check's answer, read before, stands.
    let converted = convert_in_session(client, &rel, convert_keys).await;
    Outcome::CheckedConverted(probe, checked, converted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xids_are_plain_decimal_u64s() {
        for (text, want) in [("0", Some(0)), ("3", Some(3)), ("007", Some(7)), ("18446744073709551615", Some(u64::MAX))] {
            assert_eq!(parse_xid(text), want, "{text:?}");
        }
        for bad in ["", "+1", "-0", "1 ", "1e3", "١٢", "18446744073709551616", "0x1"] {
            assert_eq!(parse_xid(bad), None, "{bad:?}");
        }
    }

    // S13: every answer of the console's checks maps to one reason.
    #[test]
    fn console_answers_map_to_s13_reasons() {
        let cases = [
            (ApiError::not_found("database not found"), Refusal::NotFound),
            (ApiError::new(409, ErrorCode::BranchNotReady, "x"), Refusal::Unavailable),
            (ApiError::new(409, ErrorCode::BackupBranch, "x"), Refusal::Forbidden),
            (ApiError::new(409, ErrorCode::DataApiDisabled, "x"), Refusal::Forbidden),
            (ApiError::new(409, ErrorCode::OwnerPasswordUnknown, "x"), Refusal::Forbidden),
            (ApiError::new(403, ErrorCode::RoleRefused, "x"), Refusal::Forbidden),
            (ApiError::new(403, ErrorCode::Forbidden, "x"), Refusal::Forbidden),
            (ApiError::new(422, ErrorCode::InvalidRequest, "x"), Refusal::Forbidden),
            (ApiError::internal(), Refusal::Unavailable),
            (ApiError::new(503, ErrorCode::WorkerUnavailable, "x"), Refusal::Unavailable),
            (ApiError::new(503, ErrorCode::ComputeTlsFailed, "x"), Refusal::Unavailable),
            (ApiError::new(504, ErrorCode::Timeout, "x"), Refusal::Unavailable),
        ];
        for (err, want) in cases {
            assert_eq!(Refusal::of(&err), want, "{} {:?}", err.status, err.code);
        }
    }

    #[test]
    fn refusals_answer_with_their_status() {
        assert_eq!(Refusal::Forbidden.status(), StatusCode::FORBIDDEN);
        assert_eq!(Refusal::NotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(Refusal::TokenExpired.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(Refusal::Unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
