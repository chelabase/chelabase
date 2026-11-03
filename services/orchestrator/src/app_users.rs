//! A project's app users: the settings (where the users table lives, email verification, what
//! deleting a user does to its row), enabling, and disabling. The worker does the work behind
//! the 202s; these routes validate, claim the row and publish the job.

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_identity::{IdentityError, Missing, Pool};
use chelabase_metadata::{
    AppUsersStatus, BeginEnable, BranchKind, BranchStatus, DatabaseOrigin, DatabaseStatus, MetadataError,
    NewProjectAuth, OnDelete, ProjectAuth, ProjectRecord,
};
use chelabase_nats::{AppUsersCheckRequest, AppUsersJob, JobEnvelope};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::{ApiJson, ApiPath, DatabaseId};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::branches::{conflict, unprocessable, valid_schema_name};
use crate::projects::{AppUsersStatusView, load_project};
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort, load_main};
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::Access;

const DEFAULT_SCHEMA: &str = "public";
const DEFAULT_TABLE: &str = "users";
/// Longest Postgres identifier.
const MAX_PG_DATABASE: usize = 63;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new().routes(routes!(get_app_users, put_app_users, disable_app_users))
}

/// A project's app-users settings.
#[derive(Debug, Serialize, ToSchema)]
pub struct AppUsersSettings {
    pub status: AppUsersStatusView,
    /// Why enabling failed, or, on an enabled project, the last problem reconcile found (a pool
    /// gone from the identity service, a branch it couldn't upgrade); `null` otherwise.
    #[schema(required = true)]
    pub error: Option<String>,
    /// The Chelabase database whose Postgres database holds the users table.
    pub database_id: DatabaseId,
    pub pg_database: String,
    pub schema: String,
    pub table: String,
    pub email_verification: bool,
    pub on_delete: OnDelete,
    /// Unused: always `null`. App users live in the identity pool `app-<project>`.
    #[schema(required = true)]
    pub zitadel_org_id: Option<String>,
}

impl From<ProjectAuth> for AppUsersSettings {
    fn from(a: ProjectAuth) -> Self {
        Self {
            status: a.status.into(),
            error: a.error,
            database_id: a.database_id,
            pg_database: a.pg_database,
            schema: a.schema_name,
            table: a.table_name,
            email_verification: a.email_verification,
            on_delete: a.on_delete,
            zitadel_org_id: a.zitadel_org_id,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutAppUsersRequest {
    /// The Chelabase database (of this project) that holds the users table.
    pub database_id: DatabaseId,
    /// Defaults to the database's main Postgres database. Fixed once app users are enabled.
    #[serde(default)]
    pub pg_database: Option<String>,
    /// `^[a-z_][a-z0-9_]{0,62}$`, default `public`; not a reserved schema. Fixed once enabled.
    #[serde(default)]
    pub schema: Option<String>,
    /// `^[a-z_][a-z0-9_]{0,62}$`, default `users`. Fixed once enabled.
    #[serde(default)]
    pub table: Option<String>,
    /// Default false.
    #[serde(default)]
    pub email_verification: Option<bool>,
    /// What deleting a user does to its row: `delete` (default) or `mark`.
    #[serde(default)]
    pub on_delete: Option<String>,
}

/// The caller's own project, refusing a deleting one.
pub(crate) async fn load_live_project<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    id: Uuid,
) -> Result<ProjectRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let project = load_project(state, principal, access, id).await?;
    if project.status != chelabase_metadata::ProjectStatus::Active {
        return Err(project_deleting());
    }
    Ok(project)
}

pub(crate) fn project_deleting() -> ApiError {
    ApiError::new(409, ErrorCode::ProjectDeleting, "project is being deleted")
}

pub(crate) fn app_users_disabled(status: u16) -> ApiError {
    ApiError::new(status, ErrorCode::AppUsersDisabled, "app users aren't enabled for this project")
}

pub(crate) async fn read_auth<M, J>(
    state: &AppState<M, J>,
    project: &ProjectRecord,
) -> Result<Option<ProjectAuth>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    state.metadata.project_auth(project.id).await.map_err(|err| {
        error!("project auth: {err}");
        ApiError::internal()
    })
}

/// [`read_auth`] for a route called with `access`: the row's Chelabase database is the one the
/// route acts on, so its override applies too (R20; an override only narrows, and a refusal
/// fails closed).
pub(crate) async fn read_auth_checked<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    project: &ProjectRecord,
) -> Result<Option<ProjectAuth>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let auth = read_auth(state, project).await?;
    if let Some(auth) = &auth {
        crate::access::authorize(
            state,
            principal,
            access,
            crate::access::Target::Database(auth.database_id.as_uuid()),
        )
        .await?;
    }
    Ok(auth)
}

/// Reserved schemas, `information_schema` and the `pg_` prefix (I8).
fn schema_is_reserved(schema: &str) -> bool {
    chelabase_query::schema::is_reserved(schema) || schema == "information_schema" || schema.starts_with("pg_")
}

#[utoipa::path(
    get,
    path = "/v1/projects/{id}/app-users",
    operation_id = "getAppUsers",
    tag = "app-users",
    summary = "Get a project's app-users settings",
    params(("id" = Uuid, Path, description = "Project id")),
    responses(
        (status = 200, description = "The settings and where enabling is", body = AppUsersSettings),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same); `app_users_disabled`: app users were never enabled", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`: the project is being deleted", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_app_users<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let project = match load_live_project(&state, &principal, ep::GET_APP_USERS.policy.access, id).await {
        Ok(project) => project,
        Err(err) => return err.into_response(),
    };
    match read_auth_checked(&state, &principal, ep::GET_APP_USERS.policy.access, &project).await {
        Ok(Some(auth)) => Json(AppUsersSettings::from(auth)).into_response(),
        Ok(None) => app_users_disabled(404).into_response(),
        Err(err) => err.into_response(),
    }
}

/// The request's values, checked and with the defaults filled in.
struct Wanted {
    database_id: DatabaseId,
    pg_database: Option<String>,
    schema: Option<String>,
    table: Option<String>,
    email_verification: Option<bool>,
    on_delete: Option<OnDelete>,
}

/// Step 1: identifiers (P22), reserved schemas (I8) and `on_delete`.
fn check_request(body: PutAppUsersRequest) -> Result<Wanted, ApiError> {
    if let Some(schema) = &body.schema {
        if !valid_schema_name(schema) {
            return Err(unprocessable("schema must match ^[a-z_][a-z0-9_]{0,62}$"));
        }
        if schema_is_reserved(schema) {
            return Err(unprocessable(
                "schema is reserved (chelabase, information_schema, pg_* and Postgres's and Neon's own)",
            ));
        }
    }
    if let Some(table) = &body.table
        && !valid_schema_name(table)
    {
        return Err(unprocessable("table must match ^[a-z_][a-z0-9_]{0,62}$"));
    }
    if let Some(name) = &body.pg_database
        && (name.is_empty() || name.len() > MAX_PG_DATABASE || name.contains('\0'))
    {
        return Err(unprocessable("pg_database must be 1 to 63 bytes"));
    }
    let on_delete = match body.on_delete.as_deref() {
        None => None,
        Some("delete") => Some(OnDelete::Delete),
        Some("mark") => Some(OnDelete::Mark),
        Some(_) => return Err(unprocessable("on_delete must be \"delete\" or \"mark\"")),
    };
    Ok(Wanted {
        database_id: body.database_id,
        pg_database: body.pg_database,
        schema: body.schema,
        table: body.table,
        email_verification: body.email_verification,
        on_delete,
    })
}

fn location_fixed() -> ApiError {
    conflict(
        ErrorCode::AppUsersLocationFixed,
        "the users table's location can't change once app users are enabled; disable app users first",
    )
}

/// Whether a request that omits parts of the location names `row`'s.
fn same_location(row: &ProjectAuth, wanted: &Wanted) -> bool {
    row.database_id == wanted.database_id
        && wanted.pg_database.as_ref().is_none_or(|v| *v == row.pg_database)
        && wanted.schema.as_ref().is_none_or(|v| *v == row.schema_name)
        && wanted.table.as_ref().is_none_or(|v| *v == row.table_name)
}

/// Steps 2 to 4 for a new location: the database, its data API on every live branch, and the
/// table. Returns the Postgres database's name.
async fn check_new_location<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    project: &ProjectRecord,
    wanted: &Wanted,
    schema: &str,
    table: &str,
) -> Result<String, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let internal = |what: &str, err: MetadataError| {
        error!("{what}: {err}");
        ApiError::internal()
    };
    // 2. A ready database of this project.
    let database = match state.metadata.get_database(wanted.database_id).await {
        Ok(Some(db))
            if db.tenant_id == principal.tenant_id
                && db.project_id == project.id
                && db.status != DatabaseStatus::Deleted =>
        {
            db
        }
        Ok(_) => return Err(ApiError::not_found("database not found")),
        Err(err) => return Err(internal("load database", err)),
    };
    if database.status != DatabaseStatus::Ready {
        return Err(conflict(ErrorCode::DatabaseNotReady, "the database is not ready"));
    }
    // 3. The Postgres database, served by the data API on every live normal branch.
    let main = load_main(state, database.database_id)
        .await?
        .ok_or_else(|| ApiError::not_found("database not found"))?;
    let main_rows = state
        .metadata
        .branch_databases(main.id)
        .await
        .map_err(|e| internal("list branch databases", e))?;
    let pg_database = match &wanted.pg_database {
        Some(name) => name.clone(),
        None => main_rows
            .iter()
            .find(|d| d.origin == DatabaseOrigin::Main)
            .map(|d| d.name.clone())
            .ok_or_else(|| conflict(ErrorCode::DatabaseNotReady, "the database's main Postgres database isn't known yet"))?,
    };
    match main_rows.iter().find(|d| d.name == pg_database) {
        None => return Err(ApiError::not_found("Postgres database not found")),
        Some(d) if d.origin == DatabaseOrigin::System => {
            return Err(unprocessable("the system database \"postgres\" is never served"));
        }
        Some(_) => {}
    }
    let branches = state
        .metadata
        .list_branches(database.database_id)
        .await
        .map_err(|e| internal("list branches", e))?;
    for branch in branches
        .iter()
        .filter(|b| b.kind == BranchKind::Normal && matches!(b.status, BranchStatus::Ready | BranchStatus::Resetting))
    {
        let rows = state
            .metadata
            .branch_databases(branch.id)
            .await
            .map_err(|e| internal("list branch databases", e))?;
        let on = rows
            .iter()
            .any(|d| d.name == pg_database && d.data_api == chelabase_metadata::DataApiState::On);
        if !on {
            return Err(conflict(
                ErrorCode::DataApiNotEnabled,
                format!(
                    "the data API must be on for {pg_database:?} on every branch of the database; it isn't on branch {:?}",
                    branch.name
                ),
            ));
        }
    }
    // 4. The worker looks at the table.
    let request = AppUsersCheckRequest {
        database_id: database.database_id,
        pg_database: pg_database.clone(),
        schema: schema.to_string(),
        table: table.to_string(),
    };
    match state.js.check_app_users_location(&request).await {
        Ok(reply) if reply.ok => Ok(pg_database),
        Ok(reply) if reply.code.as_deref() == Some("app_users_table_exists") => Err(conflict(
            ErrorCode::AppUsersTableExists,
            reply.error.unwrap_or_else(|| "a table with this name already exists".into()),
        )),
        Ok(reply) => {
            error!(project_id = %project.id, code = ?reply.code, "app users location check failed: {:?}", reply.error);
            Err(ApiError::new(503, ErrorCode::WorkerUnavailable, "the worker could not check the table"))
        }
        Err(err) => {
            error!(project_id = %project.id, "app users location check: {err}");
            Err(ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable"))
        }
    }
}

async fn publish_job<M, J>(state: &AppState<M, J>, job: AppUsersJob)
where
    M: MetadataPort,
    J: JobPort,
{
    // The row says what's wanted; a lost message is healed by the worker's reconcile.
    if let Err(err) = state.js.publish_app_users(&JobEnvelope { job_id: Uuid::new_v4(), payload: job.clone() }).await {
        warn!(?job, "publish app users job failed (reconcile will retry): {err}");
    }
}

#[utoipa::path(
    put,
    path = "/v1/projects/{id}/app-users",
    operation_id = "putAppUsers",
    tag = "app-users",
    summary = "Enable app users, or change their settings",
    description = "With no settings yet this enables app users: it checks the database, the data API on every branch and the table, then answers 202 and the worker creates the project's identity pool, the users table and its guard. Pressing it again while `enabling`, or after `failed` (same location), starts the work again (202). Once enabled it changes `email_verification` and `on_delete` only (200); another location is 409 `app_users_location_fixed`.",
    params(("id" = Uuid, Path, description = "Project id")),
    request_body = PutAppUsersRequest,
    responses(
        (status = 200, description = "The settings were changed", body = AppUsersSettings),
        (status = 202, description = "Enabling started (or started again)", body = AppUsersSettings),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project, database or Postgres database (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `database_not_ready`: the database isn't ready; `data_api_not_enabled`: the data API is off for the Postgres database on a branch; `app_users_table_exists`: the table exists already; `app_users_location_fixed`: the location can't change; `conflict`: app users are being disabled", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a field is missing or invalid, the schema is reserved (`chelabase`, `information_schema`, `pg_*`, Neon's), or `on_delete` isn't `delete` or `mark`", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker can't check the table", body = ErrorEnvelope),
    ),
)]
async fn put_app_users<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<PutAppUsersRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let project = match load_live_project(&state, &principal, ep::PUT_APP_USERS.policy.access, id).await {
        Ok(project) => project,
        Err(err) => return err.into_response(),
    };
    let wanted = match check_request(body) {
        Ok(wanted) => wanted,
        Err(err) => return err.into_response(),
    };
    // The body names a Chelabase database: its override applies too (an override only narrows).
    if let Err(err) = crate::access::authorize(
        &state,
        &principal,
        ep::PUT_APP_USERS.policy.access,
        crate::access::Target::Database(wanted.database_id.as_uuid()),
    )
    .await
    {
        return err.into_response();
    }
    let existing = match read_auth_checked(&state, &principal, ep::PUT_APP_USERS.policy.access, &project).await {
        Ok(existing) => existing,
        Err(err) => return err.into_response(),
    };
    // What is stored when the row is new, and what a row that exists keeps.
    let (pg_database, schema, table) = match &existing {
        Some(row) => {
            if !same_location(row, &wanted) {
                return location_fixed().into_response();
            }
            (row.pg_database.clone(), row.schema_name.clone(), row.table_name.clone())
        }
        None => {
            let schema = wanted.schema.clone().unwrap_or_else(|| DEFAULT_SCHEMA.into());
            let table = wanted.table.clone().unwrap_or_else(|| DEFAULT_TABLE.into());
            match check_new_location(&state, &principal, &project, &wanted, &schema, &table).await {
                Ok(pg_database) => (pg_database, schema, table),
                Err(err) => return err.into_response(),
            }
        }
    };
    let new = NewProjectAuth {
        project_id: project.id,
        database_id: wanted.database_id,
        pg_database: &pg_database,
        schema_name: &schema,
        table_name: &table,
        email_verification: wanted
            .email_verification
            .or(existing.as_ref().map(|r| r.email_verification))
            .unwrap_or(false),
        on_delete: wanted.on_delete.or(existing.as_ref().map(|r| r.on_delete)).unwrap_or(OnDelete::Delete),
    };
    let (row, accepted) = match state.metadata.begin_enable_app_users(&new).await {
        Ok(BeginEnable::Started | BeginEnable::Retry) => match read_auth(&state, &project).await {
            Ok(Some(row)) => (row, true),
            Ok(None) => return ApiError::internal().into_response(),
            Err(err) => return err.into_response(),
        },
        Ok(BeginEnable::Exists(row)) => {
            if !same_location(&row, &wanted) {
                return location_fixed().into_response();
            }
            if row.status == AppUsersStatus::Disabling {
                return conflict(ErrorCode::Conflict, "app users are being disabled; try again when that is done")
                    .into_response();
            }
            match state
                .metadata
                .update_app_users_settings(project.id, Some(new.email_verification), Some(new.on_delete))
                .await
            {
                Ok(Some(row)) => {
                    // Still enabling: the press starts the work again (I6).
                    let again = row.status == AppUsersStatus::Enabling;
                    (row, again)
                }
                Ok(None) => return ApiError::internal().into_response(),
                Err(err) => {
                    error!("update app users settings: {err}");
                    return ApiError::internal().into_response();
                }
            }
        }
        Err(MetadataError::ProjectDeleting(_)) => return project_deleting().into_response(),
        Err(MetadataError::ProjectNotFound(_)) => return ApiError::not_found("project not found").into_response(),
        Err(err) => {
            error!("begin enable app users: {err}");
            return ApiError::internal().into_response();
        }
    };
    if accepted {
        publish_job(&state, AppUsersJob::Enable { project_id: project.id }).await;
        info!(request_id = %principal.request_id, project_id = %project.id, "app users enabling");
    } else {
        info!(request_id = %principal.request_id, project_id = %project.id, "app users settings changed");
    }
    let status = if accepted { StatusCode::ACCEPTED } else { StatusCode::OK };
    (status, Json(AppUsersSettings::from(row))).into_response()
}

#[utoipa::path(
    delete,
    path = "/v1/projects/{id}/app-users",
    operation_id = "disableAppUsers",
    tag = "app-users",
    summary = "Disable app users",
    description = "Refused while the project's pool has any user. Otherwise 202: the worker removes the guard machinery (the table stays as an ordinary table), drops the pool and deletes the settings. Pressing it again while `disabling` starts the work again.",
    params(("id" = Uuid, Path, description = "Project id")),
    responses(
        (status = 202, description = "Disabling started", body = AppUsersSettings),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same); `app_users_disabled`: app users aren't enabled", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_has_users`: users exist; `conflict`: app users are being enabled", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached", body = ErrorEnvelope),
    ),
)]
async fn disable_app_users<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let project = match load_live_project(&state, &principal, ep::DISABLE_APP_USERS.policy.access, id).await {
        Ok(project) => project,
        Err(err) => return err.into_response(),
    };
    let auth = match read_auth_checked(&state, &principal, ep::DISABLE_APP_USERS.policy.access, &project).await {
        Ok(Some(auth)) => auth,
        Ok(None) => return app_users_disabled(404).into_response(),
        Err(err) => return err.into_response(),
    };
    // A retry of a disable in progress skips the check: the users were checked when it began and
    // the pool may be gone already. The drop is irreversible, so a pool that can't be read
    // refuses the disable.
    if auth.status != AppUsersStatus::Disabling {
        let Some(identity) = &state.identity_service else {
            return crate::app_user_admin::identity_unavailable().into_response();
        };
        match identity.list_users(&Pool::App(project.id.as_uuid()), 1, 0).await {
            Ok(page) if page.total == 0 => {}
            // A failed enable never got as far as the pool: nothing to lose.
            Err(IdentityError::NotFound(Missing::Pool)) => {}
            Ok(_) => {
                return conflict(
                    ErrorCode::AppUsersHasUsers,
                    "the project still has users; delete them before disabling app users",
                )
                .into_response();
            }
            Err(err) => return crate::app_user_admin::identity_failed("check the pool's users", &err).into_response(),
        }
    }
    if auth.status != AppUsersStatus::Disabling {
        match state.metadata.begin_disable_app_users(project.id).await {
            Ok(true) => {}
            Ok(false) => {
                return conflict(ErrorCode::Conflict, "app users are being enabled; try again when that is done")
                    .into_response();
            }
            Err(err) => {
                error!("begin disable app users: {err}");
                return ApiError::internal().into_response();
            }
        }
    }
    publish_job(&state, AppUsersJob::Disable { project_id: project.id }).await;
    info!(request_id = %principal.request_id, project_id = %project.id, "app users disabling");
    match read_auth(&state, &project).await {
        Ok(Some(row)) => (StatusCode::ACCEPTED, Json(AppUsersSettings::from(row))).into_response(),
        Ok(None) => ApiError::internal().into_response(),
        Err(err) => err.into_response(),
    }
}
