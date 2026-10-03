//! The data API per Postgres database of a branch: enabling and disabling a database, its
//! default schema, and the checks on a key's database allow-list.

use std::collections::HashSet;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use cheladb_metadata::{BranchKind, BranchRecord, BranchStatus, DataApiState, DatabaseOrigin, EnableStart};
use cheladb_nats::CheckDataRoleReply;
use cheladb_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use cheladb_types::{ApiPath, BranchId};
use serde::Deserialize;
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::branches::{BranchDatabaseView, conflict, load_branch, parse_body, unprocessable, valid_schema_name};
use crate::{
    ApiBytes, AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, ResetError, load_owned,
};

/// Most databases one key may name.
const MAX_KEY_DATABASES: usize = 20;
/// Longest Postgres identifier.
const MAX_DATABASE_NAME: usize = 63;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(enable_data_api, disable_data_api))
        .routes(routes!(update_pg_database))
}

/// The branch behind the path, ready to take a data API job: not a backup, and ready.
async fn ready_branch<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
    branch_id: Uuid,
) -> Result<BranchRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let record = load_owned(state, principal, id).await?;
    let branch = load_branch(state, &record, branch_id).await?;
    if branch.kind == BranchKind::Backup {
        return Err(conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL));
    }
    if branch.status != BranchStatus::Ready {
        return Err(conflict(ErrorCode::BranchNotReady, "branch is not ready"));
    }
    Ok(branch)
}

/// What the worker said about a disable or a rebootstrap, as an error when it isn't `ok`.
fn worker_outcome(
    branch: BranchId,
    reply: Result<CheckDataRoleReply, ResetError>,
) -> Result<(), ApiError> {
    match reply {
        Ok(reply) if reply.ok => Ok(()),
        Ok(reply) => match (reply.kind.as_deref(), reply.error) {
            (Some("not_ready"), _) => Err(conflict(ErrorCode::BranchNotReady, "branch is not ready")),
            (Some("refused"), Some(message)) => Err(conflict(ErrorCode::Conflict, message)),
            (kind, message) => {
                error!(branch_id = %branch, ?kind, "data API job failed: {message:?}");
                Err(ApiError::new(
                    502,
                    ErrorCode::Unavailable,
                    "the worker could not finish the request",
                ))
            }
        },
        Err(ResetError::Timeout) => Err(ApiError::new(
            504,
            ErrorCode::Timeout,
            "worker did not respond; try again",
        )),
        Err(ResetError::Transport(err)) => {
            error!(branch_id = %branch, "data API job: {err}");
            Err(ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable"))
        }
    }
}

/// Checks a key's database allow-list: 1 to 20 names, each 1 to 63 bytes, no duplicates, each
/// a database of the branch in any state; the system `postgres` database can't be served, so
/// it can't be named.
pub(crate) async fn validate_key_databases<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    names: &[String],
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    if names.is_empty() {
        return Err(unprocessable(
            "databases must name at least one database (use null for every served database)",
        ));
    }
    if names.len() > MAX_KEY_DATABASES {
        return Err(unprocessable(&format!(
            "databases may name at most {MAX_KEY_DATABASES} databases"
        )));
    }
    let mut seen = HashSet::new();
    for name in names {
        if name.is_empty() || name.len() > MAX_DATABASE_NAME {
            return Err(unprocessable(&format!(
                "each database name must be 1-{MAX_DATABASE_NAME} bytes"
            )));
        }
        if !seen.insert(name.as_str()) {
            return Err(unprocessable("databases must not repeat a name"));
        }
    }
    let rows = state.metadata.branch_databases(branch).await.map_err(|err| {
        error!("list branch databases: {err}");
        ApiError::internal()
    })?;
    for name in names {
        match rows.iter().find(|d| d.name == *name) {
            None => {
                return Err(unprocessable(&format!("the branch has no database {name:?}")));
            }
            Some(d) if d.origin == DatabaseOrigin::System => {
                return Err(unprocessable(&format!(
                    "the system database {name:?} is never served"
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/data-api",
    operation_id = "enableDataApi",
    tag = "data-api",
    summary = "Serve a Postgres database through the data API",
    description = "Marks the database `enabling` and asks the worker to bootstrap it; the answer is 202 at once. Poll the branch's databases for `data_api`: `on`, or `error` with the reason. Pressing it again retries a failed or stuck enable. A database already `on` answers 200 and nothing runs. The branch serves at most `CHELADB_BRANCH_DATA_API_DATABASE_CAP` databases (default 10, main included).",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "The Postgres database's name"),
    ),
    responses(
        (status = 200, description = "The database is already served", body = BranchDatabaseView),
        (status = 202, description = "The database is `enabling`", body = BranchDatabaseView),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready; `conflict`: the branch already serves as many databases as the cap allows", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the cluster's own `postgres` database is never served", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn enable_data_api<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let branch = match ready_branch(&state, &principal, id, branch_id).await {
        Ok(branch) => branch,
        Err(err) => return err.into_response(),
    };
    let cap = state.data_api_database_cap;
    let started = match state.metadata.begin_enable_data_api(branch.id, &name, cap).await {
        Ok(EnableStart::Started) => true,
        Ok(EnableStart::AlreadyOn) => false,
        Ok(EnableStart::Unknown) => {
            return ApiError::not_found("database not found").into_response();
        }
        Ok(EnableStart::System) => {
            return unprocessable("the system database \"postgres\" is never served").into_response();
        }
        Ok(EnableStart::OverCap(n)) => {
            return conflict(
                ErrorCode::Conflict,
                format!("this branch already serves {n} databases (the cap is {n})"),
            )
            .into_response();
        }
        Err(err) => {
            error!("begin enable data API: {err}");
            return ApiError::internal().into_response();
        }
    };
    if started {
        // The row says `enabling`; a lost message is healed by the worker's reconcile.
        if let Err(err) = state.js.enable_data_api(branch.id, &name).await {
            warn!(branch_id = %branch.id, "publish enable data API failed (reconcile will retry): {err}");
        }
        info!(request_id = %principal.request_id, branch_id = %branch.id, "data API enabling");
    }
    match state.metadata.branch_databases(branch.id).await {
        Ok(rows) => match rows.into_iter().find(|d| d.name == name) {
            Some(row) => {
                let status = if started { StatusCode::ACCEPTED } else { StatusCode::OK };
                (status, Json(BranchDatabaseView::from(row))).into_response()
            }
            None => ApiError::not_found("database not found").into_response(),
        },
        Err(err) => {
            error!("list branch databases: {err}");
            ApiError::internal().into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct RemoveQuery {
    remove: Option<bool>,
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/data-api",
    operation_id = "disableDataApi",
    tag = "data-api",
    summary = "Stop serving a Postgres database",
    description = "Stops serving the database at once. The `cheladb` schema and the grants stay, so enabling again is instant; `remove=true` also drops the `cheladb` schema and its event trigger (grants to `anon` and `authenticated` stay). Answers 204 after the worker has done it. The branch's main database can't be disabled.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "The Postgres database's name"),
        ("remove" = Option<bool>, Query, description = "Also drop the `cheladb` machinery in the database"),
    ),
    responses(
        (status = 204, description = "The database is no longer served"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the query is malformed", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the main database is always served, app users use the database, `remove=true` on a database the data API was never enabled on, or the worker refused (the message says why); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the cluster's own `postgres` database is never served", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 502, description = "`unavailable`: the worker failed to do it", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn disable_data_api<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
    query: Result<Query<RemoveQuery>, QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let branch = match ready_branch(&state, &principal, id, branch_id).await {
        Ok(branch) => branch,
        Err(err) => return err.into_response(),
    };
    let remove = match query {
        Ok(Query(RemoveQuery { remove })) => remove.unwrap_or(false),
        Err(rej) => {
            return ApiError::invalid_request(400, format!("invalid query: {}", rej.body_text()))
                .into_response();
        }
    };
    let rows = match state.metadata.branch_databases(branch.id).await {
        Ok(rows) => rows,
        Err(err) => {
            error!("list branch databases: {err}");
            return ApiError::internal().into_response();
        }
    };
    let Some(row) = rows.iter().find(|d| d.name == name) else {
        return ApiError::not_found("database not found").into_response();
    };
    // The project's users table lives there: the data API stays on until app users are off.
    match state.metadata.app_auth_for_database(branch.database_id).await {
        Ok(Some(auth)) if auth.pg_database == name => {
            return conflict(ErrorCode::Conflict, "app users use this database: disable app users first")
                .into_response();
        }
        Ok(_) => {}
        Err(err) => {
            error!("app auth for database: {err}");
            return ApiError::internal().into_response();
        }
    }
    match row.origin {
        DatabaseOrigin::Main => {
            return conflict(
                ErrorCode::Conflict,
                "the branch's main database is always served by the data API",
            )
            .into_response();
        }
        DatabaseOrigin::System => {
            return unprocessable("the system database \"postgres\" is never served").into_response();
        }
        _ => {}
    }
    // No enable ever began here: a `cheladb` schema in it isn't the data API's.
    if remove && row.data_api == DataApiState::Off && !row.data_api_bootstrapped {
        return conflict(
            ErrorCode::Conflict,
            format!("the data API was never enabled on the database {name:?}; there is nothing to remove"),
        )
        .into_response();
    }
    let reply = state.js.disable_data_api(branch.id, &name, remove).await;
    if let Err(err) = worker_outcome(branch.id, reply) {
        return err.into_response();
    }
    // Rest drops its pool and caches for the branch now, not when they expire; so does the
    // console's owner cache.
    state.rest.invalidate(branch.id).await;
    state.console.invalidate_branch(branch.id);
    info!(request_id = %principal.request_id, branch_id = %branch.id, remove, "data API disabled");
    StatusCode::NO_CONTENT.into_response()
}

/// A database's data API settings.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdatePgDatabaseRequest {
    /// The schema a request to this database uses when it names none: `^[a-z_][a-z0-9_]{0,62}$`.
    default_schema: String,
    /// Also grant the data API roles on the objects already in the schema (default false).
    /// Without it, only objects created from now on get grants (through default privileges),
    /// plus, once per database, the first schema's existing objects (a database whose first
    /// enable ran while its schema was missing gets that full first grant once the schema
    /// exists, flag or not). Grants only: it never revokes anything. Sent with an unchanged
    /// `default_schema` it grants on that schema's existing objects now, and answers 409
    /// `data_api_disabled` when the database isn't served.
    #[serde(default)]
    grant_existing: bool,
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/branches/{branch_id}/pg-databases/{name}",
    operation_id = "updatePgDatabase",
    tag = "data-api",
    summary = "Change a Postgres database's default schema",
    description = "Stores the schema a data API request to this database uses when it names none. On a served database the bootstrap re-runs for the new schema (so default privileges cover it) before the answer, and the data API picks the change up at once. Objects already in the new schema get the data API grants only with `grant_existing: true`; that flag with an unchanged schema grants on its existing objects now.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "The Postgres database's name"),
    ),
    request_body = UpdatePgDatabaseRequest,
    responses(
        (status = 200, description = "The database after the change", body = BranchDatabaseView),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready; `conflict`: the worker refused the bootstrap; `data_api_disabled`: `grant_existing` with an unchanged schema on a database the data API does not serve", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is the wrong shape, `default_schema` isn't `^[a-z_][a-z0-9_]{0,62}$`, or the database is the cluster's own `postgres` (never served)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 502, description = "`unavailable`: the worker failed to bootstrap the new schema; the old one stays", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn update_pg_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let branch = match ready_branch(&state, &principal, id, branch_id).await {
        Ok(branch) => branch,
        Err(err) => return err.into_response(),
    };
    let request: UpdatePgDatabaseRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(err) => return err.into_response(),
    };
    if !valid_schema_name(&request.default_schema) {
        return unprocessable("default_schema must match ^[a-z_][a-z0-9_]{0,62}$").into_response();
    }
    let rows = match state.metadata.branch_databases(branch.id).await {
        Ok(rows) => rows,
        Err(err) => {
            error!("list branch databases: {err}");
            return ApiError::internal().into_response();
        }
    };
    let Some(mut row) = rows.into_iter().find(|d| d.name == name) else {
        return ApiError::not_found("database not found").into_response();
    };
    if row.origin == DatabaseOrigin::System {
        return unprocessable("the system database \"postgres\" is never served").into_response();
    }
    let changed = row.default_schema != request.default_schema;
    if !changed && request.grant_existing && row.data_api != DataApiState::On {
        return crate::console::data_api_disabled(&name).into_response();
    }
    if !changed && !(request.grant_existing && row.data_api == DataApiState::On) {
        return Json(BranchDatabaseView::from(row)).into_response();
    }
    let previous = std::mem::replace(&mut row.default_schema, request.default_schema);
    if changed {
        match state
            .metadata
            .set_database_default_schema(branch.id, &name, &row.default_schema)
            .await
        {
            Ok(true) => {}
            Ok(false) => return ApiError::not_found("database not found").into_response(),
            Err(err) => {
                error!("set default schema: {err}");
                return ApiError::internal().into_response();
            }
        }
    }
    if row.data_api == DataApiState::On {
        let reply = state
            .js
            .rebootstrap_data_api(branch.id, &name, request.grant_existing)
            .await;
        if let Err(err) = worker_outcome(branch.id, reply) {
            // Put the old schema back, so what is stored is what the bootstrap covers.
            if changed
                && let Err(revert) = state
                    .metadata
                    .set_database_default_schema(branch.id, &name, &previous)
                    .await
            {
                error!(branch_id = %branch.id, "restoring the default schema failed: {revert}");
            }
            // rest may have read the new schema in between: it reads the stored one again.
            if changed {
                state.rest.invalidate(branch.id).await;
                state.console.invalidate_branch(branch.id);
            }
            return err.into_response();
        }
        state.rest.invalidate(branch.id).await;
        state.console.invalidate_branch(branch.id);
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, "default schema changed");
    Json(BranchDatabaseView::from(row)).into_response()
}
