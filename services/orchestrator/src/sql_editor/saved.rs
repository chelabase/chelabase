//! SQL editor saved queries: each user's own, per Chelabase database.

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_metadata::{SavedQuery, SavedQueryEditor, SavedQueryPatch, SavedQuerySummary, SavedQueryWrite};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::access::PlatformRole;
use chelabase_types::{ApiJson, ApiPath};
use serde::Deserialize;
use tracing::error;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{SAVED_QUERY_CAP, invalid_sql};
use crate::branches::{conflict, unprocessable};
use crate::access::{authorize, authorize_database, Target};
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort};
use chelabase_routes::orchestrator as ep;

const MAX_NAME_CHARS: usize = 120;
/// Postgres identifiers are at most 63 bytes.
const MAX_HINT_BYTES: usize = 63;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_saved_queries, create_saved_query))
        .routes(routes!(get_saved_query, update_saved_query, delete_saved_query))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateSavedQueryRequest {
    /// 1-120 characters after trimming; unique per user and database, ignoring case.
    name: String,
    /// 1 byte to 1 MiB.
    sql: String,
    /// The Postgres database the editor last ran it in (up to 63 bytes; empty is stored as null).
    pg_database: Option<String>,
    /// The role the editor last ran it as (up to 63 bytes; empty is stored as null); absent: the owner.
    role: Option<String>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateSavedQueryRequest {
    /// The version the client last saw; a mismatch answers 409 `saved_query_conflict`.
    version: i32,
    name: Option<String>,
    sql: Option<String>,
    /// `null` or `""` clears the hint; absent leaves it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>, nullable)]
    pg_database: Option<Option<String>>,
    /// `null` or `""` clears the hint; absent leaves it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>, nullable)]
    role: Option<Option<String>>,
}

/// A present field, `null` included, is `Some`; an absent one stays `None` (via `default`).
fn present<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

fn saved_not_found() -> ApiError {
    ApiError::not_found("saved query not found")
}

fn internal(err: impl std::fmt::Display) -> ApiError {
    error!("saved queries: {err}");
    ApiError::internal()
}

/// Who is editing: an Admin may change any saved query; anyone else only their own, and a query
/// saved before members has no author (Admin only).
fn editor(
    principal: &AuthPrincipal,
    grant: Option<&chelabase_types::access::Effective>,
) -> Result<SavedQueryEditor, ApiError> {
    if grant.is_some_and(|g| g.platform_role == PlatformRole::Admin) {
        return Ok(SavedQueryEditor::Admin);
    }
    match principal.member.as_ref() {
        Some(member) => Ok(SavedQueryEditor::Author(member.id)),
        None => Err(internal("an edit passed access without a member")),
    }
}

/// The saved query, or the refusal for `by`: 404 when there is none in this database, 403
/// `forbidden{required:"admin"}` when it is visible but not theirs (saved queries are shared in
/// the project; a legacy row has no author, so only an Admin changes it).
async fn editable<M, J>(
    state: &AppState<M, J>,
    database: Uuid,
    query_id: Uuid,
    by: SavedQueryEditor,
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let Some(row) = state.metadata.get_saved_query(database, query_id).await.map_err(internal)? else {
        return Err(saved_not_found());
    };
    match by {
        SavedQueryEditor::Admin => Ok(()),
        SavedQueryEditor::Author(member) if row.author_member_id == Some(member) => Ok(()),
        SavedQueryEditor::Author(_) => Err(crate::access::deny_to_api(chelabase_types::access::Deny::Forbidden {
            required: PlatformRole::Admin.as_str().to_string(),
        })),
    }
}

fn valid_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(unprocessable("`name` must be 1 to 120 characters"));
    }
    Ok(name.to_owned())
}

fn valid_sql(sql: &str) -> Result<(), ApiError> {
    match invalid_sql(sql) {
        Some(message) => Err(unprocessable(message)),
        None => Ok(()),
    }
}

/// An empty hint names nothing: it is stored as null.
fn hint(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

fn valid_hint(field: &str, value: Option<&str>) -> Result<(), ApiError> {
    match value {
        Some(v) if v.len() > MAX_HINT_BYTES => {
            Err(unprocessable(&format!("`{field}` must be at most 63 bytes")))
        }
        _ => Ok(()),
    }
}

fn name_taken() -> ApiError {
    conflict(ErrorCode::SavedQueryNameTaken, "you already have a saved query with that name")
}

fn stale(current: SavedQuery) -> ApiError {
    conflict(ErrorCode::SavedQueryConflict, "the saved query changed since you loaded it")
        .with_extra("current", serde_json::to_value(current).unwrap_or_default())
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/sql-queries",
    operation_id = "listSavedQueries",
    tag = "sql-editor",
    summary = "List your saved SQL queries",
    description = "The caller's saved queries for the database, by name (ignoring case), without their SQL text. Saved queries are private to their author.",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The caller's saved queries", body = Vec<SavedQuerySummary>),
        (status = 400, description = "`invalid_request`: the path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or is another tenant's", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_saved_queries<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Vec<SavedQuerySummary>>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    authorize_database(&state, &principal, ep::LIST_SAVED_QUERIES.policy.access, Target::Database(id)).await?;
    let rows = state.metadata.list_saved_queries(id).await.map_err(internal)?;
    Ok(Json(rows))
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/sql-queries",
    operation_id = "createSavedQuery",
    tag = "sql-editor",
    summary = "Save a SQL query",
    description = "Saves a query for the caller. A user keeps at most 500 per database.",
    params(("id" = Uuid, Path, description = "Database id")),
    request_body = CreateSavedQueryRequest,
    responses(
        (status = 201, description = "The saved query", body = SavedQuery),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or is another tenant's", body = ErrorEnvelope),
        (status = 409, description = "`saved_query_name_taken`: the caller has a query with that name (ignoring case); `saved_query_cap`: the caller already keeps 500", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown or missing field, `name` isn't 1 to 120 characters, `sql` isn't 1 byte to 1 MiB or holds a NUL byte, or a hint is over 63 bytes", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn create_saved_query<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(request): ApiJson<CreateSavedQueryRequest>,
) -> Result<Response, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    authorize_database(&state, &principal, ep::CREATE_SAVED_QUERY.policy.access, Target::Database(id)).await?;
    let name = valid_name(&request.name)?;
    valid_sql(&request.sql)?;
    valid_hint("pg_database", request.pg_database.as_deref())?;
    valid_hint("role", request.role.as_deref())?;
    let written = state
        .metadata
        .create_saved_query(
            id,
            &principal.sub,
            principal.member.as_ref().map(|m| m.id),
            Uuid::new_v4(),
            &name,
            &request.sql,
            hint(request.pg_database).as_deref(),
            hint(request.role).as_deref(),
        )
        .await
        .map_err(internal)?;
    match written {
        SavedQueryWrite::Saved(q) => Ok((StatusCode::CREATED, Json(q)).into_response()),
        SavedQueryWrite::NameTaken => Err(name_taken()),
        SavedQueryWrite::CapReached => Err(conflict(
            ErrorCode::SavedQueryCap,
            format!("you can keep at most {SAVED_QUERY_CAP} saved queries per database"),
        )),
        SavedQueryWrite::Stale(_) | SavedQueryWrite::NotFound => Err(internal("create returned an update outcome")),
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/sql-queries/{query_id}",
    operation_id = "getSavedQuery",
    tag = "sql-editor",
    summary = "Get a saved SQL query",
    description = "One of the caller's saved queries, with its SQL text.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("query_id" = Uuid, Path, description = "Saved query id"),
    ),
    responses(
        (status = 200, description = "The saved query", body = SavedQuery),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the saved query doesn't exist, or isn't the caller's", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_saved_query<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, query_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<SavedQuery>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    authorize_database(&state, &principal, ep::GET_SAVED_QUERY.policy.access, Target::Database(id)).await?;
    state
        .metadata
        .get_saved_query(id, query_id)
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(saved_not_found)
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/sql-queries/{query_id}",
    operation_id = "updateSavedQuery",
    tag = "sql-editor",
    summary = "Update a saved SQL query",
    description = "Changes the given fields of one of the caller's saved queries (an Admin may change any). `version` must be the version last seen; the version goes up by one with each change. `pg_database` and `role` accept `null` to clear them.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("query_id" = Uuid, Path, description = "Saved query id"),
    ),
    request_body = UpdateSavedQueryRequest,
    responses(
        (status = 200, description = "The updated saved query", body = SavedQuery),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: the query isn't yours (or has no author) and you aren't an Admin (`required: \"admin\"`)", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the saved query doesn't exist, or isn't the caller's", body = ErrorEnvelope),
        (status = 409, description = "`saved_query_conflict`: `version` is stale (the response carries the row as `error.current`); `saved_query_name_taken`: another query of the caller's has that name", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown or missing field, `name` isn't 1 to 120 characters, `sql` isn't 1 byte to 1 MiB or holds a NUL byte, or a hint is over 63 bytes", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_saved_query<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, query_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<UpdateSavedQueryRequest>,
) -> Result<Json<SavedQuery>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let authorized =
        authorize(&state, &principal, ep::UPDATE_SAVED_QUERY.policy.access, Target::Database(id)).await?;
    let by = editor(&principal, authorized.grant.as_ref())?;
    editable(&state, id, query_id, by).await?;
    let name = request.name.as_deref().map(valid_name).transpose()?;
    if let Some(sql) = &request.sql {
        valid_sql(sql)?;
    }
    valid_hint("pg_database", request.pg_database.as_ref().and_then(|v| v.as_deref()))?;
    valid_hint("role", request.role.as_ref().and_then(|v| v.as_deref()))?;
    let patch = SavedQueryPatch {
        name,
        sql: request.sql,
        pg_database: request.pg_database.map(hint),
        role: request.role.map(hint),
    };
    let written = state
        .metadata
        .update_saved_query(id, by, query_id, request.version, &patch)
        .await
        .map_err(internal)?;
    match written {
        SavedQueryWrite::Saved(q) => Ok(Json(q)),
        SavedQueryWrite::NameTaken => Err(name_taken()),
        SavedQueryWrite::Stale(current) => Err(stale(current)),
        SavedQueryWrite::NotFound => Err(saved_not_found()),
        SavedQueryWrite::CapReached => Err(internal("update returned CapReached")),
    }
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/sql-queries/{query_id}",
    operation_id = "deleteSavedQuery",
    tag = "sql-editor",
    summary = "Delete a saved SQL query",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("query_id" = Uuid, Path, description = "Saved query id"),
    ),
    responses(
        (status = 204, description = "The saved query is deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: the query isn't yours (or has no author) and you aren't an Admin (`required: \"admin\"`)", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the saved query doesn't exist, or isn't the caller's", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_saved_query<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, query_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let authorized =
        authorize(&state, &principal, ep::DELETE_SAVED_QUERY.policy.access, Target::Database(id)).await?;
    let by = editor(&principal, authorized.grant.as_ref())?;
    editable(&state, id, query_id, by).await?;
    if state.metadata.delete_saved_query(id, by, query_id).await.map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(saved_not_found())
    }
}
