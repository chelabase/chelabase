//! A branch's login roles: list, create, reset password, delete. The worker creates and
//! changes the roles inside the branch's compute; these handlers check every refusal first,
//! ask it over request/reply and translate its answer. Passwords are never in a response here;
//! they are revealed only by `GET …/connection?role=`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use chelabase_metadata::members::AccessAction;
use chelabase_metadata::personal_logins::AccessEventInput;
use chelabase_metadata::{
    BranchKind, BranchRecord, BranchStatus, DataKeyRecord, DatabaseRecord, PasswordState, RoleLevel,
    RoleOrigin, RoleRow,
};
use chelabase_nats::{
    QUARANTINE_SUCCESSOR, ROLE_CREATE_NOT_APPLIED, ROLE_DELETE_NOT_APPLIED, ROLE_UPDATE_NOT_APPLIED,
    RoleCreateRequest, RoleDeleteRequest, RoleReply, RoleRequest, RoleSessionsReply, RoleUpdateRequest,
};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::data_key::DataKeyKind;
use chelabase_types::role_name::{
    RoleNameError, check_any_role_name, check_login_role_name, is_membership_reserved, is_reserved_target,
};
use chelabase_types::{ApiJson, ApiPath, BranchId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{error, info, warn};
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{authorize_database, is_personal_login, Target};
use crate::branches::load_branch;
use crate::privileges::check_all_rows;
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::Access;
use crate::{
    AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, RESET_NOT_APPLIED, ResetError,
    reset_branch_password,
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
        .routes(routes!(preview_role_delete))
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
    /// Those of `member_of` without the SET option: the role can't `SET ROLE` to them, though
    /// it still inherits their privileges (I2).
    member_no_set: Vec<String>,
    /// Those of `member_of` with the ADMIN option: the role can grant the group on (I2). A role
    /// holding it is Admin-only to grant (R30).
    member_admin: Vec<String>,
    /// Listed in the branch's guard exemptions (by name): on a protected branch with "guard
    /// login roles" on, this login role may still change the schema.
    guard_exempt: bool,
}

impl BranchRole {
    fn new(row: RoleRow, keys: &[DataKeyRecord], exemptions: &[String]) -> Self {
        let guard_exempt = exemptions.contains(&row.name);
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
            member_no_set: row.attributes.member_no_set,
            member_admin: row.attributes.member_admin,
            guard_exempt,
        }
    }
}

/// The branch's guard exemptions for the role views; a failed read is logged and is a 500
/// (callers after a write fall back to none).
async fn exemptions_of<M, J>(state: &AppState<M, J>, branch: BranchId) -> Result<Vec<String>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    state.metadata.list_guard_exemptions(branch).await.map_err(|err| {
        error!(branch_id = %branch, "list guard exemptions for roles: {err}");
        ApiError::internal()
    })
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
    access: Access,
    id: Uuid,
    branch_id: Uuid,
) -> Result<(DatabaseRecord, BranchRecord, String), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let record = authorize_database(state, principal, access, Target::Branch { database: id, branch: branch_id }).await?;
    let branch = load_branch(state, &record, branch_id).await?;
    let (branch, endpoint) = usable(branch)?;
    Ok((record, branch, endpoint))
}

/// MH-11: a personal login is absent from every generic role route (it is managed through its
/// own routes only): 404 like a role that isn't there, before the worker is asked.
async fn not_personal<M, J>(state: &AppState<M, J>, record: &DatabaseRecord, role: &str) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    if is_personal_login(state, record, role).await? {
        return Err(ApiError::not_found("role not found"));
    }
    Ok(())
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
        "invalid_successor" => invalid_successor(),
        // T10 fix: the exposures as they are now, and the token that confirms them.
        "exposure_unconfirmed" => {
            let preview = reply.preview.clone().unwrap_or_default();
            ApiError::new(
                409,
                ErrorCode::ExposureUnconfirmed,
                "the delete would let other roles run routines or read views with the successor's rights; review `exposures` and delete again with `confirm`",
            )
            .with_extra("exposures", serde_json::to_value(&preview.exposures).unwrap_or_default())
            .with_extra("confirm", preview.confirm.map_or(Value::Null, Value::String))
        }
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

/// Whether a role call gets an event (P25, R-T9a): `Some(true)` when the worker did it,
/// `Some(false)` when it stored the change for a retry (`not_applied`) or didn't answer in time
/// (T9-m1: the change may have been made), `None` for a refusal or a transport failure (no
/// event).
fn event_applied(result: &Result<RoleReply, ResetError>) -> Option<bool> {
    match result {
        Ok(reply) if reply.ok => Some(true),
        Ok(reply) if reply.code.as_deref() == Some("not_applied") => Some(false),
        Err(ResetError::Timeout) => Some(false),
        _ => None,
    }
}

/// T9-m3: what a deleted role was, for its event: `login` and the attributes the API sets.
fn role_was(row: &chelabase_metadata::RoleRow) -> Value {
    let a = &row.attributes;
    json!({
        "login": row.login,
        "attributes": {
            "owner_level": row.level == chelabase_metadata::RoleLevel::OwnerLevel,
            "inherit": a.inherit,
            "connection_limit": a.connection_limit,
            "valid_until": a.valid_until,
            "bypass_rls": a.bypass_rls,
            "member_of": a.member_of,
        },
    })
}

/// Writes the role event of a change, `after` plus `applied`, with the project, database and
/// branch (MH-37). Never a password. A failed write is logged: the change is made, so the
/// request still answers as it did.
async fn log_role_event<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: &DatabaseRecord,
    branch: BranchId,
    action: AccessAction,
    mut after: Value,
    applied: bool,
) where
    M: MetadataPort,
{
    after["applied"] = Value::Bool(applied);
    let event = AccessEventInput {
        tenant: principal.tenant_id,
        actor: principal.member.as_ref().map(|m| m.id),
        action,
        target: None,
        project: Some(record.project_id.as_uuid()),
        database: Some(record.database_id.as_uuid()),
        branch: Some(branch.as_uuid()),
        before: None,
        after: Some(&after),
    };
    if let Err(err) = state.metadata.record_access_event(&event).await {
        warn!(branch_id = %branch, "role event not recorded: {err}");
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
    let (_, branch, endpoint) = match load_target(&state, &principal, ep::LIST_ROLES.policy.access, id, branch_id).await {
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
    let exemptions = match exemptions_of(&state, branch.id).await {
        Ok(exemptions) => exemptions,
        Err(err) => return err.into_response(),
    };
    // MH-11: personal logins aren't listed (their rows are `personal`).
    Json(
        rows.into_iter()
            .filter(|row| row.origin != RoleOrigin::Personal)
            .map(|row| BranchRole::new(row, &keys, &exemptions))
            .collect::<Vec<_>>(),
    )
    .into_response()
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
    let (record, branch, endpoint) = match load_target(&state, &principal, ep::CREATE_ROLE.policy.access, id, branch_id).await {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = check_login_role_name(&request.name) {
        return name_error(err).into_response();
    }
    // S11, D3: a personal login's name is taken on every branch of the database, also where
    // its role isn't made yet.
    match is_personal_login(&state, &record, &request.name).await {
        Ok(false) => {}
        Ok(true) => {
            return ApiError::new(409, ErrorCode::RoleExists, "a role with that name already exists").into_response();
        }
        Err(err) => return err.into_response(),
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
    if let Some(applied) = event_applied(&reply) {
        let created = json!({
            "role": request.name,
            "login": request.login,
            "attributes": {
                "owner_level": request.owner_level,
                "inherit": request.inherit,
                "connection_limit": request.connection_limit,
                "valid_until": request.valid_until,
                "bypass_rls": request.bypass_rls,
                "member_of": request.member_of,
            },
        });
        log_role_event(&state, &principal, &record, branch.id, AccessAction::RoleCreated, created, applied).await;
    }
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
    // The role exists now: a failed read of the exemptions must not answer the create with a
    // 500 (a retry would find the name taken), so it shows none.
    let exemptions = exemptions_of(&state, branch.id).await.unwrap_or_default();
    match state.metadata.list_roles(&endpoint).await {
        Ok(rows) => match rows.into_iter().find(|r| r.name == request.name) {
            Some(row) => (StatusCode::CREATED, Json(BranchRole::new(row, &[], &exemptions))).into_response(),
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
    let record = match authorize_database(&state, &principal, ep::RESET_ROLE.policy.access, Target::Branch { database: id, branch: branch_id }).await {
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
    if let Err(err) = not_personal(&state, &record, &role).await {
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
    description = "Deletes a role. Its objects in every database go to `successor` (never the owner): a role the owner can `SET ROLE` to that isn't owner-level, reserved or a personal login, or `chelabase_orphaned` (the default), a powerless role. Databases it owns go to the owner. PUBLIC loses EXECUTE on every routine that moved, and the answer lists the `SECURITY DEFINER` ones for review. Refused while a data API key uses it (`role_in_use`, with the keys in `keys`). The owner can't be deleted. A named successor that would let other roles run the role's `SECURITY DEFINER` routines or read its views without `security_invoker` with the successor's rights needs `confirm`, the current token of the delete's preview (`GET …/roles/{role}/delete-preview`); the grants are kept. `chelabase_orphaned` never needs one.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("role" = String, Path, description = "Role name"),
        DeleteRoleQuery,
    ),
    responses(
        (status = 200, description = "The role was deleted; what went to the successor", body = DeletedRole),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the role doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`role_in_use`: data API keys still use the role, listed in `keys` as `{id, name}`; `exposure_unconfirmed`: the move to the named successor exposes routines or views to other roles and `confirm` isn't the current token, with the current list in `exposures` (as the preview's) and its token in `confirm`; nothing changed, unless the exposures changed while the delete ran, in which case databases already done stay done and the role is left without LOGIN; `branch_not_ready`: the branch isn't ready; `backup_branch`: a restore backup has no compute; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 422, description = "`reserved_role_name`: the owner can't be deleted; `invalid_role_name`: the name is empty, over 63 bytes or has a NUL byte; `invalid_successor`: the successor is the role itself, the owner, reserved, a personal login, owner-level, a role the owner can't SET to, or doesn't exist; `invalid_request`: the query string is malformed (a parameter given twice, say)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed to delete the role", body = ErrorEnvelope),
        (status = 503, description = "`not_applied`: the change is stored but not applied yet and will be retried; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn delete_role<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, role)): ApiPath<(Uuid, Uuid, String)>,
    query: Result<axum::extract::Query<DeleteRoleQuery>, axum::extract::rejection::QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (record, branch, endpoint) = match load_target(&state, &principal, ep::DELETE_ROLE.policy.access, id, branch_id).await {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if role == owner_name(&record) {
        return name_error(RoleNameError::Reserved).into_response();
    }
    if let Err(err) = check_any_name(&role) {
        return err.into_response();
    }
    if let Err(err) = not_personal(&state, &record, &role).await {
        return err.into_response();
    }
    let Ok(axum::extract::Query(DeleteRoleQuery { successor, confirm })) = query else {
        return ApiError::new(422, ErrorCode::InvalidRequest, "the query string is malformed").into_response();
    };
    let successor = successor.unwrap_or_else(|| QUARANTINE_SUCCESSOR.to_string());
    if let Err(err) = check_successor(&state, &endpoint, owner_name(&record), &role, &successor).await {
        return err.into_response();
    }
    // T9-m3: the event says what was deleted, as it was before the delete. A failed read only
    // leaves those fields out.
    let was = match state.metadata.list_roles(&endpoint).await {
        Ok(rows) => rows.iter().find(|r| r.name == role).map(role_was),
        Err(err) => {
            warn!(branch_id = %branch.id, %role, "delete role: the role's row isn't readable for its event: {err}");
            None
        }
    };
    let reply = state
        .js
        .delete_role(RoleDeleteRequest {
            branch_id: branch.id,
            name: role.clone(),
            successor: successor.clone(),
            confirm,
        })
        .await;
    if let Some(applied) = event_applied(&reply) {
        let confirmed = match &reply {
            Ok(reply) => reply.reassigned.as_ref().map_or(0, |r| r.confirmed.len()),
            Err(_) => 0,
        };
        let mut deleted = json!({ "role": role, "successor": successor, "exposures_confirmed": confirmed });
        if let Some(Value::Object(was)) = &was {
            for (key, value) in was {
                deleted[key] = value.clone();
            }
        }
        log_role_event(&state, &principal, &record, branch.id, AccessAction::RoleDeleted, deleted, applied).await;
    }
    let reassigned = match &reply {
        Ok(reply) => reply.reassigned.clone(),
        Err(_) => None,
    };
    if let Err(err) = rpc_result(reply, "delete role", ROLE_DELETE_NOT_APPLIED) {
        return err.into_response();
    }
    // T10-4: a done delete always says what it reassigned. A worker older than the successor
    // ignores it, hands the objects to the owner and answers without `reassigned`: never a
    // 200 claiming they went to the successor.
    let Some(reassigned) = reassigned else {
        error!(
            request_id = %principal.request_id, branch_id = %branch.id, %role, %successor,
            "role delete: the worker answered without what it reassigned (a worker older than the successor); the objects may have gone to the owner"
        );
        return ApiError::internal().into_response();
    };
    info!(
        request_id = %principal.request_id, branch_id = %branch.id, %role, %successor,
        reassigned = reassigned.count, definers = reassigned.definers.len(), "login role deleted"
    );
    let body = DeletedRole {
        successor,
        reassigned: ReassignedObjects { count: reassigned.count, definers: reassigned.definers },
    };
    (StatusCode::OK, Json(body)).into_response()
}

/// `?successor=&confirm=` on a role delete.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct DeleteRoleQuery {
    /// The role the deleted role's objects go to: a role the owner can `SET ROLE` to that isn't
    /// owner-level, reserved or a personal login, or `chelabase_orphaned` (the default), a
    /// powerless role. Its databases go to the owner.
    successor: Option<String>,
    /// The `confirm` token of the delete's preview (`GET …/roles/{role}/delete-preview`), when
    /// the move to a named successor exposes routines or views to other roles. Not needed for
    /// `chelabase_orphaned`, nor when nothing is exposed.
    confirm: Option<String>,
}

/// `?successor=` on a role delete's preview.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct PreviewRoleDeleteQuery {
    /// The successor the delete would name, with the delete's rules; `chelabase_orphaned` (the
    /// default) when omitted.
    successor: Option<String>,
}

/// What deleting a role to `successor` would expose: routines and views that would run with
/// the successor's rights for other roles.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct RoleDeletePreview {
    /// The successor previewed.
    successor: String,
    /// Sorted by database, kind, identity. Empty when nothing is exposed.
    exposures: Vec<RoleExposure>,
    /// The token to pass as the delete's `confirm`; `null` when there is nothing to confirm
    /// (no exposures, or `chelabase_orphaned`, which never needs one). It changes whenever the
    /// exposures do.
    confirm: Option<String>,
}

/// An object the deleted role owns that would run with the successor's rights for others: a
/// `SECURITY DEFINER` routine granted to roles other than the successor (PUBLIC's EXECUTE is
/// revoked on every routine that moves), or a view without `security_invoker` (or a
/// materialized view) granted to anyone but the successor, PUBLIC included.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct RoleExposure {
    /// The database it lives in.
    database: String,
    kind: RoleExposureKind,
    /// `schema.name(argument types)` for a routine, `schema.name` for a view, quoted as SQL
    /// quotes it.
    identity: String,
    /// The roles granted a privilege on it (`PUBLIC` for everyone), sorted.
    grantees: Vec<String>,
}

/// The kind of an exposed object.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RoleExposureKind {
    Function,
    Procedure,
    View,
    MaterializedView,
}

impl From<chelabase_nats::ExposureKind> for RoleExposureKind {
    fn from(kind: chelabase_nats::ExposureKind) -> Self {
        use chelabase_nats::ExposureKind as K;
        match kind {
            K::Function => Self::Function,
            K::Procedure => Self::Procedure,
            K::View => Self::View,
            K::MaterializedView => Self::MaterializedView,
        }
    }
}

impl From<chelabase_nats::DeletePreview> for RoleDeletePreview {
    fn from(p: chelabase_nats::DeletePreview) -> Self {
        Self {
            successor: p.successor,
            exposures: p
                .exposures
                .into_iter()
                .map(|e| RoleExposure {
                    database: e.database,
                    kind: e.kind.into(),
                    identity: e.identity,
                    grantees: e.grantees,
                })
                .collect(),
            confirm: p.confirm,
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/roles/{role}/delete-preview",
    operation_id = "previewRoleDelete",
    tag = "roles",
    summary = "Preview a role delete",
    description = "What deleting the role to `successor` would expose: its `SECURITY DEFINER` routines granted to roles other than the successor, and its views without `security_invoker` (and materialized views) granted to anyone but the successor, PUBLIC included. After the delete they run with the successor's rights for those roles. Review them, then pass `confirm` to the delete; a delete to a named successor that exposes something is refused without the current token (`exposure_unconfirmed`). `chelabase_orphaned` (the default) never needs one: its preview lists what it finds with `confirm: null`. Changes nothing. The delete's rules for the role and the successor apply.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("role" = String, Path, description = "Role name"),
        PreviewRoleDeleteQuery,
    ),
    responses(
        (status = 200, description = "What the delete would expose, and the token that confirms it", body = RoleDeletePreview),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: the caller isn't an Admin of the project", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the role doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`branch_not_ready`: the branch isn't ready; `backup_branch`: a restore backup has no compute; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 422, description = "`reserved_role_name`: the owner can't be deleted; `invalid_role_name`: the name is empty, over 63 bytes or has a NUL byte; `invalid_successor`: the successor is the role itself, the owner, reserved, a personal login, owner-level, a role the owner can't SET to, or doesn't exist; `invalid_request`: the query string is malformed", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed to read the exposures", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn preview_role_delete<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, role)): ApiPath<(Uuid, Uuid, String)>,
    query: Result<axum::extract::Query<PreviewRoleDeleteQuery>, axum::extract::rejection::QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (record, branch, endpoint) =
        match load_target(&state, &principal, ep::PREVIEW_ROLE_DELETE.policy.access, id, branch_id).await {
            Ok(target) => target,
            Err(err) => return err.into_response(),
        };
    if role == owner_name(&record) {
        return name_error(RoleNameError::Reserved).into_response();
    }
    if let Err(err) = check_any_name(&role) {
        return err.into_response();
    }
    if let Err(err) = not_personal(&state, &record, &role).await {
        return err.into_response();
    }
    let Ok(axum::extract::Query(PreviewRoleDeleteQuery { successor })) = query else {
        return ApiError::new(422, ErrorCode::InvalidRequest, "the query string is malformed").into_response();
    };
    let successor = successor.unwrap_or_else(|| QUARANTINE_SUCCESSOR.to_string());
    if let Err(err) = check_successor(&state, &endpoint, owner_name(&record), &role, &successor).await {
        return err.into_response();
    }
    let reply = state
        .js
        .preview_role_delete(RoleDeleteRequest {
            branch_id: branch.id,
            name: role.clone(),
            successor: successor.clone(),
            confirm: None,
        })
        .await;
    let preview = match &reply {
        Ok(reply) if reply.ok => reply.preview.clone(),
        _ => None,
    };
    if let Err(err) = rpc_result(reply, "preview role delete", RESET_NOT_APPLIED) {
        return err.into_response();
    }
    // An ok answer without a preview lists nothing. Safe either way: the preview only informs,
    // the worker checks the exposures again on the delete itself.
    let preview = preview.unwrap_or_else(|| chelabase_nats::DeletePreview {
        successor: successor.clone(),
        ..Default::default()
    });
    (StatusCode::OK, Json(RoleDeletePreview::from(preview))).into_response()
}

/// A done role delete.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct DeletedRole {
    /// The role that received the objects.
    successor: String,
    reassigned: ReassignedObjects,
}

/// What a role delete handed to its successor, over every database.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ReassignedObjects {
    /// The objects the role owned (tables, functions, types, schemas, …).
    count: u64,
    /// The `SECURITY DEFINER` routines among them, as `schema.name(argument types)`: review
    /// them, they now run as the successor. PUBLIC's EXECUTE on every routine that moved was
    /// revoked.
    definers: Vec<String>,
}

fn invalid_successor() -> ApiError {
    ApiError::new(
        422,
        ErrorCode::InvalidSuccessor,
        "the successor can't receive the role's objects: name a role the owner can SET to that isn't owner-level, reserved or a personal login, or chelabase_orphaned",
    )
}

/// T10's refusals the API can make itself (defence in depth; the worker checks the rest in
/// SQL): a name Postgres can hold, not the role itself, not the owner, not reserved (except
/// `chelabase_orphaned`), and not recorded as a personal login, the owner or owner-level.
async fn check_successor<M, J>(
    state: &AppState<M, J>,
    endpoint: &str,
    owner: &str,
    role: &str,
    successor: &str,
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    if successor == QUARANTINE_SUCCESSOR {
        return Ok(());
    }
    if check_any_role_name(successor).is_err()
        || successor == role
        || successor == owner
        || is_membership_reserved(successor)
    {
        return Err(invalid_successor());
    }
    let rows = state.metadata.list_roles(endpoint).await.map_err(|err| {
        error!("list roles for a delete's successor: {err}");
        ApiError::internal()
    })?;
    let refused = rows.iter().any(|r| {
        r.name == successor
            && (matches!(r.origin, RoleOrigin::Personal | RoleOrigin::Owner) || r.level == RoleLevel::OwnerLevel)
    });
    if refused { Err(invalid_successor()) } else { Ok(()) }
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
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the role doesn't exist, or the role was made in SQL by a role other than the owner (the owner doesn't administer it)", body = ErrorEnvelope),
        (status = 409, description = "`role_cap_reached`: turning LOGIN on would give the branch more login roles than its maximum; `branch_not_ready`: the branch isn't ready; `backup_branch`: a restore backup has no compute; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 422, description = "`reserved_role_name`: the owner, a reserved role or any `chelabase_` role; `invalid_role_name`: the name is empty, over 63 bytes or has a NUL byte; `role_membership_not_allowed`: a membership in a reserved role, the owner, the role itself, a role that doesn't exist or isn't the owner's to grant, or a role that is itself a member (at any depth) of a reserved role; `role_attribute_not_allowed`: a connection limit under -1; `invalid_request`: an unknown field (an attribute that can't be set); `exposes_data_api`: the new memberships would let a data API key's role, `anon`, `authenticated` or `authenticator` reach a role with the members hub's \"all rows\" policy on the branch (every row through the data API); `chain` lists the roles from that role to it, and nothing changed", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed to change the role", body = ErrorEnvelope),
        (status = 503, description = "`not_applied`: the change is stored but not applied yet and will be retried; `unavailable`: a `member_of` change's data API exposure check (\"all rows\" roles, read on the branch's compute) couldn't be completed, nothing changed, retry; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
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
    let (record, branch, endpoint) = match load_target(&state, &principal, ep::UPDATE_ROLE.policy.access, id, branch_id).await {
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
    if let Err(err) = not_personal(&state, &record, &role).await {
        return err.into_response();
    }
    if let Err(err) = check_settings(&role, owner, request.connection_limit, request.member_of.as_deref()) {
        return err.into_response();
    }
    if let Some(member_of) = &request.member_of
        && let Err(err) = refuse_all_rows_exposure(&state, &branch, &role, member_of).await
    {
        return err.into_response();
    }
    // Only what the request changes: fields it leaves out are dropped from `attributes` below.
    let mut changed = json!({
        "role": role,
        "login": request.login,
        "attributes": {
            "owner_level": request.owner_level,
            "inherit": request.inherit,
            "connection_limit": request.connection_limit,
            "valid_until": request.valid_until.map(|v| v.map_or(json!("cleared"), |d| json!(d))),
            "bypass_rls": request.bypass_rls,
            "member_of": request.member_of,
        },
    });
    if let Some(attributes) = changed["attributes"].as_object_mut() {
        attributes.retain(|_, v| !v.is_null());
    }
    // T9-m2: no `login` the request doesn't change.
    if let Some(fields) = changed.as_object_mut() {
        fields.retain(|key, v| key != "login" || !v.is_null());
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
    if let Some(applied) = event_applied(&reply) {
        log_role_event(&state, &principal, &record, branch.id, AccessAction::RoleUpdated, changed, applied).await;
    }
    if let Err(err) = rpc_result(reply, "update role", ROLE_UPDATE_NOT_APPLIED) {
        return err.into_response();
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, %role, "role changed");
    let keys = state.metadata.list_data_keys(branch.id).await.unwrap_or_default();
    // As the keys: the change is done, so a failed read shows no exemption.
    let exemptions = exemptions_of(&state, branch.id).await.unwrap_or_default();
    match state.metadata.list_roles(&endpoint).await {
        Ok(rows) => match rows.into_iter().find(|r| r.name == role) {
            Some(row) => (StatusCode::ACCEPTED, Json(BranchRole::new(row, &keys, &exemptions))).into_response(),
            // A role made in SQL without a password has no row: the change is done all the same.
            None => StatusCode::ACCEPTED.into_response(),
        },
        Err(err) => {
            error!("list roles after update: {err}");
            ApiError::internal().into_response()
        }
    }
}

/// EOS-H1: a membership change of `role` (its other memberships replaced by `member_of`, as the
/// worker applies it: `pg_*` and Chelabase's own groups stay) that would let a live data API
/// key's role, `anon`, `authenticated` or `authenticator` newly reach a role with the hub's
/// "all rows" policy on the branch is 422 `exposes_data_api` with the chain; the worker checks
/// it (as `cloud_admin`, every live database) before the update is sent, and a check that
/// can't complete refuses with 503 (user ruling 2026-10-23: fail closed). Exposure that was
/// already there isn't this change's.
async fn refuse_all_rows_exposure<M, J>(
    state: &AppState<M, J>,
    branch: &BranchRecord,
    role: &str,
    member_of: &[String],
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let keys = state.metadata.list_data_keys(branch.id).await.map_err(|err| {
        error!(branch_id = %branch.id, "all-rows check: list data keys: {err}");
        ApiError::internal()
    })?;
    let starting: BTreeSet<String> = ["anon", "authenticated", "authenticator"]
        .into_iter()
        .map(String::from)
        .chain(keys.into_iter().filter(|k| k.revoked_at.is_none()).map(|k| k.role))
        .collect();
    let change = chelabase_nats::MembershipChange { role: role.to_string(), member_of: member_of.to_vec() };
    check_all_rows(state, branch.id, starting, Some(change), &format!("changing {role:?}'s memberships")).await
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
    let (record, branch, _) = match load_target(&state, &principal, ep::LIST_ROLE_SESSIONS.policy.access, id, branch_id).await {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = check_any_name(&role) {
        return err.into_response();
    }
    if let Err(err) = not_personal(&state, &record, &role).await {
        return err.into_response();
    }
    let reply = state.js.role_sessions(RoleRequest { branch_id: branch.id, name: role }).await;
    match sessions_result(reply) {
        Ok(sessions) => Json(sessions).into_response(),
        Err(err) => err.into_response(),
    }
}
