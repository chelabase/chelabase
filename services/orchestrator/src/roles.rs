//! A branch's login roles: list, create, reset password, delete. The worker creates and
//! changes the roles inside the branch's compute; these handlers check every refusal first,
//! ask it over request/reply and translate its answer. Passwords are never in a response here;
//! they are revealed only by `GET …/connection?role=`.

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use chelabase_metadata::{
    BranchKind, BranchRecord, BranchStatus, DataKeyRecord, DatabaseRecord, PasswordState, RoleLevel,
    RoleOrigin, RoleRow,
};
use chelabase_nats::{
    ROLE_CREATE_NOT_APPLIED, ROLE_DELETE_NOT_APPLIED, ROLE_UPDATE_NOT_APPLIED, RoleCreateRequest, RoleReply,
    RoleRequest, RoleSessionsReply, RoleUpdateRequest,
};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::data_key::DataKeyKind;
use chelabase_types::role_name::{
    RoleNameError, check_any_role_name, check_login_role_name, is_membership_reserved, is_reserved_target,
};
use chelabase_types::{ApiJson, ApiPath};
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::branches::load_branch;
use crate::{
    AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, RESET_NOT_APPLIED, ResetError,
    load_owned, reset_branch_password,
};

/// Documents the `keys` extra of `role_in_use`, which no handler returns as a body type.
#[derive(OpenApi)]
#[openapi(components(schemas(RoleInUseKey)))]
struct RolesDoc;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::with_openapi(RolesDoc::openapi())
        .routes(routes!(list_roles, create_role))
        .routes(routes!(reset_role_password))
        .routes(routes!(delete_role, update_role))
        .routes(routes!(list_role_sessions))
}

/// A role of a branch, without secrets. The owner comes first.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct BranchRole {
    name: String,
    /// The branch's owner, a role made in SQL, or one made through the API.
    origin: RoleOrigin,
    /// `owner_level`: superuser, CREATEROLE, CREATEDB, REPLICATION, or a member of
    /// `neon_superuser` at any depth. Such a role can't back a data API secret key. BYPASSRLS
    /// alone isn't owner-level (see `bypass_rls`).
    level: RoleLevel,
    /// Whether the role has the LOGIN attribute.
    login: bool,
    /// Whether the password can be shown (`generated`), was set in SQL, or is an unusable MD5
    /// hash.
    password: PasswordState,
    /// Live data API secret keys that name this role while it is owner-level: the keys the
    /// data API refuses (empty for a plain role).
    blocked_keys: Vec<RoleInUseKey>,
    created_at: DateTime<Utc>,
    /// Written but not yet applied to the compute; applied when the compute next runs.
    pending: bool,
    /// INHERIT: the role uses its groups' privileges without `SET ROLE`.
    inherit: bool,
    /// The most connections the role may hold at once; `-1` is no limit.
    connection_limit: i32,
    /// When the role's password stops working; `null` is never.
    valid_until: Option<DateTime<Utc>>,
    /// BYPASSRLS on its own (an owner-level role has it too, through the owner's attributes).
    bypass_rls: bool,
    /// The roles it's a direct member of, by name (`neon_superuser` isn't listed).
    member_of: Vec<String>,
}

impl BranchRole {
    fn new(row: RoleRow, keys: &[DataKeyRecord]) -> Self {
        let blocked_keys = if row.level == RoleLevel::OwnerLevel {
            keys.iter()
                .filter(|k| k.kind == DataKeyKind::Secret && k.revoked_at.is_none() && k.role == row.name)
                .map(|k| RoleInUseKey { id: k.id, name: k.name.clone() })
                .collect()
        } else {
            Vec::new()
        };
        Self {
            name: row.name,
            origin: row.origin,
            level: row.level,
            login: row.login,
            password: row.password,
            blocked_keys,
            created_at: row.created_at,
            pending: row.pending,
            inherit: row.attributes.inherit,
            connection_limit: row.attributes.connection_limit,
            valid_until: row.attributes.valid_until,
            bypass_rls: row.attributes.bypass_rls,
            member_of: row.attributes.member_of,
        }
    }
}

fn yes() -> bool {
    true
}

fn no_connection_limit() -> i32 {
    -1
}

/// A field that tells "absent" (`None`) from `null` (`Some(None)`).
fn double_option<'de, D>(d: D) -> Result<Option<Option<DateTime<Utc>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<DateTime<Utc>>::deserialize(d).map(Some)
}

/// A role to create. Only the attributes listed here can be set; any other field is refused.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateRoleRequest {
    /// `^[a-z_][a-z0-9_]{0,62}$`, and not a reserved name (`postgres`, `anon`, `pg_…`, …).
    name: String,
    /// Makes the role owner-level, like the branch owner: CREATEROLE, CREATEDB, BYPASSRLS
    /// and REPLICATION, and a member of `neon_superuser`. Default `false`.
    #[serde(default)]
    owner_level: bool,
    /// LOGIN, with a generated password. `false` makes a NOLOGIN role (a group) with no
    /// password. Default `true`.
    #[serde(default = "yes")]
    login: bool,
    /// INHERIT. Default `true`.
    #[serde(default = "yes")]
    inherit: bool,
    /// `-1` (the default) is no limit.
    #[serde(default = "no_connection_limit")]
    connection_limit: i32,
    /// When the password stops working. Default: never.
    #[serde(default)]
    valid_until: Option<DateTime<Utc>>,
    /// BYPASSRLS: the role ignores row-level security. Default `false`.
    #[serde(default)]
    bypass_rls: bool,
    /// Roles to make it a member of. Each must exist and be the owner's to grant; never one of
    /// Chelabase's own (`authenticator`, `anon`, `authenticated`, `neon_superuser`,
    /// `chelabase_…`), the owner, a `pg_…` role, or the role itself.
    #[serde(default)]
    member_of: Vec<String>,
}

/// A change to a role: only the fields given change.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateRoleRequest {
    /// Owner-level on (CREATEROLE, CREATEDB, BYPASSRLS, REPLICATION, `neon_superuser`) or off.
    #[serde(default)]
    owner_level: Option<bool>,
    /// Turning LOGIN on generates a password (read it with the connection details and
    /// `role`); turning it off drops the password.
    #[serde(default)]
    login: Option<bool>,
    #[serde(default)]
    inherit: Option<bool>,
    /// `-1` is no limit.
    #[serde(default)]
    connection_limit: Option<i32>,
    /// `null` clears the expiry.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>, format = DateTime, nullable = true)]
    valid_until: Option<Option<DateTime<Utc>>>,
    #[serde(default)]
    bypass_rls: Option<bool>,
    /// The new list of the role's other memberships, with the create's rules
    /// (`pg_*` and Chelabase's own groups stay).
    #[serde(default)]
    member_of: Option<Vec<String>>,
}

/// The 422 for a membership the rules refuse.
fn membership_error(message: &str) -> ApiError {
    ApiError::new(422, ErrorCode::RoleMembershipNotAllowed, message)
}

/// U2's rules the API checks itself: a connection limit of `-1` or more, and no membership
/// in Chelabase's own roles, the owner, a `pg_…` role or the role itself. Whether a target
/// exists and is the owner's to grant, the worker checks.
fn check_settings(
    name: &str,
    owner: &str,
    connection_limit: Option<i32>,
    member_of: Option<&[String]>,
) -> Result<(), ApiError> {
    if connection_limit.is_some_and(|l| l < -1) {
        return Err(ApiError::new(
            422,
            ErrorCode::RoleAttributeNotAllowed,
            "connection_limit is -1 (no limit) or more",
        ));
    }
    for target in member_of.unwrap_or_default() {
        if check_any_role_name(target).is_err() {
            return Err(membership_error("a membership names an invalid role"));
        }
        if target == name || target == owner || is_membership_reserved(target) {
            return Err(membership_error(
                "memberships in the role itself, the owner, Chelabase's own roles and pg_ roles aren't allowed",
            ));
        }
    }
    Ok(())
}

/// A data API key that names an owner-level role; also the `keys` extra of `role_in_use`.
#[derive(Debug, Serialize, ToSchema)]
struct RoleInUseKey {
    id: Uuid,
    name: String,
}

/// The 422 for a name the rules refuse.
pub(crate) fn name_error(err: RoleNameError) -> ApiError {
    match err {
        RoleNameError::Invalid => ApiError::new(
            422,
            ErrorCode::InvalidRoleName,
            "role names are 1-63 characters from a-z, 0-9 and _, starting with a letter or _",
        ),
        RoleNameError::Reserved => ApiError::new(
            422,
            ErrorCode::ReservedRoleName,
            "that role name is reserved for the platform",
        ),
    }
}

/// The 422 for a name Postgres can't hold (empty, over 63 bytes, a NUL byte).
pub(crate) fn check_any_name(name: &str) -> Result<(), ApiError> {
    check_any_role_name(name).map_err(|_| {
        ApiError::new(
            422,
            ErrorCode::InvalidRoleName,
            "role names are 1-63 bytes and can't contain a NUL byte",
        )
    })
}

/// The owner's role name on this database.
pub(crate) fn owner_name(record: &DatabaseRecord) -> &str {
    record.owner_role.as_deref().unwrap_or("postgres")
}

/// A branch a role can be managed on: a ready `normal` branch with an endpoint. Returns the
/// branch and its endpoint id.
fn usable(branch: BranchRecord) -> Result<(BranchRecord, String), ApiError> {
    if branch.kind == BranchKind::Backup {
        return Err(ApiError::new(409, ErrorCode::BackupBranch, BACKUP_REFUSAL));
    }
    if branch.status != BranchStatus::Ready {
        return Err(ApiError::new(409, ErrorCode::BranchNotReady, "branch is not ready"));
    }
    match branch.endpoint_id.clone() {
        Some(endpoint) => Ok((branch, endpoint)),
        None => Err(ApiError::new(
            409,
            ErrorCode::CredentialsUnavailable,
            "credentials unavailable; recreate this database",
        )),
    }
}

/// The database and the usable branch with its endpoint id.
async fn load_target<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
    branch_id: Uuid,
) -> Result<(DatabaseRecord, BranchRecord, String), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let record = load_owned(state, principal, id).await?;
    let branch = load_branch(state, &record, branch_id).await?;
    let (branch, endpoint) = usable(branch)?;
    Ok((record, branch, endpoint))
}

/// The worker's refusal or failure as the API's error; `not_applied` is the operation's own
/// `not_applied` message.
fn reply_error(reply: &RoleReply, what: &str, not_applied: &'static str) -> ApiError {
    let code = reply.code.as_deref().unwrap_or("failed");
    match code {
        "invalid_role_name" => name_error(RoleNameError::Invalid),
        "reserved_role_name" => name_error(RoleNameError::Reserved),
        "role_exists" => ApiError::new(409, ErrorCode::RoleExists, "a role with that name already exists"),
        "role_cap_reached" => ApiError::new(
            409,
            ErrorCode::RoleCapReached,
            "this branch has reached its limit of login roles; delete one first",
        ),
        "branch_not_ready" => ApiError::new(409, ErrorCode::BranchNotReady, "branch is not ready"),
        "role_in_use" => ApiError::new(
            409,
            ErrorCode::RoleInUse,
            "the role is used by data API keys; revoke or rotate them first",
        )
        .with_extra(
            "keys",
            serde_json::Value::Array(
                reply
                    .keys
                    .iter()
                    .map(|k| serde_json::json!({ "id": k.id, "name": k.name }))
                    .collect(),
            ),
        ),
        "not_found" => ApiError::not_found("role not found"),
        // The worker's message names the target (the caller's own input).
        "role_membership_not_allowed" => membership_error(reply.error.as_deref().unwrap_or(
            "a membership names a role that doesn't exist, isn't the owner's to grant, or is reserved",
        )),
        "role_attribute_not_allowed" => ApiError::new(
            422,
            ErrorCode::RoleAttributeNotAllowed,
            "a role attribute is outside the allowed values",
        ),
        "role_privilege_mismatch" => {
            error!("{what}: role privilege mismatch: {:?}", reply.error);
            ApiError::new(
                500,
                ErrorCode::RolePrivilegeMismatch,
                "the role's privileges in the database don't match what Chelabase recorded",
            )
        }
        "not_applied" => ApiError::new(503, ErrorCode::NotApplied, not_applied),
        _ => {
            error!("{what} failed: {code}: {:?}", reply.error);
            ApiError::internal()
        }
    }
}

/// The result of one role RPC: `Ok` when the worker did it.
fn rpc_result(
    result: Result<RoleReply, ResetError>,
    what: &str,
    not_applied: &'static str,
) -> Result<(), ApiError> {
    match result {
        Ok(reply) if reply.ok => Ok(()),
        Ok(reply) => Err(reply_error(&reply, what, not_applied)),
        Err(ResetError::Timeout) => {
            Err(ApiError::new(504, ErrorCode::Timeout, "worker did not respond; try again"))
        }
        Err(ResetError::Transport(err)) => {
            error!("{what}: {err}");
            Err(ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable"))
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/roles",
    operation_id = "listRoles",
    tag = "roles",
    summary = "List a branch's roles",
    description = "The branch's owner role first, then the others by name: roles made through the API and roles made in SQL, with their level, password state and the data API secret keys an owner-level role blocks. No passwords: read one with the connection details and `role`.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 200, description = "The roles, owner first", body = Vec<BranchRole>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_roles<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (_, branch, endpoint) = match load_target(&state, &principal, id, branch_id).await {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    let rows = match state.metadata.list_roles(&endpoint).await {
        Ok(rows) => rows,
        Err(err) => {
            error!("list roles: {err}");
            return ApiError::internal().into_response();
        }
    };
    let keys = match state.metadata.list_data_keys(branch.id).await {
        Ok(keys) => keys,
        Err(err) => {
            error!("list data keys for roles: {err}");
            return ApiError::internal().into_response();
        }
    };
    Json(rows.into_iter().map(|row| BranchRole::new(row, &keys)).collect::<Vec<_>>()).into_response()
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/roles",
    operation_id = "createRole",
    tag = "roles",
    summary = "Create a role",
    description = "Creates a role and answers with it. A LOGIN role (the default) gets a generated password; `login: false` makes a NOLOGIN role (a group) with none. `owner_level` gives it the owner's attributes (CREATEROLE, CREATEDB, BYPASSRLS, REPLICATION) and makes it a member of `neon_superuser`. `inherit`, `connection_limit`, `valid_until`, `bypass_rls` and `member_of` set the other allowed attributes and memberships; nothing else can be set. The password isn't in the answer: read it with the connection details and `role`. A role the compute couldn't take yet is `pending` and applied later.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    request_body = CreateRoleRequest,
    responses(
        (status = 201, description = "The role was created", body = BranchRole),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't valid JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`role_exists`: the name is taken; `role_cap_reached`: the branch has its maximum of login roles; `branch_not_ready`: the branch isn't ready; `backup_branch`: a restore backup has no compute; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 422, description = "`invalid_role_name`: the name doesn't fit the rules; `reserved_role_name`: the name is reserved; `role_membership_not_allowed`: a membership in a reserved role, the owner, the role itself, a role that doesn't exist or isn't the owner's to grant, or a role that is itself a member (at any depth) of a reserved role; `role_attribute_not_allowed`: a connection limit under -1; `invalid_request`: an unknown field (an attribute that can't be set, or `privileged`: use `owner_level`) or a missing one", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed", body = ErrorEnvelope),
        (status = 503, description = "`not_applied`: the role is stored but not applied yet and will be retried; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn create_role<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<CreateRoleRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (record, branch, endpoint) = match load_target(&state, &principal, id, branch_id).await {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = check_login_role_name(&request.name) {
        return name_error(err).into_response();
    }
    if let Err(err) = check_settings(
        &request.name,
        owner_name(&record),
        Some(request.connection_limit),
        Some(&request.member_of),
    ) {
        return err.into_response();
    }
    let reply = state
        .js
        .create_role(RoleCreateRequest {
            login: request.login,
            inherit: request.inherit,
            connection_limit: request.connection_limit,
            valid_until: request.valid_until,
            bypass_rls: request.bypass_rls,
            member_of: request.member_of.clone(),
            ..RoleCreateRequest::login(branch.id, &request.name, request.owner_level)
        })
        .await;
    if let Err(err) = rpc_result(reply, "create role", ROLE_CREATE_NOT_APPLIED) {
        return err.into_response();
    }
    info!(
        request_id = %principal.request_id,
        branch_id = %branch.id,
        role = %request.name,
        owner_level = request.owner_level,
        "login role created"
    );
    match state.metadata.list_roles(&endpoint).await {
        Ok(rows) => match rows.into_iter().find(|r| r.name == request.name) {
            Some(row) => (StatusCode::CREATED, Json(BranchRole::new(row, &[]))).into_response(),
            None => {
                error!(branch_id = %branch.id, role = %request.name, "created role is not listed");
                ApiError::internal().into_response()
            }
        },
        Err(err) => {
            error!("list roles after create: {err}");
            ApiError::internal().into_response()
        }
    }
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/roles/{role}/reset-password",
    operation_id = "resetRolePassword",
    tag = "roles",
    summary = "Reset a role's password",
    description = "Generates a new password for the role. The owner's name resets the owner password, like the branch's reset-password. Read the new password with the connection details and `role`.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("role" = String, Path, description = "Role name; the owner's name resets the owner password"),
    ),
    responses(
        (status = 204, description = "The password was reset"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the role doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`branch_not_ready`: the branch isn't ready; `backup_branch`: a restore backup has no compute; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 422, description = "`invalid_role_name`: the name is empty, over 63 bytes or has a NUL byte", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed to reset the password", body = ErrorEnvelope),
        (status = 503, description = "`not_applied`: the new password is stored but not applied yet and will be retried; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn reset_role_password<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, role)): ApiPath<(Uuid, Uuid, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(err) => return err.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(err) => return err.into_response(),
    };
    if role == owner_name(&record) {
        return reset_branch_password(&state, &principal, &record, Some(branch)).await;
    }
    let (branch, _) = match usable(branch) {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = check_any_name(&role) {
        return err.into_response();
    }
    let reply = state
        .js
        .reset_role(RoleRequest { branch_id: branch.id, name: role.clone() })
        .await;
    if let Err(err) = rpc_result(reply, "reset role password", RESET_NOT_APPLIED) {
        return err.into_response();
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, %role, "login role password reset");
    StatusCode::NO_CONTENT.into_response()
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}/roles/{role}",
    operation_id = "deleteRole",
    tag = "roles",
    summary = "Delete a login role",
    description = "Deletes a login role. Refused while a data API key uses it (`role_in_use`, with the keys in `keys`). The owner can't be deleted.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("role" = String, Path, description = "Role name"),
    ),
    responses(
        (status = 204, description = "The role was deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the role doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`role_in_use`: data API keys still use the role, listed in `keys` as `{id, name}`; `branch_not_ready`: the branch isn't ready; `backup_branch`: a restore backup has no compute; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 422, description = "`reserved_role_name`: the owner can't be deleted; `invalid_role_name`: the name is empty, over 63 bytes or has a NUL byte", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed to delete the role", body = ErrorEnvelope),
        (status = 503, description = "`not_applied`: the change is stored but not applied yet and will be retried; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn delete_role<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, role)): ApiPath<(Uuid, Uuid, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (record, branch, _) = match load_target(&state, &principal, id, branch_id).await {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if role == owner_name(&record) {
        return name_error(RoleNameError::Reserved).into_response();
    }
    if let Err(err) = check_any_name(&role) {
        return err.into_response();
    }
    let reply = state
        .js
        .delete_role(RoleRequest { branch_id: branch.id, name: role.clone() })
        .await;
    if let Err(err) = rpc_result(reply, "delete role", ROLE_DELETE_NOT_APPLIED) {
        return err.into_response();
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, %role, "login role deleted");
    StatusCode::NO_CONTENT.into_response()
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/branches/{branch_id}/roles/{role}",
    operation_id = "updateRole",
    tag = "roles",
    summary = "Change a role",
    description = "Changes a role's LOGIN, owner level, attributes or memberships; fields left out stay as they are, and `valid_until: null` clears the expiry. Turning LOGIN on generates a password (read it with the connection details and `role`); turning it off drops the password. `member_of` replaces the list of the role's other memberships (`pg_*` and Chelabase's own groups stay). Works on roles made through the API, and on roles made in SQL that the owner administers; the owner can't be changed. A change starts from the role as SQL left it: SQL controls every role's attributes and memberships. Turning LOGIN on counts toward the branch's login-role cap. Answers with the role, never a password.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("role" = String, Path, description = "Role name"),
    ),
    request_body = UpdateRoleRequest,
    responses(
        (status = 202, description = "The change was made; the role as it is now (no body for a role made in SQL that has no password, which the list doesn't show)", body = BranchRole),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't valid JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the role doesn't exist, or the role was made in SQL by a role other than the owner (the owner doesn't administer it)", body = ErrorEnvelope),
        (status = 409, description = "`role_cap_reached`: turning LOGIN on would give the branch more login roles than its maximum; `branch_not_ready`: the branch isn't ready; `backup_branch`: a restore backup has no compute; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 422, description = "`reserved_role_name`: the owner, a reserved role or any `chelabase_` role; `invalid_role_name`: the name is empty, over 63 bytes or has a NUL byte; `role_membership_not_allowed`: a membership in a reserved role, the owner, the role itself, a role that doesn't exist or isn't the owner's to grant, or a role that is itself a member (at any depth) of a reserved role; `role_attribute_not_allowed`: a connection limit under -1; `invalid_request`: an unknown field (an attribute that can't be set)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed to change the role", body = ErrorEnvelope),
        (status = 503, description = "`not_applied`: the change is stored but not applied yet and will be retried; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn update_role<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, role)): ApiPath<(Uuid, Uuid, String)>,
    ApiJson(request): ApiJson<UpdateRoleRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (record, branch, endpoint) = match load_target(&state, &principal, id, branch_id).await {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = check_any_name(&role) {
        return err.into_response();
    }
    let owner = owner_name(&record);
    if role == owner || is_reserved_target(&role, owner) {
        return name_error(RoleNameError::Reserved).into_response();
    }
    if let Err(err) = check_settings(&role, owner, request.connection_limit, request.member_of.as_deref()) {
        return err.into_response();
    }
    let reply = state
        .js
        .update_role(RoleUpdateRequest {
            owner_level: request.owner_level,
            login: request.login,
            inherit: request.inherit,
            connection_limit: request.connection_limit,
            valid_until: request.valid_until,
            bypass_rls: request.bypass_rls,
            member_of: request.member_of,
            ..RoleUpdateRequest::new(branch.id, &role)
        })
        .await;
    if let Err(err) = rpc_result(reply, "update role", ROLE_UPDATE_NOT_APPLIED) {
        return err.into_response();
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, %role, "role changed");
    let keys = state.metadata.list_data_keys(branch.id).await.unwrap_or_default();
    match state.metadata.list_roles(&endpoint).await {
        Ok(rows) => match rows.into_iter().find(|r| r.name == role) {
            Some(row) => (StatusCode::ACCEPTED, Json(BranchRole::new(row, &keys))).into_response(),
            // A role made in SQL without a password has no row: the change is done all the same.
            None => StatusCode::ACCEPTED.into_response(),
        },
        Err(err) => {
            error!("list roles after update: {err}");
            ApiError::internal().into_response()
        }
    }
}

/// The live sessions of a role on the branch's compute (E29a).
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct RoleSessions {
    /// Whether the branch's compute is running. When it isn't, nothing was read (the read
    /// never starts it): `count` is 0 and `sessions` is empty.
    compute_running: bool,
    /// How many sessions are open as the role right now.
    count: usize,
    /// The sessions, oldest first.
    sessions: Vec<RoleSessionView>,
}

/// One session open as the role (a `pg_stat_activity` row).
#[derive(Debug, Serialize, ToSchema)]
struct RoleSessionView {
    /// The client's address; `null` for a connection from inside the compute.
    #[schema(required = true)]
    client_addr: Option<String>,
    /// The client's `application_name` (empty when it set none).
    application_name: String,
    /// When the session started.
    backend_start: DateTime<Utc>,
}

/// The worker's sessions answer as the API's response or error.
fn sessions_result(result: Result<RoleSessionsReply, ResetError>) -> Result<RoleSessions, ApiError> {
    match result {
        Ok(reply) if reply.ok => Ok(RoleSessions {
            compute_running: reply.compute_running,
            count: reply.sessions.len(),
            sessions: reply
                .sessions
                .into_iter()
                .map(|s| RoleSessionView {
                    client_addr: s.client_addr,
                    application_name: s.application_name,
                    backend_start: s.backend_start,
                })
                .collect(),
        }),
        Ok(reply) if reply.code.as_deref() == Some("branch_not_ready") => {
            Err(ApiError::new(409, ErrorCode::BranchNotReady, "branch is not ready"))
        }
        Ok(reply) => {
            error!("role sessions failed: {:?}: {:?}", reply.code, reply.error);
            Err(ApiError::internal())
        }
        Err(ResetError::Timeout) => {
            Err(ApiError::new(504, ErrorCode::Timeout, "worker did not respond; try again"))
        }
        Err(ResetError::Transport(err)) => {
            error!("role sessions: {err}");
            Err(ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable"))
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/roles/{role}/sessions",
    operation_id = "listRoleSessions",
    tag = "roles",
    summary = "List a role's live sessions",
    description = "The sessions open as the role on the branch's compute right now: the count, and each client's address, `application_name` and start time, oldest first. A read only: when the compute isn't running, the answer is `compute_running: false` with no sessions, and the compute isn't started. Clients that aren't connected at this moment aren't listed. A name with no role has no sessions.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("role" = String, Path, description = "Role name"),
    ),
    responses(
        (status = 200, description = "The role's live sessions", body = RoleSessions),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 422, description = "`invalid_role_name`: the name is empty, over 63 bytes or has a NUL byte", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker couldn't read the sessions; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn list_role_sessions<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, role)): ApiPath<(Uuid, Uuid, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (_, branch, _) = match load_target(&state, &principal, id, branch_id).await {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = check_any_name(&role) {
        return err.into_response();
    }
    let reply = state.js.role_sessions(RoleRequest { branch_id: branch.id, name: role }).await;
    match sessions_result(reply) {
        Ok(sessions) => Json(sessions).into_response(),
        Err(err) => err.into_response(),
    }
}
