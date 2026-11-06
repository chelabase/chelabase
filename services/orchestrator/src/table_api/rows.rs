//! `POST …/tables/{schema}/{table}/rows/default` (a row of all defaults) and
//! `POST …/tables/{schema}/{table}/cell` (one cell's full value, found by primary key), run on
//! the SQL editor's session: the owner's connection, or the caller's member role and `SET ROLE`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_routes::orchestrator as ep;
use chelabase_schema::catalog;
use chelabase_schema::cell::{
    CELL_STATEMENT_TIMEOUT_MS, KEY_TYPE_SQL, cell_query, decode_cell, primary_key,
};
use chelabase_schema::model::Table;
use chelabase_schema::sql::qualified;
use chelabase_types::ApiPath;
use chelabase_types::access::{Access, Requested};
use chelabase_metadata::{BranchRecord, DataApiState, DatabaseRecord};
use chelabase_types::api_error::{ApiError, ErrorEnvelope};
use chelabase_types::role_name::quote_ident;
use chelabase_types::table_api::{
    CellRequest, CellValue, ConsoleCellRequest, DefaultRow, DefaultRowRequest, cell_failure,
};
use serde_json::Value;
use tokio_postgres::SimpleQueryMessage;
use tokio_postgres::types::ToSql;
use tracing::{error, info};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::changes::{AUDIT_TARGET, Notices, collecting, failure, rollback, simple, simple_text};
use crate::access::{Authorized, Target};
use crate::branches::parse_body;
use crate::console::{
    at_developer, check_role, data_api_disabled, database_not_found, pass_on, pick_database,
};
use crate::sql_editor::run::AfterRun;
use crate::sql_editor::{
    Session, hand_over_after, invalid_role, load_authorized, open_session, pick_pg_database,
};
use crate::{ApiBytes, AppState, AuthPrincipal, JobPort, MetadataPort};

/// `statement_timeout` of both endpoints.
const STATEMENT_TIMEOUT_MS: u64 = 15_000;

pub(super) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(insert_default_row))
        .routes(routes!(get_cell))
}

/// An open session and what runs after it.
struct Opened {
    session: Session,
    after: Option<AfterRun>,
    branch_id: Uuid,
    pg_database: String,
}

/// Access, the Postgres database and the session (steps after the request's own checks). The
/// refusal is boxed: a `Response` is large next to an `Opened`.
async fn open<M, J>(
    state: &Arc<AppState<M, J>>,
    principal: &AuthPrincipal,
    id: Uuid,
    branch_id: Uuid,
    access: Access,
    database: Option<&str>,
    role: Option<&str>,
) -> Result<Opened, Box<Response>>
where
    M: MetadataPort,
    J: JobPort,
{
    let loaded = load(state, principal, id, branch_id, access, database, role)
        .await
        .map_err(|e| Box::new(e.into_response()))?;
    open_loaded(state, principal, loaded, branch_id, database, role).await
}

/// What [`load_authorized`] returns.
type Loaded = (Authorized, DatabaseRecord, BranchRecord);

/// The access check and the branch, for a request as `role` on `database`.
async fn load<M, J>(
    state: &Arc<AppState<M, J>>,
    principal: &AuthPrincipal,
    id: Uuid,
    branch_id: Uuid,
    access: Access,
    database: Option<&str>,
    role: Option<&str>,
) -> Result<Loaded, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let target = Target::Role {
        database: id,
        branch: branch_id,
        pg_database: database.unwrap_or(""),
        role: role.map_or(Requested::Owner, Requested::Role),
    };
    load_authorized(state, principal, access, target, branch_id).await
}

/// The Postgres database and the session, after [`load`].
async fn open_loaded<M, J>(
    state: &Arc<AppState<M, J>>,
    principal: &AuthPrincipal,
    (authorized, record, branch): Loaded,
    branch_id: Uuid,
    database: Option<&str>,
    role: Option<&str>,
) -> Result<Opened, Box<Response>>
where
    M: MetadataPort,
    J: JobPort,
{
    fn refused(e: impl IntoResponse) -> Box<Response> {
        Box::new(e.into_response())
    }
    let row = pick_pg_database(state, &branch, database)
        .await
        .map_err(refused)?;
    let session = open_session(
        state, &authorized, principal, &record, &branch, &row.name, role, true,
    )
    .await
    .map_err(refused)?;
    let after = match (session.member, session.set_role.as_ref()) {
        (Some(member), Some(acted)) => Some(hand_over_after(
            state.clone(),
            branch.id,
            member,
            row.name.clone(),
            acted.clone(),
            (
                session.connected.backend_pid(),
                session.connected.backend_start().to_owned(),
            ),
        )),
        _ => None,
    };
    Ok(Opened {
        session,
        after,
        branch_id,
        pg_database: row.name.clone(),
    })
}

/// `BEGIN` with the timeout and the acted role (`SET LOCAL`s).
async fn begin(session: &mut Session, read_only: bool) -> Result<(), ApiError> {
    let mut setup = vec![
        if read_only { "BEGIN READ ONLY" } else { "BEGIN" }.to_owned(),
        format!("SET LOCAL statement_timeout = {STATEMENT_TIMEOUT_MS}"),
    ];
    if let Some(role) = &session.set_role {
        setup.push(format!("SET LOCAL ROLE {}", quote_ident(role)));
    }
    for sql in &setup {
        if let Err(e) = simple(&mut session.connected, sql).await {
            rollback(&mut session.connected).await;
            return Err(failure(&e, None, false));
        }
    }
    Ok(())
}

/// A failure inside the transaction as these endpoints answer it: a table or schema that
/// doesn't exist is 404, the rest as P8 says.
fn row_failure(err: &tokio_postgres::Error) -> ApiError {
    let Some(db) = err.as_db_error() else {
        return failure(err, None, false);
    };
    let position = match db.position() {
        Some(tokio_postgres::error::ErrorPosition::Original(p)) => Some(*p),
        _ => None,
    };
    cell_failure(db.code().code(), db.message(), db.detail(), db.hint(), position)
}

/// The table's model (`None`: no such ordinary table), read before the transaction (it is its
/// own `BEGIN … COMMIT`) under the statement timeout.
async fn read_model(
    session: &mut Session,
    schema: &str,
    name: &str,
) -> Result<Option<Table>, ApiError> {
    let conn = &mut session.connected;
    simple(conn, &format!("SET statement_timeout = {STATEMENT_TIMEOUT_MS}"))
        .await
        .map_err(|e| failure(&e, None, false))?;
    let json = match simple_text(conn, &catalog::table_sql(schema, name)).await {
        Ok(json) => json.unwrap_or_default(),
        Err(e) => {
            rollback(conn).await;
            return Err(failure(&e, None, false));
        }
    };
    simple(conn, "RESET statement_timeout")
        .await
        .map_err(|e| failure(&e, None, false))?;
    catalog::parse_table(&json).map_err(|e| {
        error!("table API row endpoint: the table read's JSON: {e}");
        ApiError::internal()
    })
}

fn audit(
    principal: &AuthPrincipal,
    id: Uuid,
    branch_id: Uuid,
    pg_database: &str,
    role: Option<&str>,
    what: &str,
    status: u16,
) {
    info!(
        target: AUDIT_TARGET,
        user = %principal.sub,
        database_id = %id,
        branch_id = %branch_id,
        pg_database = %pg_database,
        role = %role.unwrap_or("owner"),
        status,
        "{what}"
    );
}

/// Ends the session, then the hand-over of an acted role (R5).
fn finish(opened: Opened) {
    let Opened { session, after, .. } = opened;
    drop(session);
    if let Some(after) = after {
        tokio::spawn(after);
    }
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}/rows/default",
    operation_id = "insertDefaultRow",
    tag = "tables",
    summary = "Insert a row of all defaults",
    description = "Runs `INSERT INTO <table> DEFAULT VALUES` in one committed transaction (as the owner, or as `role`) and returns the inserted row as JSON. A table whose columns have no defaults fails with the constraint's error (`constraint_violation`).",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("schema" = String, Path, description = "Schema name"),
        ("table" = String, Path, description = "Table name"),
    ),
    request_body = DefaultRowRequest,
    responses(
        (status = 200, description = "The inserted row", body = DefaultRow),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID; `constraint_violation`: a default breaks a NOT NULL, check or data rule (SQLSTATE 23502, 23514, 22xxx); `schema_change_failed`: Postgres refused the statement (e.g. the relation isn't a table)", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: below Developer on the project, or the role isn't in your set; `permission_denied`: the role may not insert (SQLSTATE 42501)", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the Postgres database or the table doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: a unique, foreign-key or exclusion constraint refuses the row, or a deadlock or serialization failure (40P01, 40001; retry);`owner_password_unknown`; `backup_branch`; `branch_not_ready`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body isn't the request's shape, or `role` is empty or a system role", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached or is out of connections right now (`Retry-After`)", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the insert ran past 15 s", body = ErrorEnvelope),
    ),
)]
async fn insert_default_row<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, schema, table)): ApiPath<(Uuid, Uuid, String, String)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let request: DefaultRowRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    if let Some(e) = invalid_role(request.role.as_deref()) {
        return e.into_response();
    }
    let mut opened = match open(
        &state,
        &principal,
        id,
        branch_id,
        ep::INSERT_DEFAULT_ROW.policy.access,
        request.database.as_deref(),
        request.role.as_deref(),
    )
    .await
    {
        Ok(o) => o,
        Err(r) => return *r,
    };
    let outcome = async {
        if read_model(&mut opened.session, &schema, &table).await?.is_none() {
            return Err(ApiError::not_found("no such table, or not an ordinary table"));
        }
        insert(&mut opened.session, &schema, &table).await
    }
    .await;
    let status = outcome.as_ref().map_or_else(|e| e.status, |_| 200);
    audit(
        &principal,
        id,
        opened.branch_id,
        &opened.pg_database,
        request.role.as_deref(),
        "default row",
        status,
    );
    finish(opened);
    match outcome {
        Ok(row) => (StatusCode::OK, Json(DefaultRow { row })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn insert(session: &mut Session, schema: &str, table: &str) -> Result<Value, ApiError> {
    begin(session, false).await?;
    let conn = &mut session.connected;
    let sql = format!(
        "WITH ins AS (INSERT INTO {} DEFAULT VALUES RETURNING *) \
         SELECT pg_catalog.to_jsonb(ins)::pg_catalog.text FROM ins",
        qualified(schema, table)
    );
    let json = match simple(conn, &sql).await {
        Ok(messages) => messages.iter().find_map(|m| match m {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
            _ => None,
        }),
        Err(e) => {
            rollback(conn).await;
            return Err(row_failure(&e));
        }
    };
    let row = json.and_then(|j| serde_json::from_str::<Value>(&j).ok());
    let Some(row) = row else {
        rollback(conn).await;
        error!("table API default row: the insert returned no row");
        return Err(ApiError::internal());
    };
    if let Err(e) = simple(conn, "COMMIT").await {
        rollback(conn).await;
        return Err(row_failure(&e));
    }
    Ok(row)
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}/cell",
    operation_id = "getCell",
    tag = "tables",
    summary = "One cell's full value",
    description = "The whole value of one column of the row with the given primary key, as text (a grid's preview cuts long values). `key` names exactly the primary key's columns; strings bind as given, numbers and booleans as their JSON text, compared at the column's type without its length or precision (never cut or rounded to fit). A value over 64 MiB is 413. Read as a role the caller holds below Developer, the read runs as the role through the data API's connection (rest), so the Postgres database must have the data API on (409 `data_api_disabled`) and the worker's role check applies (403 `role_refused`); the answers are the same.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("schema" = String, Path, description = "Schema name"),
        ("table" = String, Path, description = "Table name"),
    ),
    request_body = CellRequest,
    responses(
        (status = 200, description = "The value (`null` for a SQL NULL), its stored size in bytes and its type", body = CellValue),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, the table has no primary key, `key` doesn't name exactly the primary key's columns or has a value that isn't a string, number or boolean; `unknown_column`: `column` isn't one of the table's; `constraint_violation`: a key value can't be read as its column's type", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: below Viewer on the project, or the role isn't in your set; `role_refused`: the worker refuses the role (an owner-level role; below Developer only); `permission_denied`: the role may not read the column (SQLSTATE 42501)", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the Postgres database, the table or the row doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`owner_password_unknown`; `backup_branch`; `branch_not_ready`; `data_api_disabled`: the role is held below Developer and the database's data API is off", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the value is over 64 MiB", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body isn't the request's shape, or `role` is empty or a system role", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached or is out of connections right now (`Retry-After`)", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the read ran past 15 s", body = ErrorEnvelope),
    ),
)]
async fn get_cell<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, schema, table)): ApiPath<(Uuid, Uuid, String, String)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let request: CellRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    if let Some(e) = invalid_role(request.role.as_deref()) {
        return e.into_response();
    }
    let access = ep::GET_CELL.policy.access;
    let database = request.database.as_deref();
    let loaded = match load(&state, &principal, id, branch_id, access, database, request.role.as_deref()).await {
        Ok(loaded) => loaded,
        Err(e) => return e.into_response(),
    };
    // One access check: its `Authorized` also says whether the member role can `SET ROLE` to
    // the role (T2-2). A role held below Developer is read through rest (Q1, P40).
    if let Some(role) = request.role.as_deref() {
        match at_developer(&state, &loaded.0, Some(role), branch_id).await {
            Ok(true) => {}
            Ok(false) => {
                return cell_through_rest(&state, &principal, id, &loaded.2, role, &schema, &table, &request)
                    .await;
            }
            Err(e) => return e.into_response(),
        }
    }
    let mut opened = match open_loaded(&state, &principal, loaded, branch_id, database, request.role.as_deref()).await {
        Ok(o) => o,
        Err(r) => return *r,
    };
    let outcome = cell(&mut opened.session, &schema, &table, &request).await;
    let status = outcome.as_ref().map_or_else(|e| e.status, |_| 200);
    audit(
        &principal,
        id,
        opened.branch_id,
        &opened.pg_database,
        request.role.as_deref(),
        "cell read",
        status,
    );
    finish(opened);
    match outcome {
        Ok(cell) => (StatusCode::OK, Json(cell)).into_response(),
        Err(e) => e.into_response(),
    }
}

/// The cell as a role held below Developer: through rest's console path, which reads it as the
/// role (`authenticator`, then `SET ROLE`) in the data API's connection (P40). The branch is
/// ready (`load`), the Postgres database must serve the data API, and the worker's role check
/// applies; rest's answer comes back as it is.
#[allow(clippy::too_many_arguments)]
async fn cell_through_rest<M, J>(
    state: &Arc<AppState<M, J>>,
    principal: &AuthPrincipal,
    id: Uuid,
    branch: &BranchRecord,
    role: &str,
    schema: &str,
    table: &str,
    request: &CellRequest,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let rows = match state.metadata.branch_databases(branch.id).await {
        Ok(rows) => rows,
        Err(err) => {
            error!("list branch databases: {err}");
            return ApiError::internal().into_response();
        }
    };
    let Some(row) = pick_database(rows, request.database.as_deref()) else {
        return database_not_found().into_response();
    };
    if row.data_api != DataApiState::On {
        return data_api_disabled(&row.name).into_response();
    }
    let response = match check_role(state, branch.id, role).await {
        Err(e) => e.into_response(),
        Ok(()) => {
            let forward = ConsoleCellRequest {
                role: role.to_owned(),
                database: Some(row.name.clone()),
                schema: schema.to_owned(),
                table: table.to_owned(),
                key: request.key.clone(),
                column: request.column.clone(),
            };
            let timeout = Duration::from_millis(CELL_STATEMENT_TIMEOUT_MS) + Duration::from_secs(15);
            let reply = state.rest.cell(branch.id, &forward, timeout).await;
            pass_on(state, branch.id, &row.name, reply).await
        }
    };
    audit(
        principal,
        id,
        branch.id.as_uuid(),
        &row.name,
        Some(role),
        "cell read",
        response.status().as_u16(),
    );
    response
}

async fn cell(
    session: &mut Session,
    schema: &str,
    table: &str,
    request: &CellRequest,
) -> Result<CellValue, ApiError> {
    let Some(mut model) = read_model(session, schema, table).await? else {
        return Err(ApiError::not_found("no such table, or not an ordinary table"));
    };
    // Shape errors (no key, wrong key columns, unknown column) before any query.
    cell_query(&model, &request.key, &request.column)?;
    begin(session, true).await?;
    if let Err(e) = untyped_key_types(session, &mut model).await {
        rollback(&mut session.connected).await;
        return Err(row_failure(&e));
    }
    let (sql, params) = cell_query(&model, &request.key, &request.column)?;
    let (client, notices) = session.connected.client_and_notices();
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let rows = collecting(client.query(&sql, &refs), notices, &mut Notices::new()).await;
    rollback(&mut session.connected).await;
    let rows = rows.map_err(|e| row_failure(&e))?;
    let Some(row) = rows.first() else {
        return Err(ApiError::not_found("no row with that key"));
    };
    decode_cell(row.get(0), row.get(1), row.get(2), row.get(3))
}

/// Rewrites the primary-key columns' `data_type` to their type without its modifier
/// (`format_type(oid, NULL)`: `character varying`, not `character varying(5)`), so a key value
/// is compared as given instead of being cut or rounded to the column's length or precision
/// first (`'abcdefg'` mustn't find `'abcde'`, nor `1.249` find `1.25`).
async fn untyped_key_types(
    session: &mut Session,
    model: &mut Table,
) -> Result<(), tokio_postgres::Error> {
    let attnums: Vec<i16> = primary_key(model)
        .map(|pk| pk.iter().map(|c| c.attnum).collect())
        .unwrap_or_default();
    for attnum in attnums {
        let Some(column) = model.columns.iter_mut().find(|c| c.attnum == attnum) else {
            continue;
        };
        let row = session
            .connected
            .client()
            .query_one(KEY_TYPE_SQL, &[&column.type_oid.0])
            .await?;
        column.data_type = row.get(0);
    }
    Ok(())
}
