//! Projects: list, create, rename and delete. A project groups a tenant's databases; deleting
//! one starts the deletion of its databases and hands the rest to the worker.

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use chelabase_metadata::{
    AppUsersStatus, BeginProjectDelete, ProjectRecord, ProjectStatus, RenameProject, valid_project_name,
};
use chelabase_nats::{DeleteDatabaseJob, JobEnvelope, ProjectDeleteJob};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::{ApiJson, ApiPath, ProjectId};
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{authorize, authorize_project, Target};
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort};
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::Access;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_projects, create_project))
        .routes(routes!(get_project, update_project, delete_project))
}

/// Where a project is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatusView {
    Active,
    /// Its databases are being removed; nothing can be added to it.
    Deleting,
}

impl From<ProjectStatus> for ProjectStatusView {
    fn from(status: ProjectStatus) -> Self {
        match status {
            ProjectStatus::Active => Self::Active,
            // A deleted project is never shown; the closest honest answer.
            ProjectStatus::Deleting | ProjectStatus::Deleted => Self::Deleting,
        }
    }
}

/// Where a project's app users are, when they were ever enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppUsersStatusView {
    Enabling,
    Enabled,
    Disabling,
    Failed,
}

impl From<AppUsersStatus> for AppUsersStatusView {
    fn from(status: AppUsersStatus) -> Self {
        match status {
            AppUsersStatus::Enabling => Self::Enabling,
            AppUsersStatus::Enabled => Self::Enabled,
            AppUsersStatus::Disabling => Self::Disabling,
            AppUsersStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub name: String,
    /// Chosen from the name when the project is created; renaming doesn't change it.
    pub slug: String,
    pub status: ProjectStatusView,
    #[schema(required = true)]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `null` while app users were never enabled for the project.
    #[schema(required = true)]
    pub app_users: Option<AppUsersStatusView>,
}

impl ProjectInfo {
    pub(crate) fn new(record: ProjectRecord, app_users: Option<AppUsersStatusView>) -> Self {
        Self {
            id: record.id,
            name: record.name,
            slug: record.slug,
            status: record.status.into(),
            error: record.error,
            created_at: record.created_at,
            updated_at: record.updated_at,
            app_users,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateProjectRequest {
    /// 1 to 64 characters; the slug is made from it.
    pub name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateProjectRequest {
    /// 1 to 64 characters. The slug stays.
    pub name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DeleteProjectRequest {
    /// The project's slug, typed to confirm.
    pub confirm: String,
}

/// The caller's own, live project once `access` allows the caller on it, or the response to
/// send instead.
pub(crate) async fn load_project<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    id: Uuid,
) -> Result<ProjectRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    authorize_project(state, principal, access, id).await
}

/// The name trimmed, or the 422 to send.
fn checked_name(name: &str) -> Result<String, ApiError> {
    valid_project_name(name)
        .ok_or_else(|| ApiError::invalid_request(422, "name must be 1 to 64 characters"))
}

/// The project with its app-users status (`None` until app users were enabled).
async fn view<M, J>(state: &AppState<M, J>, record: ProjectRecord) -> Result<ProjectInfo, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let app_users = match state.metadata.project_auth(record.id).await {
        Ok(auth) => auth.map(|a| a.status.into()),
        Err(err) => {
            error!("project auth: {err}");
            return Err(ApiError::internal());
        }
    };
    Ok(ProjectInfo::new(record, app_users))
}

#[utoipa::path(
    get,
    path = "/v1/projects",
    operation_id = "listProjects",
    tag = "projects",
    summary = "List the caller's projects",
    responses(
        (status = 200, description = "The projects the caller has a grant on, oldest first (a tenant admin sees every project of the tenant)", body = Vec<ProjectInfo>),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_projects<M, J>(State(state): State<Arc<AppState<M, J>>>, principal: AuthPrincipal) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    if let Err(err) = authorize(&state, &principal, ep::LIST_PROJECTS.policy.access, Target::None).await {
        return err.into_response();
    }
    let mut records = match state.metadata.list_projects(principal.tenant_id).await {
        Ok(records) => records,
        Err(err) => {
            error!("list projects: {err}");
            return ApiError::internal().into_response();
        }
    };
    // Only the projects the caller has a grant on; tenant admins see them all.
    if !principal.member.as_ref().is_some_and(|m| m.tenant_admin) {
        let Some(member) = principal.member.as_ref() else {
            return ApiError::internal().into_response();
        };
        // T2-4: every access source, not the own grants alone.
        let granted = match state
            .metadata
            .member_grant_sources(principal.tenant_id, member.id)
            .await
        {
            Ok(granted) => granted,
            Err(err) => {
                error!("list member grant sources: {err}");
                return ApiError::internal().into_response();
            }
        };
        records.retain(|r| granted.iter().any(|(project, _)| *project == r.id.as_uuid()));
    }
    // One query for every project's app-users status.
    let statuses = match state.metadata.project_auth_statuses(principal.tenant_id).await {
        Ok(statuses) => statuses,
        Err(err) => {
            error!("project auth statuses: {err}");
            return ApiError::internal().into_response();
        }
    };
    let out: Vec<ProjectInfo> = records
        .into_iter()
        .map(|record| {
            let app_users = statuses.get(&record.id).map(|s| (*s).into());
            ProjectInfo::new(record, app_users)
        })
        .collect();
    Json(out).into_response()
}

#[utoipa::path(
    post,
    path = "/v1/projects",
    operation_id = "createProject",
    tag = "projects",
    summary = "Create a project",
    description = "The slug is made from the name (`My App` becomes `my-app`, or `my-app-2` when taken).",
    request_body = CreateProjectRequest,
    responses(
        (status = 201, description = "The new project", body = ProjectInfo),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the name is missing or isn't 1 to 64 characters", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn create_project<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiJson(body): ApiJson<CreateProjectRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    if let Err(err) = authorize(&state, &principal, ep::CREATE_PROJECT.policy.access, Target::None).await {
        return err.into_response();
    }
    let name = match checked_name(&body.name) {
        Ok(name) => name,
        Err(err) => return err.into_response(),
    };
    match state.metadata.create_project(principal.tenant_id, &name).await {
        Ok(record) => {
            info!(request_id = %principal.request_id, project_id = %record.id, "project created");
            (StatusCode::CREATED, Json(ProjectInfo::new(record, None))).into_response()
        }
        Err(err) => {
            error!("create project: {err}");
            ApiError::internal().into_response()
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/projects/{id}",
    operation_id = "getProject",
    tag = "projects",
    summary = "Get a project",
    params(("id" = Uuid, Path, description = "Project id")),
    responses(
        (status = 200, description = "The project", body = ProjectInfo),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_project<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_project(&state, &principal, ep::GET_PROJECT.policy.access, id).await {
        Ok(record) => record,
        Err(err) => return err.into_response(),
    };
    match view(&state, record).await {
        Ok(info) => Json(info).into_response(),
        Err(err) => err.into_response(),
    }
}

#[utoipa::path(
    patch,
    path = "/v1/projects/{id}",
    operation_id = "updateProject",
    tag = "projects",
    summary = "Rename a project",
    description = "Changes the name; the slug stays.",
    params(("id" = Uuid, Path, description = "Project id")),
    request_body = UpdateProjectRequest,
    responses(
        (status = 200, description = "The renamed project", body = ProjectInfo),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`: the project is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the name is missing or isn't 1 to 64 characters", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_project<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateProjectRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    if let Err(err) = load_project(&state, &principal, ep::UPDATE_PROJECT.policy.access, id).await {
        return err.into_response();
    }
    let name = match checked_name(&body.name) {
        Ok(name) => name,
        Err(err) => return err.into_response(),
    };
    match state.metadata.rename_project(principal.tenant_id, ProjectId(id), &name).await {
        Ok(RenameProject::Renamed(record)) => match view(&state, record).await {
            Ok(info) => Json(info).into_response(),
            Err(err) => err.into_response(),
        },
        Ok(RenameProject::NotFound) => ApiError::not_found("project not found").into_response(),
        Ok(RenameProject::Deleting) => {
            ApiError::new(409, ErrorCode::ProjectDeleting, "project is being deleted").into_response()
        }
        Err(err) => {
            error!("rename project: {err}");
            ApiError::internal().into_response()
        }
    }
}

#[utoipa::path(
    delete,
    path = "/v1/projects/{id}",
    operation_id = "deleteProject",
    tag = "projects",
    summary = "Delete a project",
    description = "Starts deleting the project and all its databases; the body must carry the project's slug. \
Returns 202 at once; the project shows as `deleting` until the background work is done.",
    params(("id" = Uuid, Path, description = "Project id")),
    request_body = DeleteProjectRequest,
    responses(
        (status = 202, description = "Accepted; the project is `deleting`", body = ProjectInfo),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`: already being deleted; `database_not_ready`: a database is still being created; `branch_protected`: a branch is protected; `conflict`: app users are being enabled or disabled", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: `confirm` is missing or isn't the project's slug", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_project<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<DeleteProjectRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_project(&state, &principal, ep::DELETE_PROJECT.policy.access, id).await {
        Ok(record) => record,
        Err(err) => return err.into_response(),
    };
    if body.confirm != record.slug {
        return ApiError::invalid_request(422, "type the project's slug to confirm").into_response();
    }
    let started = match state.metadata.begin_project_delete(principal.tenant_id, record.id).await {
        Ok(BeginProjectDelete::Started(ids)) => ids,
        Ok(BeginProjectDelete::NotFound) => return ApiError::not_found("project not found").into_response(),
        Ok(BeginProjectDelete::AlreadyDeleting) => {
            return ApiError::new(409, ErrorCode::ProjectDeleting, "project is already being deleted")
                .into_response();
        }
        Ok(BeginProjectDelete::NotReady(_)) => {
            return ApiError::new(
                409,
                ErrorCode::DatabaseNotReady,
                "a database of this project is still being created; try again when it is ready",
            )
            .into_response();
        }
        Ok(BeginProjectDelete::Protected(_)) => {
            return ApiError::new(
                409,
                ErrorCode::BranchProtected,
                "a branch of this project is protected; unprotect it first",
            )
            .into_response();
        }
        Ok(BeginProjectDelete::AppUsersBusy) => {
            return ApiError::new(
                409,
                ErrorCode::Conflict,
                "app users are being enabled or disabled; try again in a moment",
            )
            .into_response();
        }
        Err(err) => {
            error!("begin project delete: {err}");
            return ApiError::internal().into_response();
        }
    };
    // P10: every member's access to the project goes (a whole-tenant access event, S28), and
    // so do its databases.
    crate::internal::realtime::access_changed(&state.rt_events, principal.tenant_id, None);
    crate::internal::realtime::spawn_project(
        state.rt_events.clone(),
        record.id.as_uuid(),
        started
            .iter()
            .map(|d| chelabase_nats::rt::ProjectEvent::DatabaseDeleted { database_id: d.as_uuid() })
            .collect(),
    );
    // A publish failure is only logged: the worker's reconcile retries the database deletes
    // and republishes the project job.
    for database_id in started {
        let job = JobEnvelope { job_id: Uuid::new_v4(), payload: DeleteDatabaseJob { database_id } };
        if let Err(err) = state.js.publish_delete(&job).await {
            error!(%database_id, "publish delete failed (reconcile will retry): {err}");
        }
    }
    let job = JobEnvelope { job_id: Uuid::new_v4(), payload: ProjectDeleteJob { project_id: record.id } };
    if let Err(err) = state.js.publish_project_delete(&job).await {
        error!(project_id = %record.id, "publish project delete failed (reconcile will retry): {err}");
    }
    info!(request_id = %principal.request_id, project_id = %record.id, "project delete requested");
    let record = match state.metadata.get_project(principal.tenant_id, record.id).await {
        Ok(Some(fresh)) => fresh,
        _ => ProjectRecord { status: ProjectStatus::Deleting, ..record },
    };
    match view(&state, record).await {
        Ok(info) => (StatusCode::ACCEPTED, Json(info)).into_response(),
        Err(err) => err.into_response(),
    }
}
