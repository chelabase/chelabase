//! `POST …/tables/{schema}/{table}/rows/default` (a row of all defaults) and
//! `POST …/tables/{schema}/{table}/cell` (one cell's full value, found by primary key), run on
//! the SQL editor's session: the owner's connection, or the caller's member role and `SET ROLE`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_routes::orchestrator as ep;
use chelabase_schema::catalog;
use chelabase_schema::model::{ConstraintKind, Table};
use chelabase_schema::sql::{ident, qualified};
use chelabase_types::ApiPath;
use chelabase_types::access::{Access, Requested};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::role_name::quote_ident;
use chelabase_types::table_api::{CellRequest, CellValue, DefaultRow, DefaultRowRequest};
use serde_json::Value;
use tokio_postgres::SimpleQueryMessage;
use tokio_postgres::types::ToSql;
use tracing::{error, info};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::changes::{AUDIT_TARGET, Notices, collecting, failure, rollback, simple, simple_text};
use crate::access::Target;
use crate::branches::parse_body;
use crate::sql_editor::run::AfterRun;
use crate::sql_editor::{
    Session, hand_over_after, invalid_role, load_authorized, open_session, pick_pg_database,
};
use crate::{ApiBytes, AppState, AuthPrincipal, JobPort, MetadataPort};

/// `statement_timeout` of both endpoints.
const STATEMENT_TIMEOUT_MS: u64 = 15_000;
/// The largest cell value the cell endpoint returns.
const MAX_CELL_BYTES: i64 = 64 * 1024 * 1024;

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
    fn refused(e: impl IntoResponse) -> Box<Response> {
        Box::new(e.into_response())
    }
    let target = Target::Role {
        database: id,
        branch: branch_id,
        pg_database: database.unwrap_or(""),
        role: role.map_or(Requested::Owner, Requested::Role),
    };
    let (authorized, record, branch) = load_authorized(state, principal, access, target, branch_id)
        .await
        .map_err(refused)?;
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
    match err.as_db_error().map(|d| d.code().code()) {
        Some("42P01" | "3F000") => ApiError::not_found("no such table"),
        _ => failure(err, None, false),
    }
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

fn audit(principal: &AuthPrincipal, id: Uuid, opened: &Opened, role: Option<&str>, what: &str, status: u16) {
    info!(
        target: AUDIT_TARGET,
        user = %principal.sub,
        database_id = %id,
        branch_id = %opened.branch_id,
        pg_database = %opened.pg_database,
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
        (status = 403, description = "`forbidden`: below Developer on the project, or the role isn't in your set; `permission_denied`: the role may not insert (SQLSTATE 42501); `tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
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
    audit(&principal, id, &opened, request.role.as_deref(), "default row", status);
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

/// The primary key's columns of `table`, in key order; `None` without a primary key.
fn primary_key(table: &Table) -> Option<Vec<&chelabase_schema::model::Column>> {
    let pk = table
        .constraints
        .iter()
        .find(|c| c.kind == ConstraintKind::PrimaryKey)?;
    pk.columns
        .iter()
        .map(|n| table.columns.iter().find(|c| c.attnum == *n))
        .collect()
}

/// A key value as the text it binds as: strings as is, numbers and booleans as their JSON text.
fn key_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The cell query for `table`'s primary key and the values to bind (in `$n` order), or the 400.
fn cell_query(
    table: &Table,
    key: &BTreeMap<String, Value>,
    column: &str,
) -> Result<(String, Vec<String>), ApiError> {
    let Some(pk) = primary_key(table) else {
        return Err(ApiError::invalid_request(400, "the table has no primary key"));
    };
    let want: BTreeSet<&str> = pk.iter().map(|c| c.name.as_str()).collect();
    let got: BTreeSet<&str> = key.keys().map(String::as_str).collect();
    if want != got {
        return Err(ApiError::invalid_request(
            400,
            format!(
                "`key` must name exactly the primary key's columns: {}",
                pk.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        ));
    }
    let Some(target) = table.columns.iter().find(|c| c.name == column) else {
        return Err(ApiError::new(
            400,
            ErrorCode::UnknownColumn,
            format!("the table has no column {column:?}"),
        ));
    };
    let mut conditions = Vec::new();
    let mut params = Vec::new();
    for (i, c) in pk.iter().enumerate() {
        let Some(text) = key_text(&key[&c.name]) else {
            return Err(ApiError::invalid_request(
                400,
                format!("the key value of {:?} must be a string, a number or a boolean", c.name),
            ));
        };
        params.push(text);
        conditions.push(format!(
            "t.{} = ${}::pg_catalog.text::{}",
            ident(&c.name),
            i + 1,
            c.data_type
        ));
    }
    let col = format!("t.{}", ident(&target.name));
    // The cap is on the text Postgres would send: the cast runs once, in a lateral subquery
    // (`OFFSET 0` keeps the planner from inlining it), and an over-cap text never leaves the
    // server (only the flag does).
    let sql = format!(
        "SELECT CASE WHEN pg_catalog.octet_length(l.v) > {MAX_CELL_BYTES} THEN NULL ELSE l.v END, \
         COALESCE(pg_catalog.octet_length(l.v) > {MAX_CELL_BYTES}, false), \
         pg_catalog.pg_column_size({col}), pg_catalog.format_type({}, NULL) \
         FROM {} AS t CROSS JOIN LATERAL (SELECT {col}::pg_catalog.text AS v OFFSET 0) AS l WHERE {}",
        target.type_oid.0,
        qualified(&table.schema, &table.name),
        conditions.join(" AND ")
    );
    Ok((sql, params))
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}/cell",
    operation_id = "getCell",
    tag = "tables",
    summary = "One cell's full value",
    description = "The whole value of one column of the row with the given primary key, as text (a grid's preview cuts long values). `key` names exactly the primary key's columns; strings bind as given, numbers and booleans as their JSON text, compared at the column's type without its length or precision (never cut or rounded to fit). A value over 64 MiB is 413.",
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
        (status = 403, description = "`forbidden`: below Viewer on the project, or the role isn't in your set; `permission_denied`: the role may not read the column (SQLSTATE 42501); `tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the Postgres database, the table or the row doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`owner_password_unknown`; `backup_branch`; `branch_not_ready`", body = ErrorEnvelope),
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
    let mut opened = match open(
        &state,
        &principal,
        id,
        branch_id,
        ep::GET_CELL.policy.access,
        request.database.as_deref(),
        request.role.as_deref(),
    )
    .await
    {
        Ok(o) => o,
        Err(r) => return *r,
    };
    let outcome = cell(&mut opened.session, &schema, &table, &request).await;
    let status = outcome.as_ref().map_or_else(|e| e.status, |_| 200);
    audit(&principal, id, &opened, request.role.as_deref(), "cell read", status);
    finish(opened);
    match outcome {
        Ok(cell) => (StatusCode::OK, Json(cell)).into_response(),
        Err(e) => e.into_response(),
    }
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
    let too_large = || ApiError::new(413, ErrorCode::TooLarge, "the value is over 64 MiB");
    let rows = rows.map_err(|e| {
        // The text form is over Postgres's own 1 GB limit (program_limit_exceeded).
        if e.as_db_error().is_some_and(|d| d.code().code() == "54000") {
            too_large()
        } else {
            row_failure(&e)
        }
    })?;
    let Some(row) = rows.first() else {
        return Err(ApiError::not_found("no row with that key"));
    };
    let value: Option<String> = row.get(0);
    let over: bool = row.get(1);
    let bytes: Option<i32> = row.get(2);
    let data_type: String = row.get(3);
    let bytes = i64::from(bytes.unwrap_or(0));
    // Backstop: the flag is the check; these catch a value that slipped past it.
    if over
        || bytes > MAX_CELL_BYTES
        || value.as_ref().is_some_and(|v| v.len() as i64 > MAX_CELL_BYTES)
    {
        return Err(too_large());
    }
    Ok(CellValue { value, bytes, data_type })
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
            .query_one(
                "SELECT pg_catalog.format_type($1::pg_catalog.oid, NULL)",
                &[&column.type_oid.0],
            )
            .await?;
        column.data_type = row.get(0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn table() -> Table {
        serde_json::from_value(json!({
            "oid": 1, "schema": "public", "name": "t", "owner": {"name": "o", "oid": 2},
            "comment": null, "rls_enabled": false, "rls_forced": false, "unlogged": false,
            "options": [],
            "columns": [
                {"attnum": 1, "name": "id", "data_type": "bigint", "type_oid": 20, "collation": null,
                 "default": null, "not_null": true, "identity": null, "generated": null, "comment": null, "grants": []},
                {"attnum": 2, "name": "Body", "data_type": "text", "type_oid": 25, "collation": null,
                 "default": null, "not_null": false, "identity": null, "generated": null, "comment": null, "grants": []}
            ],
            "constraints": [
                {"oid": 3, "name": "t_pkey", "kind": "primary_key", "definition": "PRIMARY KEY (id)",
                 "columns": [1], "references": null, "deferrable": false, "initially_deferred": false, "validated": true}
            ],
            "indexes": [], "policies": [], "grants": []
        }))
        .unwrap()
    }

    fn key(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    }

    #[test]
    fn cell_query_binds_text_and_quotes_names() {
        let (sql, params) = cell_query(&table(), &key(&[("id", json!(1))]), "Body").unwrap();
        assert_eq!(params, ["1"]);
        assert!(sql.contains("t.\"Body\"") && sql.contains("t.id = $1::pg_catalog.text::bigint"), "{sql}");
        let (_, params) = cell_query(&table(), &key(&[("id", json!(true))]), "id").unwrap();
        assert_eq!(params, ["true"]);
    }

    #[test]
    fn cell_query_caps_the_text_form_server_side() {
        let (sql, _) = cell_query(&table(), &key(&[("id", json!(1))]), "Body").unwrap();
        assert!(sql.contains("octet_length(l.v) > 67108864 THEN NULL"), "{sql}");
        assert!(sql.contains("LATERAL (SELECT t.\"Body\"::pg_catalog.text AS v OFFSET 0)"), "{sql}");
    }

    #[test]
    fn cell_query_refuses_bad_keys_and_columns() {
        let t = table();
        for (k, c, code) in [
            (key(&[("id", json!([1]))]), "id", ErrorCode::InvalidRequest),
            (key(&[("id", json!(null))]), "id", ErrorCode::InvalidRequest),
            (key(&[("id", json!(1)), ("x", json!(1))]), "id", ErrorCode::InvalidRequest),
            (key(&[]), "id", ErrorCode::InvalidRequest),
            (key(&[("id", json!(1))]), "nope", ErrorCode::UnknownColumn),
        ] {
            let e = cell_query(&t, &k, c).unwrap_err();
            assert_eq!((e.status, e.code), (400, code), "{k:?} {c}");
        }
        let mut no_pk = table();
        no_pk.constraints.clear();
        let e = cell_query(&no_pk, &key(&[("id", json!(1))]), "id").unwrap_err();
        assert_eq!((e.status, e.code), (400, ErrorCode::InvalidRequest));
    }
}
