//! Edge functions m13: a function's triggers on a branch. A `table` trigger (`pg_database`,
//! `schema`, `table`, `insert`/`update`/`delete`) starts `pending_install` and the worker installs
//! it; its committed row changes are POSTed to the function's `fetch` handler marked
//! `chelabase-event: db`, at least once. A `storage` trigger (`pg_database`, `bucket?`,
//! `prefix?`, `object.*`) starts `installed`; its delivery is m14's. Access is m3's: reads need
//! Viewer, writes `BRANCH` (Developer, Admin on a protected branch), another database's branch
//! or function is 404, platform mode 403. Nothing is published after a write: the functions
//! service caches no trigger (the worker reads them each pass).

use std::future::Future;
use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_metadata::{
    FunctionKind, MetadataClient, MetadataError, NewTrigger, TriggerRecord, TriggerSource,
};
use chelabase_routes::orchestrator as ep;
use chelabase_types::api_error::{ApiError, ErrorEnvelope};
use chelabase_types::{ApiPath, BranchId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::info;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::branches::{parse_body, unprocessable};
use crate::functions::{begin, find, metadata_error, respond};
use crate::{ApiBytes, AppState, AuthPrincipal, JobPort, MetadataPort};

/// The metadata the triggers routes read and write.
pub trait TriggersPort: Send + Sync {
    fn add_trigger(
        &self,
        trigger: &NewTrigger<'_>,
    ) -> impl Future<Output = Result<Uuid, MetadataError>> + Send;
    fn list_triggers(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<TriggerRecord>, MetadataError>> + Send;
    fn update_trigger(
        &self,
        branch: BranchId,
        function: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> impl Future<Output = Result<Option<TriggerRecord>, MetadataError>> + Send;
    fn delete_trigger(
        &self,
        branch: BranchId,
        function: Uuid,
        id: Uuid,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
}

impl TriggersPort for MetadataClient {
    async fn add_trigger(&self, trigger: &NewTrigger<'_>) -> Result<Uuid, MetadataError> {
        MetadataClient::add_trigger(self, trigger).await
    }
    async fn list_triggers(&self, branch: BranchId) -> Result<Vec<TriggerRecord>, MetadataError> {
        MetadataClient::list_triggers(self, branch).await
    }
    async fn update_trigger(
        &self,
        branch: BranchId,
        function: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Option<TriggerRecord>, MetadataError> {
        MetadataClient::update_trigger(self, branch, function, id, enabled).await
    }
    async fn delete_trigger(
        &self,
        branch: BranchId,
        function: Uuid,
        id: Uuid,
    ) -> Result<bool, MetadataError> {
        MetadataClient::delete_trigger(self, branch, function, id).await
    }
}

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_triggers, create_trigger))
        .routes(routes!(update_trigger, delete_trigger))
}

/// Where a trigger's events come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = FunctionTriggerSource)]
enum SourceView {
    /// Committed row changes of a table.
    Table,
    /// Object changes in a storage bucket (delivered from m14).
    Storage,
}

/// A function's trigger on a branch.
#[derive(Debug, Serialize, ToSchema)]
#[schema(as = FunctionTrigger)]
struct TriggerView {
    id: Uuid,
    source: SourceView,
    /// A disabled table trigger is uninstalled (its queued events are dropped); turning it on
    /// installs it again.
    enabled: bool,
    /// `pending_install` (the worker installs it), `installed` (events are delivered) or
    /// `failed` (`last_error` says why).
    state: String,
    pg_database: String,
    #[schema(required = true)]
    schema: Option<String>,
    #[schema(required = true)]
    table: Option<String>,
    #[schema(required = true)]
    bucket: Option<String>,
    #[schema(required = true)]
    prefix: Option<String>,
    events: Vec<String>,
    /// Why the install failed.
    #[schema(required = true)]
    last_error: Option<String>,
    /// Events that went dead after 10 failed deliveries.
    dead_count: i64,
    /// Events the outbox bound removed undelivered: dead events past 10,000 per database or 7
    /// days, and the queued events of a trigger turned off. A pending event is never removed.
    dropped_count: i64,
    /// Why the last failed delivery failed; cleared by a delivery.
    #[schema(required = true)]
    last_delivery_error: Option<String>,
    #[schema(required = true)]
    last_delivered_at: Option<DateTime<Utc>>,
}

impl From<TriggerRecord> for TriggerView {
    fn from(t: TriggerRecord) -> Self {
        TriggerView {
            id: t.id,
            source: match t.source {
                TriggerSource::Table => SourceView::Table,
                TriggerSource::Storage => SourceView::Storage,
            },
            enabled: t.enabled,
            state: t.state.as_str().to_owned(),
            pg_database: t.pg_database,
            schema: t.schema_name,
            table: t.table_name,
            bucket: t.bucket,
            prefix: t.prefix,
            events: t.events,
            last_error: t.last_error,
            dead_count: t.dead_count,
            dropped_count: t.dropped_count,
            last_delivery_error: t.last_delivery_error,
            last_delivered_at: t.last_delivered_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = CreateFunctionTriggerRequest)]
struct CreateRequest {
    source: SourceView,
    /// The branch's Postgres database the table (or, for storage, the bucket's project) is in.
    pg_database: String,
    /// `table` only.
    schema: Option<String>,
    /// `table` only.
    table: Option<String>,
    /// `storage` only; every bucket when absent.
    bucket: Option<String>,
    /// `storage` only; every key when absent.
    prefix: Option<String>,
    /// `table`: `insert`, `update`, `delete`. `storage`: `object.created`, `object.updated`,
    /// `object.deleted`, `object.moved`. Each once, never mixed.
    events: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = UpdateFunctionTriggerRequest)]
struct UpdateRequest {
    enabled: bool,
}

const TABLE_EVENTS: [&str; 3] = ["insert", "update", "delete"];
const STORAGE_EVENTS: [&str; 4] = [
    "object.created",
    "object.updated",
    "object.deleted",
    "object.moved",
];

/// The body's checks, before the metadata's own: events of the source only, each once, and
/// the columns that go with the source.
fn check(r: &CreateRequest) -> Result<(), ApiError> {
    let allowed: &[&str] = match r.source {
        SourceView::Table => &TABLE_EVENTS,
        SourceView::Storage => &STORAGE_EVENTS,
    };
    if r.events.is_empty() {
        return Err(unprocessable("events: at least one"));
    }
    if r.events.iter().any(|e| !allowed.contains(&e.as_str())) {
        return Err(unprocessable(&format!(
            "events: a {} trigger takes only {}",
            source_name(r.source),
            allowed.join(", ")
        )));
    }
    if r.events
        .iter()
        .enumerate()
        .any(|(i, e)| r.events[..i].contains(e))
    {
        return Err(unprocessable("events: each event once"));
    }
    if r.pg_database.is_empty() || r.pg_database.len() > 63 {
        return Err(unprocessable("pg_database: 1 to 63 bytes"));
    }
    let fits = match r.source {
        SourceView::Table => {
            r.schema.is_some() && r.table.is_some() && r.bucket.is_none() && r.prefix.is_none()
        }
        SourceView::Storage => r.schema.is_none() && r.table.is_none(),
    };
    if !fits {
        return Err(unprocessable(match r.source {
            SourceView::Table => "a table trigger takes schema and table, and no bucket or prefix",
            SourceView::Storage => "a storage trigger takes no schema or table",
        }));
    }
    // m13b (B2): never on Chelabase's or Postgres's own schemas (the worker refuses too).
    if let Some(schema) = r.schema.as_deref()
        && chelabase_schema::exclusions::function_trigger_schema_refused(schema)
    {
        return Err(unprocessable(&format!(
            "schema: {schema} is reserved; a table trigger goes on a table of your own schema"
        )));
    }
    Ok(())
}

fn source_name(s: SourceView) -> &'static str {
    match s {
        SourceView::Table => "table",
        SourceView::Storage => "storage",
    }
}

fn trigger_not_found() -> ApiError {
    ApiError::not_found("trigger not found")
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/triggers",
    operation_id = "listFunctionTriggers",
    tag = "functions",
    summary = "List a function's triggers on the branch",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
    ),
    responses(
        (status = 200, description = "The triggers, oldest first", body = Vec<TriggerView>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_triggers<M, J>(
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
                ep::LIST_FUNCTION_TRIGGERS.policy.access,
                id,
                branch_id,
            )
            .await?;
            let function = find(&state, &record, &name).await?;
            let list: Vec<TriggerView> = state
                .metadata
                .list_triggers(branch.id)
                .await
                .map_err(|e| metadata_error("list triggers", e))?
                .into_iter()
                .filter(|t| t.function_id == function.id)
                .map(TriggerView::from)
                .collect();
            Ok(Json(list).into_response())
        }
        .await,
    )
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/triggers",
    operation_id = "createFunctionTrigger",
    tag = "functions",
    summary = "Add a trigger to a function on the branch",
    description = "A `table` trigger is installed in the branch's database by the worker (`pending_install`, then `installed` or `failed`); each committed matching row change is then POSTed to the function's `fetch` handler with header `chelabase-event: db` and body `{id, type, schema, table, record, old_record, txid}` (over 256 KiB: `record` and `old_record` null, `truncated: true` and `primary_key`). Delivery is at least once (dedupe on `id`): a non-2xx answer is retried with backoff, and the 10th failure makes the event dead (`dead_count`). Events queue while the function is disabled; a pending event is never dropped, and dead events past 10,000 per database or 7 days are removed and counted (`dropped_count`). The database's compute stays awake while it has an installed table trigger. A `storage` trigger starts `installed`. A new branch inherits the triggers.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
    ),
    request_body = CreateRequest,
    responses(
        (status = 201, description = "The trigger, enabled", body = TriggerView),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the branch (Admin when it's protected) is needed; `not_available_in_platform_mode`", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the function doesn't exist", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: events of the other source or mixed, a repeat, no events, columns that don't go with the source, an unknown field, a table trigger on a reserved schema (`chelabase*`, `pg_*`, `storage`, `information_schema`) or on a service", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn create_trigger<M, J>(
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
            let (_, record, branch) =
                begin(&state, &principal, ep::CREATE_FUNCTION_TRIGGER.policy.access, id, branch_id).await?;
            let request: CreateRequest = parse_body(&body)?;
            check(&request)?;
            let function = find(&state, &record, &name).await?;
            // m13b (S4): a service never gets database events.
            if request.source == SourceView::Table && function.kind != FunctionKind::Function {
                return Err(unprocessable("a table trigger needs a function; a service never gets database events"));
            }
            let events: Vec<&str> = request.events.iter().map(String::as_str).collect();
            let trigger_id = state
                .metadata
                .add_trigger(&NewTrigger {
                    branch_id: branch.id,
                    function_id: function.id,
                    source: match request.source {
                        SourceView::Table => TriggerSource::Table,
                        SourceView::Storage => TriggerSource::Storage,
                    },
                    pg_database: &request.pg_database,
                    schema_name: request.schema.as_deref(),
                    table_name: request.table.as_deref(),
                    bucket: request.bucket.as_deref(),
                    prefix: request.prefix.as_deref(),
                    events: &events,
                })
                .await
                .map_err(|e| metadata_error("add trigger", e))?;
            info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, trigger = %trigger_id, "function trigger added");
            let made = state
                .metadata
                .list_triggers(branch.id)
                .await
                .map_err(|e| metadata_error("list triggers", e))?
                .into_iter()
                .find(|t| t.id == trigger_id)
                .ok_or_else(trigger_not_found)?;
            Ok((StatusCode::CREATED, Json(TriggerView::from(made))).into_response())
        }
        .await,
    )
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/triggers/{trigger_id}",
    operation_id = "updateFunctionTrigger",
    tag = "functions",
    summary = "Turn a function's trigger on the branch on or off",
    description = "Turning a table trigger off uninstalls it from the database and drops its queued events; turning it on installs it again (`pending_install`).",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
        ("trigger_id" = Uuid, Path, description = "Trigger id"),
    ),
    request_body = UpdateRequest,
    responses(
        (status = 200, description = "The trigger, changed", body = TriggerView),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the branch (Admin when it's protected) is needed; `not_available_in_platform_mode`", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the function or its trigger doesn't exist", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: `enabled` is missing, or an unknown field", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_trigger<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name, trigger_id)): ApiPath<(Uuid, Uuid, String, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(
        async {
            let (_, record, branch) =
                begin(&state, &principal, ep::UPDATE_FUNCTION_TRIGGER.policy.access, id, branch_id).await?;
            let request: UpdateRequest = parse_body(&body)?;
            let function = find(&state, &record, &name).await?;
            let changed = state
                .metadata
                .update_trigger(branch.id, function.id, trigger_id, request.enabled)
                .await
                .map_err(|e| metadata_error("update trigger", e))?
                .ok_or_else(trigger_not_found)?;
            info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, trigger = %trigger_id, enabled = request.enabled, "function trigger changed");
            Ok(Json(TriggerView::from(changed)).into_response())
        }
        .await,
    )
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}/functions/{name}/triggers/{trigger_id}",
    operation_id = "deleteFunctionTrigger",
    tag = "functions",
    summary = "Delete a function's trigger on the branch",
    description = "No event is delivered for it from now on; the worker drops its database trigger and queued events.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "Function name"),
        ("trigger_id" = Uuid, Path, description = "Trigger id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the branch (Admin when it's protected) is needed; `not_available_in_platform_mode`", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the function or its trigger doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_trigger<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name, trigger_id)): ApiPath<(Uuid, Uuid, String, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    respond(
        async {
            let (_, record, branch) =
                begin(&state, &principal, ep::DELETE_FUNCTION_TRIGGER.policy.access, id, branch_id).await?;
            let function = find(&state, &record, &name).await?;
            if !state
                .metadata
                .delete_trigger(branch.id, function.id, trigger_id)
                .await
                .map_err(|e| metadata_error("delete trigger", e))?
            {
                return Err(trigger_not_found());
            }
            info!(request_id = %principal.request_id, branch_id = %branch.id, function = %name, trigger = %trigger_id, "function trigger deleted");
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        .await,
    )
}
