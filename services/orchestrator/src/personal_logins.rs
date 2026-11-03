//! Personal Postgres logins (members hub step 1, spec Part 6): one login per (member, Chelabase
//! database), made, reset and deleted by its member, listed by caller.
//!
//! - The record is first (T3's metadata write, with a SCRAM verifier only); the worker's
//!   member-role reconcile makes the role on each branch (P7). Create asks main for one
//!   synchronous member-filtered run and enqueues the other branches (S9).
//! - The password is generated here, answered once with `Cache-Control: no-store`, and never
//!   stored, logged, listed or put in an event or a worker request (MH-15).
//! - Every write that removes or changes a login's secret tells the proxies' cache about each
//!   of its rows (P4); the metadata already took or replaced the verifier in its transaction.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_metadata::personal_logins::{
    LoginStatus, LoginWriteError, PersonalLogin, RevokeReason,
};
use chelabase_metadata::{BranchKind, BranchRecord, BranchStatus, DatabaseRecord};
use chelabase_nats::{MemberRolesReconcileRequest, MemberRolesReply};
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::PlatformRole;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::role_name::{RoleNameError, check_login_role_name};
use chelabase_types::{ApiJson, ApiPath, BranchId, DatabaseId, ProjectId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{Target, authorize};
use crate::members::publish_login_rows;
use crate::roles::{name_error, owner_name};
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort};

/// D4: a login's `VALID UNTIL`, from its creation or last reset.
const LOGIN_LIFETIME_DAYS: i64 = 90;

/// The answer for a login that isn't there or isn't the caller's to touch: one message for
/// every case, so it tells nothing apart (and isn't a route's generic "not found").
const LOGIN_NOT_FOUND: &str = "personal login not found";

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_member_personal_logins))
        .routes(routes!(list_database_personal_logins))
        .routes(routes!(put_personal_login))
        .routes(routes!(reset_personal_login))
        .routes(routes!(delete_personal_login))
}

// --- Shapes ------------------------------------------------------------------------------------

/// A personal login's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersonalLoginStatus {
    /// Usable on every branch where the member qualifies.
    Active,
    /// Past `valid_until`: a password reset renews it.
    Expired,
    /// Revoked (see `revoked_reason`): delete it, or make a new one, which replaces it.
    Revoked,
}

/// Why a login was revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersonalLoginRevokeReason {
    /// The member no longer qualifies on the database (below Developer there, or no grant).
    AccessLost,
    /// The member was suspended.
    Suspended,
    /// A tenant admin revoked the member's Postgres access.
    RevokedNow,
    /// The member's identity user was banned or removed.
    IdentityOffboarded,
    /// The member was removed.
    MemberRemoved,
}

impl From<RevokeReason> for PersonalLoginRevokeReason {
    fn from(r: RevokeReason) -> Self {
        match r {
            RevokeReason::AccessLost => Self::AccessLost,
            RevokeReason::Suspended => Self::Suspended,
            RevokeReason::RevokedNow => Self::RevokedNow,
            RevokeReason::IdentityOffboarded => Self::IdentityOffboarded,
            RevokeReason::MemberRemoved => Self::MemberRemoved,
        }
    }
}

/// A member's personal Postgres login on one Chelabase database. Never its password or
/// verifier.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct PersonalLoginView {
    member_id: Uuid,
    database_id: DatabaseId,
    database_name: String,
    project_id: ProjectId,
    /// The Postgres role name, the same on every branch.
    role_name: String,
    /// When the password stops working (90 days after its creation or last reset).
    valid_until: DateTime<Utc>,
    created_at: DateTime<Utc>,
    status: PersonalLoginStatus,
    #[schema(required = true)]
    revoked_at: Option<DateTime<Utc>>,
    #[schema(required = true)]
    revoked_reason: Option<PersonalLoginRevokeReason>,
}

impl From<PersonalLogin> for PersonalLoginView {
    fn from(l: PersonalLogin) -> Self {
        Self {
            member_id: l.member_id,
            database_id: DatabaseId(l.database_id),
            database_name: l.database_name,
            project_id: ProjectId::from(l.project_id),
            role_name: l.role_name,
            valid_until: l.valid_until,
            created_at: l.created_at,
            status: match l.status {
                LoginStatus::Active => PersonalLoginStatus::Active,
                LoginStatus::Expired => PersonalLoginStatus::Expired,
                LoginStatus::Revoked => PersonalLoginStatus::Revoked,
            },
            revoked_at: l.revoked_at,
            revoked_reason: l.revoked_reason.map(Into::into),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct PersonalLoginList {
    logins: Vec<PersonalLoginView>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct PutPersonalLoginRequest {
    /// The login's Postgres role name: `^[a-z_][a-z0-9_]{0,62}$`, not reserved, and not taken
    /// by any role on any branch of the database nor by another login of it.
    name: String,
}

/// A login with its password, answered once (create and reset).
// T8-4: its `Debug` (below) never shows the password or the connection string that carries it.
#[derive(Serialize, ToSchema)]
pub(crate) struct PersonalLoginSecret {
    login: PersonalLoginView,
    /// Shown this once: it is never stored (only a SCRAM verifier is), listed or logged.
    password: String,
    /// For the main branch's default database. The same login works on every branch where the
    /// member qualifies, with that branch's host.
    connection_string: String,
    /// Whether the login already works on the main branch. When `false` (the compute isn't
    /// running, or the worker didn't answer in time), it works there within seconds.
    ready_on_main: bool,
}

impl std::fmt::Debug for PersonalLoginSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonalLoginSecret")
            .field("login", &self.login)
            .field("password", &"<redacted>")
            .field("connection_string", &"<redacted>")
            .field("ready_on_main", &self.ready_on_main)
            .finish()
    }
}

// --- Helpers -----------------------------------------------------------------------------------

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(err) => return err.into_response(),
        }
    };
}

fn internal(what: &str, err: impl std::fmt::Display) -> ApiError {
    error!("{what}: {err}");
    ApiError::internal()
}

fn login_not_found() -> ApiError {
    ApiError::not_found(LOGIN_NOT_FOUND)
}

/// S15: a refused login write as the API's answer.
fn write_error(what: &str, err: LoginWriteError) -> ApiError {
    match err {
        LoginWriteError::NotQualified => {
            crate::access::deny_to_api(chelabase_types::access::Deny::Forbidden {
                required: PlatformRole::Developer.as_str().to_string(),
            })
        }
        LoginWriteError::Exists => ApiError::new(
            409,
            ErrorCode::PersonalLoginExists,
            "you already have a personal login on this database: reset its password, or delete it first",
        ),
        LoginWriteError::NameTaken => ApiError::new(
            409,
            ErrorCode::RoleExists,
            "a role or a personal login of this database already has that name",
        ),
        LoginWriteError::NotFound => login_not_found(),
        LoginWriteError::Access(err) => crate::members::write_failed(what, err, LOGIN_NOT_FOUND),
        LoginWriteError::Db(err) => internal(what, err),
    }
}

/// A password of 32 characters from `[A-Za-z0-9]` (about 190 bits), from the OS's random
/// generator; alphanumeric, so it needs no quoting in a URI or a `psql` prompt.
fn generate_password() -> String {
    const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(32);
    while out.len() < 32 {
        let mut bytes = [0u8; 64];
        getrandom::fill(&mut bytes).expect("OS random number generator");
        // Bytes of 248 or more are skipped, so every character is equally likely.
        for b in bytes.into_iter().filter(|b| *b < 248) {
            if out.len() == 32 {
                break;
            }
            out.push(ALPHABET[usize::from(b % 62)] as char);
        }
    }
    out
}

/// The response of a call carrying a password: nothing may keep it.
fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

/// The database's live branches (not deleting, deleted or a backup), main first.
async fn live_branches<M, J>(
    state: &AppState<M, J>,
    database: DatabaseId,
) -> Result<Vec<BranchRecord>, ApiError>
where
    M: MetadataPort,
{
    let mut branches: Vec<BranchRecord> = state
        .metadata
        .list_branches(database)
        .await
        .map_err(|err| internal("personal login: list branches", err))?
        .into_iter()
        .filter(|b| {
            b.kind != BranchKind::Backup
                && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted)
        })
        .collect();
    branches.sort_by_key(|b| b.parent_branch_id.is_some());
    Ok(branches)
}

/// P7, S9: one synchronous member-filtered reconcile of main, then the other branches
/// enqueued. Whether the login works on main now: the worker answered `Done` and the login's
/// row is on main's endpoint. Any other outcome is `false`, never an error.
async fn converge<M, J>(
    state: &AppState<M, J>,
    database: DatabaseId,
    member: Uuid,
    role_name: &str,
) -> bool
where
    M: MetadataPort,
    J: JobPort,
{
    let branches = match live_branches(state, database).await {
        Ok(branches) => branches,
        Err(_) => {
            warn!(database_id = %database, "personal login: branches unreadable; the sweep will converge it");
            return false;
        }
    };
    let main = branches
        .iter()
        .find(|b| b.parent_branch_id.is_none())
        .filter(|b| b.status == BranchStatus::Ready && b.endpoint_id.is_some())
        .map(|b| b.id);
    let mut ready = false;
    // T8-5: main is enqueued like the others unless its synchronous reconcile was `Done`.
    let mut main_done = false;
    if let Some(main) = main {
        let reply = state
            .js
            .reconcile_member_roles(MemberRolesReconcileRequest {
                branch_id: main.as_uuid(),
                member_id: Some(member),
                from_ddl: false,
            })
            .await;
        match reply {
            Ok(MemberRolesReply::Done { .. }) => {
                main_done = true;
                ready = match state.metadata.member_login_endpoints(member).await {
                    Ok(rows) => rows
                        .iter()
                        .any(|r| r.branch_id == main.as_uuid() && r.role_name == role_name),
                    Err(err) => {
                        warn!(member_id = %member, "personal login: its rows aren't readable: {err}");
                        false
                    }
                };
            }
            Ok(MemberRolesReply::Failed { message }) => {
                warn!(branch_id = %main, member_id = %member, "personal login: main's reconcile failed; it is retried: {message}");
            }
            Err(err) => {
                warn!(branch_id = %main, member_id = %member, "personal login: main's reconcile didn't answer; it is retried: {err:?}");
            }
        }
    }
    for b in branches.iter().filter(|b| Some(b.id) != main || !main_done) {
        if let Err(err) = state.js.enqueue_member_roles(b.id).await {
            warn!(branch_id = %b.id, "member roles: enqueue failed; the sweep will reconcile it: {err}");
        }
    }
    ready
}

/// Main's endpoint, read before any write: a password answered without a way to use it would
/// be lost, so a main without one is refused first (409 `credentials_unavailable`).
async fn main_endpoint<M, J>(
    state: &AppState<M, J>,
    record: &DatabaseRecord,
) -> Result<String, ApiError>
where
    M: MetadataPort,
{
    live_branches(state, record.database_id)
        .await?
        .into_iter()
        .find(|b| b.parent_branch_id.is_none())
        .and_then(|b| b.endpoint_id)
        .ok_or_else(|| {
            ApiError::new(
                409,
                ErrorCode::CredentialsUnavailable,
                "the main branch has no endpoint yet",
            )
        })
}

/// The login's connection string for main's default database (P23).
fn connection_string<M, J>(
    state: &AppState<M, J>,
    record: &DatabaseRecord,
    endpoint: &str,
    role_name: &str,
    password: &str,
) -> String {
    let database = record
        .database_name
        .clone()
        .unwrap_or_else(|| record.name.clone());
    format!(
        "postgresql://{}:{}@{}:{}/{}?sslmode=verify-full",
        crate::uri_component(role_name),
        crate::uri_component(password),
        crate::proxy_host(endpoint, &state.proxy.domain),
        state.proxy.pg_port,
        crate::uri_component(&database),
    )
}

/// The caller's member id (a passed access check means there is one).
fn caller_id(principal: &AuthPrincipal) -> Result<Uuid, ApiError> {
    principal
        .member
        .as_ref()
        .map(|m| m.id)
        .ok_or_else(|| internal("personal login", "allowed a caller with no member row"))
}

fn valid_until() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::days(LOGIN_LIFETIME_DAYS)
}

// --- Routes ------------------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/members/{member_id}/personal-logins",
    operation_id = "listMemberPersonalLogins",
    tag = "members",
    summary = "List a member's personal logins",
    description = "Any status. Filtered by the caller: a member sees all of their own, a tenant admin all of anyone's, an Admin of a project the logins on that project's databases; anyone else gets an empty list. Never a password or verifier.",
    params(("member_id" = Uuid, Path, description = "Member id")),
    responses(
        (status = 200, description = "The logins the caller may see", body = PersonalLoginList),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`member_suspended`: the caller is suspended; `member_offboarded`: the caller's account was offboarded", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such member (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_member_personal_logins<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(member_id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::LIST_MEMBER_PERSONAL_LOGINS.policy.access,
            Target::None
        )
        .await
    );
    let Some(caller) = principal.member.as_ref() else {
        return internal("personal logins", "allowed a caller with no member row").into_response();
    };
    match state.metadata.member(member_id).await {
        Ok(Some(m)) if m.tenant_id == principal.tenant_id => {}
        Ok(_) => return ApiError::not_found("member not found").into_response(),
        Err(err) => return internal("personal logins: load member", err).into_response(),
    }
    let logins = tri!(
        state
            .metadata
            .member_personal_logins(member_id)
            .await
            .map_err(|err| internal("personal logins: list", err))
    );
    let mut shown = Vec::new();
    if caller.id == member_id || caller.tenant_admin {
        shown = logins;
    } else {
        // MH-22: an Admin of the database (overrides included) sees the logins there.
        let mut admin_on: HashMap<Uuid, bool> = HashMap::new();
        for login in logins {
            let admin = match admin_on.get(&login.database_id) {
                Some(admin) => *admin,
                None => {
                    let sources = tri!(
                        state
                            .metadata
                            .grant_sources(caller.id, login.project_id, Some(login.database_id))
                            .await
                            .map_err(|err| internal("personal logins: caller's access", err))
                    );
                    let admin = sources
                        .effective()
                        .is_some_and(|e| e.platform_role == PlatformRole::Admin);
                    admin_on.insert(login.database_id, admin);
                    admin
                }
            };
            if admin {
                shown.push(login);
            }
        }
    }
    Json(PersonalLoginList {
        logins: shown.into_iter().map(Into::into).collect(),
    })
    .into_response()
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/personal-logins",
    operation_id = "listDatabasePersonalLogins",
    tag = "members",
    summary = "List a database's personal logins",
    description = "Every personal login of the database, any status, by role name (the Roles tab counts the active ones). Never a password or verifier.",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The database's logins", body = PersonalLoginList),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: only the project's Admins and tenant admins may (`required`); `member_suspended`: the caller is suspended; `member_offboarded`: the caller's account was offboarded", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_database_personal_logins<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::LIST_DATABASE_PERSONAL_LOGINS.policy.access,
            Target::Database(id)
        )
        .await
    );
    let logins = tri!(
        state
            .metadata
            .database_personal_logins(id)
            .await
            .map_err(|err| internal("personal logins of a database", err))
    );
    Json(PersonalLoginList {
        logins: logins.into_iter().map(Into::into).collect(),
    })
    .into_response()
}

#[utoipa::path(
    put,
    path = "/v1/databases/{id}/personal-login",
    operation_id = "createPersonalLogin",
    tag = "members",
    summary = "Create your personal login on a database",
    description = "Your own Postgres login for this database, for Developers and above there (tenant admins included). Its password is generated and answered this once, with `Cache-Control: no-store`; only a SCRAM verifier is stored. It is valid for 90 days (a password reset renews it) and works on every branch where you qualify, acting only as the roles you hold at Developer or above. A revoked or expired login of yours on the database is replaced.",
    params(("id" = Uuid, Path, description = "Database id")),
    request_body = PutPersonalLoginRequest,
    responses(
        (status = 201, description = "The login and its password, once", body = PersonalLoginSecret),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: you're below Developer on the database (`required` is `developer`); `member_suspended`: you're suspended; `member_offboarded`: your account was offboarded", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`personal_login_exists`: you have a live login on this database (reset it, or delete it first); `role_exists`: a role on a branch of the database, or another login of it, has the name; `credentials_unavailable`: the main branch has no endpoint", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_role_name`: the name doesn't fit the rules; `reserved_role_name`: the name is reserved; `invalid_request`: `name` is missing", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn put_personal_login<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<PutPersonalLoginRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let authorized = tri!(
        authorize(
            &state,
            &principal,
            ep::PUT_PERSONAL_LOGIN.policy.access,
            Target::Database(id)
        )
        .await
    );
    let record = tri!(authorized.into_record());
    let member = tri!(caller_id(&principal));
    if let Err(err) = check_login_role_name(&body.name) {
        return name_error(err).into_response();
    }
    // The database owner's name is the owner's on every branch, also one without its rows yet.
    if body.name == owner_name(&record) {
        return name_error(RoleNameError::Reserved).into_response();
    }
    // T3-M4: a revoked or expired record of the caller's is replaced, and its rows go with it:
    // read where they are first, to tell the proxies once the create has committed.
    let replaced = tri!(replaced_rows(&state, &record, member).await);
    let endpoint = tri!(main_endpoint(&state, &record).await);
    let password = generate_password();
    let verifier = chelabase_neon::scram::verifier(&password);
    let login = tri!(
        state
            .metadata
            .create_personal_login(member, id, &body.name, &verifier, valid_until())
            .await
            .map_err(|err| write_error("create personal login", err))
    );
    publish_login_rows(&state, replaced);
    info!(request_id = %principal.request_id, member_id = %member, database_id = %id, role = %login.role_name, "personal login created");
    let ready_on_main = converge(&state, record.database_id, member, &login.role_name).await;
    let connection_string =
        connection_string(&state, &record, &endpoint, &login.role_name, &password);
    no_store(
        (
            StatusCode::CREATED,
            Json(PersonalLoginSecret {
                login: login.into(),
                password,
                connection_string,
                ready_on_main,
            }),
        )
            .into_response(),
    )
}

/// The rows of `member`'s record on the database when it would be replaced (revoked or
/// expired); none for a live one or none at all.
async fn replaced_rows<M, J>(
    state: &AppState<M, J>,
    record: &DatabaseRecord,
    member: Uuid,
) -> Result<Vec<chelabase_metadata::personal_logins::LoginEndpoint>, ApiError>
where
    M: MetadataPort,
{
    let database = record.database_id.as_uuid();
    let logins = state
        .metadata
        .member_personal_logins(member)
        .await
        .map_err(|err| internal("personal login: list", err))?;
    let Some(old) = logins.into_iter().find(|l| l.database_id == database) else {
        return Ok(Vec::new());
    };
    if old.status == LoginStatus::Active {
        return Ok(Vec::new());
    }
    let branches: Vec<Uuid> = state
        .metadata
        .list_branches(record.database_id)
        .await
        .map_err(|err| internal("personal login: list branches", err))?
        .into_iter()
        .map(|b| b.id.as_uuid())
        .collect();
    Ok(state
        .metadata
        .member_login_endpoints(member)
        .await
        .map_err(|err| internal("personal login: rows", err))?
        .into_iter()
        .filter(|r| r.role_name == old.role_name && branches.contains(&r.branch_id))
        .collect())
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/personal-login/reset-password",
    operation_id = "resetPersonalLoginPassword",
    tag = "members",
    summary = "Reset your personal login's password",
    description = "A new generated password, answered this once with `Cache-Control: no-store`; the old one is refused at once. It also renews the login for 90 days (an expired login comes back this way). A revoked login can't be reset: make a new one.",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The login and its new password, once", body = PersonalLoginSecret),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: you're below Developer on the database (`required` is `developer`); `member_suspended`: you're suspended; `member_offboarded`: your account was offboarded", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) doesn't exist, or you have no login there that isn't revoked", body = ErrorEnvelope),
        (status = 409, description = "`credentials_unavailable`: the main branch has no endpoint", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn reset_personal_login<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let authorized = tri!(
        authorize(
            &state,
            &principal,
            ep::RESET_PERSONAL_LOGIN.policy.access,
            Target::Database(id)
        )
        .await
    );
    let record = tri!(authorized.into_record());
    let member = tri!(caller_id(&principal));
    let endpoint = tri!(main_endpoint(&state, &record).await);
    let password = generate_password();
    let verifier = chelabase_neon::scram::verifier(&password);
    let (login, rows) = tri!(
        state
            .metadata
            .reset_personal_login(member, id, &verifier, valid_until())
            .await
            .map_err(|err| write_error("reset personal login", err))
    );
    // P8: the old verifier is gone from every row; the proxies forget what they cached.
    publish_login_rows(&state, rows);
    info!(request_id = %principal.request_id, member_id = %member, database_id = %id, role = %login.role_name, "personal login password reset");
    let ready_on_main = converge(&state, record.database_id, member, &login.role_name).await;
    let connection_string =
        connection_string(&state, &record, &endpoint, &login.role_name, &password);
    no_store(
        Json(PersonalLoginSecret {
            login: login.into(),
            password,
            connection_string,
            ready_on_main,
        })
        .into_response(),
    )
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/personal-logins/{member_id}",
    operation_id = "deletePersonalLogin",
    tag = "members",
    summary = "Delete a member's personal login",
    description = "Any status. The member themself, a tenant admin, or an Admin of the database's project may; for anyone else the login doesn't exist (404). The proxies forget it at once and each branch drops the role.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("member_id" = Uuid, Path, description = "The login's member"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`member_suspended`: the caller is suspended; `member_offboarded`: the caller's account was offboarded", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) doesn't exist, or there is no such login you may delete", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_personal_login<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, member_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let authorized = tri!(
        authorize(
            &state,
            &principal,
            ep::DELETE_PERSONAL_LOGIN.policy.access,
            Target::Database(id)
        )
        .await
    );
    let Some(caller) = principal.member.as_ref() else {
        return internal("personal login", "allowed a caller with no member row").into_response();
    };
    // Part 7: the member, a tenant admin (their effective access is Admin), or an Admin of the
    // database (overrides included). Anyone else learns nothing: the same 404 as no login.
    let admin = authorized
        .grant
        .as_ref()
        .is_some_and(|g| g.platform_role == PlatformRole::Admin);
    if caller.id != member_id && !caller.tenant_admin && !admin {
        return login_not_found().into_response();
    }
    let rows = tri!(
        state
            .metadata
            .delete_personal_login(caller.id, member_id, id)
            .await
            .map_err(|err| write_error("delete personal login", err))
    );
    info!(request_id = %principal.request_id, member_id = %member_id, database_id = %id, "personal login deleted");
    let mut branches: Vec<Uuid> = rows.iter().map(|r| r.branch_id).collect();
    branches.sort();
    branches.dedup();
    publish_login_rows(&state, rows);
    // The worker drops the role where it is (an unmarked record is an orphan there).
    for branch in branches {
        if let Err(err) = state.js.enqueue_member_roles(BranchId(branch)).await {
            warn!(branch_id = %branch, "member roles: enqueue failed; the sweep will reconcile it: {err}");
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    // MH-15: 32 characters from the 62 alphanumerics (≥ 128 bits), never the same twice, and
    // every character reachable (the rejection sampling keeps them equally likely).
    #[test]
    fn passwords_are_32_alphanumerics_and_differ() {
        let mut seen = std::collections::HashSet::new();
        let mut chars = std::collections::HashSet::new();
        for _ in 0..200 {
            let p = generate_password();
            assert_eq!(p.len(), 32, "{p}");
            assert!(p.chars().all(|c| c.is_ascii_alphanumeric()), "{p}");
            chars.extend(p.chars());
            assert!(seen.insert(p), "a password repeated");
        }
        assert_eq!(chars.len(), 62, "every alphanumeric shows up in 6400 draws");
    }

    // Final round A, T8-4: a secret answer's `Debug` (a `{:?}` in a log line, a panic message)
    // never shows the password or the connection string that carries it; the login's own
    // fields stay readable.
    #[test]
    fn a_secrets_debug_never_shows_the_password() {
        let password = generate_password();
        let secret = PersonalLoginSecret {
            login: PersonalLoginView::from(PersonalLogin {
                member_id: Uuid::nil(),
                database_id: Uuid::nil(),
                project_id: Uuid::nil(),
                database_name: "web1".into(),
                role_name: "alice_login".into(),
                valid_until: Utc::now(),
                created_at: Utc::now(),
                revoked_at: None,
                revoked_reason: None,
                status: LoginStatus::Active,
            }),
            password: password.clone(),
            connection_string: format!(
                "postgresql://alice_login:{password}@ep.example.test/web1?sslmode=verify-full"
            ),
            ready_on_main: true,
        };
        for shown in [format!("{secret:?}"), format!("{secret:#?}")] {
            assert!(!shown.contains(&password), "{shown}");
            assert!(shown.contains("alice_login"), "{shown}");
        }
    }
}
