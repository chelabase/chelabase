//! Edge functions m7: a function's schedules on a branch (5-field cron in an IANA time zone).
//! The worker runs the function's `scheduled` handler at each due time. Access is m3's: reads
//! need Viewer, writes `BRANCH` (Developer, Admin on a protected branch), another database's
//! branch or function is 404, platform mode 403. Schedules belong to the branch, so a new
//! branch inherits its parent's (m2's copy) and a tombstone purges them. Nothing is published
//! after a write: the functions service caches no schedule.

use std::future::Future;
use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_metadata::{
    MetadataClient, MetadataError, NewSchedule, Schedule, ScheduleError, ScheduleRecord,
};
use chelabase_routes::orchestrator as ep;
use chelabase_types::ApiPath;
use chelabase_types::BranchId;
use chelabase_types::api_error::{ApiError, ErrorEnvelope};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::info;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::branches::parse_body;
use crate::functions::{begin, find, metadata_error, respond};
use crate::{ApiBytes, AppState, AuthPrincipal, JobPort, MetadataPort};

/// The metadata the schedules routes read and write.
pub trait SchedulesPort: Send + Sync {
    fn add_schedule(
        &self,
        schedule: &NewSchedule<'_>,
    ) -> impl Future<Output = Result<Uuid, MetadataError>> + Send;
    fn list_schedules(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<ScheduleRecord>, MetadataError>> + Send;
    fn update_schedule(
        &self,
        branch: BranchId,
        function: Uuid,
        id: Uuid,
        cron: Option<&str>,
        timezone: Option<&str>,
        enabled: Option<bool>,
    ) -> impl Future<Output = Result<Option<ScheduleRecord>, MetadataError>> + Send;
    fn delete_schedule(
        &self,
        branch: BranchId,
        function: Uuid,
        id: Uuid,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
}

impl SchedulesPort for MetadataClient {
    async fn add_schedule(&self, schedule: &NewSchedule<'_>) -> Result<Uuid, MetadataError> {
        MetadataClient::add_schedule(self, schedule).await
    }
    async fn list_schedules(&self, branch: BranchId) -> Result<Vec<ScheduleRecord>, MetadataError> {
        MetadataClient::list_schedules(self, branch).await
    }
    async fn update_schedule(
        &self,
        branch: BranchId,
        function: Uuid,
        id: Uuid,
        cron: Option<&str>,
        timezone: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<Option<ScheduleRecord>, MetadataError> {
        MetadataClient::update_schedule(self, branch, function, id, cron, timezone, enabled).await
    }
    async fn delete_schedule(
        &self,
        branch: BranchId,
        function: Uuid,
        id: Uuid,
    ) -> Result<bool, MetadataError> {
        MetadataClient::delete_schedule(self, branch, function, id).await
    }
}

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_schedules, create_schedule))
        .routes(routes!(update_schedule, delete_schedule))
}

/// A function's schedule on a branch.
#[derive(Debug, Serialize, ToSchema)]
#[schema(as = FunctionSchedule)]
struct ScheduleView {
    id: Uuid,
    /// 5 fields: minute, hour, day of month, month, day of week.
    cron: String,
    /// IANA time zone the cron is read in.
    timezone: String,
    /// A disabled schedule never runs.
    enabled: bool,
    /// When it runs next.
    #[schema(required = true)]
    next_run_at: Option<DateTime<Utc>>,
}

impl From<ScheduleRecord> for ScheduleView {
    fn from(s: ScheduleRecord) -> Self {
        ScheduleView {
            id: s.id,
            cron: s.cron,
            timezone: s.timezone,
            enabled: s.enabled,
            next_run_at: s.next_run_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = CreateFunctionScheduleRequest)]
struct CreateRequest {
    /// 5 fields (minute granularity), e.g. `*/15 * * * *`.
    cron: String,
    /// IANA time zone, e.g. `Europe/London`; defaults to `UTC`.
    timezone: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = UpdateFunctionScheduleRequest)]
struct UpdateRequest {
    cron: Option<String>,
    timezone: Option<String>,
    enabled: Option<bool>,
}

/// 422 naming the field.
fn invalid(err: ScheduleError) -> ApiError {
    ApiError::invalid_request(422, err.to_string())
        .with_extra("field", serde_json::Value::String(err.field().into()))
}

fn schedule_not_found() -> ApiError {
    ApiError::not_found("schedule not found")
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/schedules",
    operation_id = "listFunctionSchedules",
    tag = "functions",
    summary = "List a function's schedules on the branch",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
    ),
    responses(
        (status = 200, description = "The schedules, oldest first", body = Vec<ScheduleView>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_schedules<M, J>(
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
                ep::LIST_FUNCTION_SCHEDULES.policy.access,
                id,
                branch_id,
            )
            .await?;
            let function = find(&state, &record, &name).await?;
            let list: Vec<ScheduleView> = state
                .metadata
                .list_schedules(branch.id)
                .await
                .map_err(|e| metadata_error("list schedules", e))?
                .into_iter()
                .filter(|s| s.function_id == function.id)
                .map(ScheduleView::from)
                .collect();
            Ok(Json(list).into_response())
        }
        .await,
    )
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/schedules",
    operation_id = "createFunctionSchedule",
    tag = "functions",
    summary = "Add a schedule to a function on the branch",
    description = "At each due time the function's `scheduled(controller, env, ctx)` handler runs once, with `controller.cron` and `controller.scheduledTime`, while the function is enabled on the branch. A cron has 5 fields (minute granularity); ticks missed while the worker was down run once. A new branch inherits the schedules.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
    ),
    request_body = CreateRequest,
    responses(
        (status = 201, description = "The schedule, enabled", body = ScheduleView),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the branch (Admin when it's protected) is needed; `not_available_in_platform_mode`", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a bad cron or time zone (`field` names it), or an unknown field", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn create_schedule<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
    ApiBytes(body): ApiBytes,
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
                ep::CREATE_FUNCTION_SCHEDULE.policy.access,
                id,
                branch_id,
            )
            .await?;
            let request: CreateRequest = parse_body(&body)?;
            let timezone = request.timezone.as_deref().unwrap_or("UTC");
            let schedule = Schedule::parse(&request.cron, timezone).map_err(invalid)?;
            let function = find(&state, &record, &name).await?;
            let next_run_at = schedule.next_after(Utc::now());
            let schedule_id = state
                .metadata
                .add_schedule(&NewSchedule {
                    branch_id: branch.id,
                    function_id: function.id,
                    cron: &request.cron,
                    timezone,
                    next_run_at,
                })
                .await
                .map_err(|e| metadata_error("add schedule", e))?;
            info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, schedule = %schedule_id, "function schedule added");
            let view = ScheduleView {
                id: schedule_id,
                cron: request.cron,
                timezone: timezone.to_owned(),
                enabled: true,
                next_run_at,
            };
            Ok((StatusCode::CREATED, Json(view)).into_response())
        }
        .await,
    )
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/schedules/{schedule_id}",
    operation_id = "updateFunctionSchedule",
    tag = "functions",
    summary = "Change a function's schedule on the branch",
    description = "Changes what the body names. A new cron or time zone, or turning the schedule on, sets its next run from now.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
        ("schedule_id" = Uuid, Path, description = "Schedule id"),
    ),
    request_body = UpdateRequest,
    responses(
        (status = 200, description = "The schedule, changed", body = ScheduleView),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the branch (Admin when it's protected) is needed; `not_available_in_platform_mode`", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the function or its schedule doesn't exist", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a bad cron or time zone (`field` names it), or an unknown field", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_schedule<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name, schedule_id)): ApiPath<(Uuid, Uuid, String, Uuid)>,
    ApiBytes(body): ApiBytes,
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
                ep::UPDATE_FUNCTION_SCHEDULE.policy.access,
                id,
                branch_id,
            )
            .await?;
            let request: UpdateRequest = parse_body(&body)?;
            // Checked here so the 422 names its field; the metadata checks the merged pair again.
            if let Some(cron) = &request.cron {
                Schedule::parse(cron, "UTC").map_err(invalid)?;
            }
            if let Some(tz) = &request.timezone {
                Schedule::parse("* * * * *", tz).map_err(invalid)?;
            }
            let function = find(&state, &record, &name).await?;
            let changed = state
                .metadata
                .update_schedule(
                    branch.id,
                    function.id,
                    schedule_id,
                    request.cron.as_deref(),
                    request.timezone.as_deref(),
                    request.enabled,
                )
                .await
                .map_err(|e| metadata_error("update schedule", e))?
                .ok_or_else(schedule_not_found)?;
            info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, schedule = %schedule_id, "function schedule changed");
            Ok(Json(ScheduleView::from(changed)).into_response())
        }
        .await,
    )
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/schedules/{schedule_id}",
    operation_id = "deleteFunctionSchedule",
    tag = "functions",
    summary = "Delete a function's schedule on the branch",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
        ("schedule_id" = Uuid, Path, description = "Schedule id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the branch (Admin when it's protected) is needed; `not_available_in_platform_mode`", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the function or its schedule doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_schedule<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name, schedule_id)): ApiPath<(Uuid, Uuid, String, Uuid)>,
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
                ep::DELETE_FUNCTION_SCHEDULE.policy.access,
                id,
                branch_id,
            )
            .await?;
            let function = find(&state, &record, &name).await?;
            if !state
                .metadata
                .delete_schedule(branch.id, function.id, schedule_id)
                .await
                .map_err(|e| metadata_error("delete schedule", e))?
            {
                return Err(schedule_not_found());
            }
            info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, schedule = %schedule_id, "function schedule deleted");
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        .await,
    )
}
