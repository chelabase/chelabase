//! `POST …/schema-changes`: the table API's DDL and enum changes, rendered by
//! `chelabase_schema::ddl` and run on the SQL editor's session (the owner's connection, or the
//! caller's member role and `SET ROLE`), in one transaction (a dry run always rolls back), or,
//! for a single `CONCURRENTLY` index operation, outside one.
//!
//! Every request that ran writes one `info` line on the `orchestrator::table_api::audit`
//! target: who, where, as whom, how many operations, dry run or not, committed or not (never
//! the SQL or any value).

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_metadata::BranchRecord;
use chelabase_routes::orchestrator as ep;
use chelabase_schema::catalog;
use chelabase_schema::ddl::{
    self, ContextTable, DdlError, RenderContext, RenderedStatement, TargetParts, cascaded_objects,
};
use chelabase_schema::sql::{literal, qualified};
use chelabase_types::ApiPath;
use chelabase_types::access::Requested;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::role_name::quote_ident;
use chelabase_types::table_api::{
    Dependent, Dependents, IndexKey, IndexSpec, Operation, SchemaChangeRequest,
    SchemaChangeResponse, SkipReason, StatementResult, TableRef, classify_failure,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_postgres::error::{DbError, ErrorPosition};
use tokio_postgres::SimpleQueryMessage;
use tracing::{error, info, warn};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::privilege_ops::{self, PrivilegeChecks, Prepared};
use crate::access::Target;
use crate::branches::{parse_body, unprocessable};
use crate::console::unavailable;
use crate::sql_editor::run::Connected;
use crate::sql_editor::{
    Session, connect_as_owner, hand_over_after, invalid_role, load_authorized, open_session,
    pick_pg_database,
};
use crate::{ApiBytes, AppState, AuthPrincipal, JobPort, MetadataPort, roles};

/// The audit log's target (shared with the row endpoints).
pub(super) const AUDIT_TARGET: &str = "orchestrator::table_api::audit";
/// `lock_timeout` when the request names none, and the most it may name.
const DEFAULT_LOCK_TIMEOUT_MS: u64 = 5_000;
const MAX_LOCK_TIMEOUT_MS: u64 = 60_000;
/// `statement_timeout` of a dry run, a real run, and a `CONCURRENTLY` index operation.
const DRY_RUN_STATEMENT_TIMEOUT_MS: u64 = 15_000;
const REAL_STATEMENT_TIMEOUT_MS: u64 = 60_000;
const CONCURRENT_STATEMENT_TIMEOUT_MS: u64 = 900_000;
/// Notices a statement's result keeps (every notice is still read for the cascade check).
const NOTICES_KEPT: usize = 100;
/// What the dependents query raises for a drop target the catalog doesn't have.
const MISSING_TARGET: &str = "chelabase: drop target ";

pub(super) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new().routes(routes!(apply_schema_changes))
}

/// The value, or the error to answer with instead.
macro_rules! or_answer {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(err) => return IntoResponse::into_response(err),
        }
    };
}

/// 400 `invalid_request` for an operation the renderer refuses.
fn refused(err: &DdlError) -> ApiError {
    ApiError::invalid_request(400, err.to_string()).with_extra("op_index", json!(err.op_index))
}

/// R6 / MH-30: an owner run is the route's Admin rule (Admin implies the owner, R7), so a
/// caller below Admin is told `required: "admin"` rather than the SQL editor's `"owner"`.
fn owner_run_needs_admin(mut err: ApiError, owner_run: bool) -> ApiError {
    if owner_run && err.status == 403 && err.code == ErrorCode::Forbidden {
        for (key, value) in &mut err.extra {
            if key == "required" && value == "owner" {
                *value = json!("admin");
            }
        }
    }
    err
}

/// The request's checks that need no compute (step 1): its `lock_timeout` in ms, or the error.
fn validate(request: &SchemaChangeRequest) -> Result<u64, ApiError> {
    if request.operations.is_empty() {
        return Err(unprocessable("`operations` must not be empty"));
    }
    let lock_timeout = request.lock_timeout_ms.unwrap_or(DEFAULT_LOCK_TIMEOUT_MS);
    if !(1..=MAX_LOCK_TIMEOUT_MS).contains(&lock_timeout) {
        return Err(unprocessable("`lock_timeout_ms` must be 1 to 60000"));
    }
    if let Some(refusal) = invalid_role(request.role.as_deref()) {
        return Err(refusal);
    }
    if let Some(cascade) = &request.cascade
        && !is_hash(&cascade.dependents_hash)
    {
        return Err(unprocessable(
            "`cascade.dependents_hash` must be 64 lowercase hex characters (a dry run's `dependents.hash`)",
        ));
    }
    if let Some(e) = request.expect_acl.iter().find(|e| !is_hash(&e.acl_hash)) {
        return Err(unprocessable(&format!(
            "`expect_acl`: {:?} isn't 64 lowercase hex characters (a privileges read's `acl_hash`)",
            e.acl_hash
        )));
    }
    // What the compute decides is left open here: the server version (MAINTAIN), the
    // functions (resolved in the transaction, P11) and the session role (the grantee check).
    let resolved_functions = request
        .operations
        .iter()
        .filter_map(|op| match op {
            Operation::GrantFunction { function, .. } | Operation::RevokeFunction { function, .. } => {
                Some((function.oid, "pg_catalog.now()".to_string()))
            }
            _ => None,
        })
        .collect();
    let ctx = RenderContext {
        tables: Vec::new(),
        cascade: request.cascade.is_some(),
        resolved_functions,
        server_version_num: i32::MAX,
        ..Default::default()
    };
    let statements = ddl::render(&request.operations, &ctx).map_err(|e| refused(&e))?;
    for (op_index, op) in request.operations.iter().enumerate() {
        if let Operation::CreateIndex(spec) = op
            && spec.concurrently
            && spec.name.is_none()
        {
            return Err(ApiError::invalid_request(
                400,
                format!(
                    "operation {op_index}: a concurrent index needs a `name` (a failed build's INVALID index is dropped by name)"
                ),
            )
            .with_extra("op_index", json!(op_index)));
        }
    }
    if let Some(s) = statements.iter().find(|s| s.sql.contains('\0')) {
        return Err(ApiError::invalid_request(
            400,
            format!(
                "operation {}: the SQL must not contain a NUL byte",
                s.op_index
            ),
        )
        .with_extra("op_index", json!(s.op_index)));
    }
    Ok(lock_timeout)
}

/// A SHA-256 in lowercase hex.
fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `CHELABASE_TABLE_API_DRY_RUN_MAX_BYTES`: 268435456 (256 MiB) when unset or blank, otherwise
/// a whole number of bytes from 0.
pub fn dry_run_max_bytes(value: Option<&str>) -> Result<i64, String> {
    const NAME: &str = "CHELABASE_TABLE_API_DRY_RUN_MAX_BYTES";
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(DEFAULT_DRY_RUN_MAX_BYTES),
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .ok_or_else(|| format!("{NAME}={v:?}: expected a whole number of bytes")),
    }
}

/// The default of [`dry_run_max_bytes`].
pub const DEFAULT_DRY_RUN_MAX_BYTES: i64 = 268_435_456;

/// What a statement reported while it ran: the notices kept for the response and, read from
/// every one, the objects Postgres said a cascading drop removed (`None` once a notice can't be
/// read as a complete list).
pub(super) struct Notices {
    kept: Vec<String>,
    cascaded: Option<BTreeSet<String>>,
    /// P29: Postgres warned that it granted or revoked less than asked (01006, 01007).
    not_applied: bool,
}

impl Notices {
    pub(super) fn new() -> Self {
        Notices {
            kept: Vec::new(),
            cascaded: Some(BTreeSet::new()),
            not_applied: false,
        }
    }

    fn push(&mut self, notice: &DbError) {
        // P29: matched by SQLSTATE (`privilege_not_revoked`, `privilege_not_granted`), never
        // by text; a relation-level REVOKE sends one per column.
        if matches!(notice.code().code(), "01006" | "01007") {
            self.not_applied = true;
        }
        match cascaded_objects(notice.message(), notice.detail()) {
            Some(objects) => {
                if let Some(set) = &mut self.cascaded {
                    set.extend(objects);
                }
            }
            None => self.cascaded = None,
        }
        if self.kept.len() < NOTICES_KEPT {
            let mut text = format!("{}: {}", notice.severity(), notice.message());
            if let Some(detail) = notice.detail() {
                text.push_str("\nDETAIL: ");
                text.push_str(detail);
            }
            self.kept.push(text);
        }
    }
}

/// Awaits `query` while taking every notice its connection receives (so a flood of them can't
/// stall the connection), into `sink`; once the query is done, the notices it caused are queued
/// and taken too.
pub(super) async fn collecting<F: Future>(
    query: F,
    notices: &mut mpsc::Receiver<DbError>,
    sink: &mut Notices,
) -> F::Output {
    let mut query = std::pin::pin!(query);
    loop {
        tokio::select! {
            biased;
            out = &mut query => {
                while let Ok(notice) = notices.try_recv() {
                    sink.push(&notice);
                }
                return out;
            }
            Some(notice) = notices.recv() => sink.push(&notice),
        }
    }
}

/// One of our own queries (no caller text), its notices dropped.
pub(super) async fn simple(
    conn: &mut Connected,
    sql: &str,
) -> Result<Vec<SimpleQueryMessage>, tokio_postgres::Error> {
    let (client, notices) = conn.client_and_notices();
    collecting(client.simple_query(sql), notices, &mut Notices::new()).await
}

/// The first column of the first row `sql` returns.
pub(super) async fn simple_text(
    conn: &mut Connected,
    sql: &str,
) -> Result<Option<String>, tokio_postgres::Error> {
    Ok(simple(conn, sql).await?.iter().find_map(|m| match m {
        SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
        _ => None,
    }))
}

/// Ends the transaction without its changes. A failure is moot: the session is closed next.
pub(super) async fn rollback(conn: &mut Connected) {
    let _ = simple(conn, "ROLLBACK").await;
}

/// A failed statement as the change endpoint answers it (P8): the typed code by SQLSTATE and
/// Postgres's fields; `dry_run: true` on a dry run (P10). A broken connection is 503.
pub(super) fn failure(err: &tokio_postgres::Error, op_index: Option<usize>, dry_run: bool) -> ApiError {
    let Some(db) = err.as_db_error() else {
        warn!("table API change: the connection broke: {err}");
        return unavailable("the connection to the branch's compute broke; try again");
    };
    db_failure(db, op_index, dry_run)
}

fn db_failure(db: &DbError, op_index: Option<usize>, dry_run: bool) -> ApiError {
    let sqlstate = db.code().code();
    let (status, code) = classify_failure(sqlstate, db.hint());
    with_fields(
        ApiError::new(status, code, db.message()),
        db,
        op_index,
        dry_run,
    )
}

/// `op_index`, `sqlstate`, `message`, `detail`, `hint`, `position` (and `dry_run`).
fn with_fields(err: ApiError, db: &DbError, op_index: Option<usize>, dry_run: bool) -> ApiError {
    let position = match db.position() {
        Some(ErrorPosition::Original(p)) => json!(p),
        _ => Value::Null,
    };
    let err = err
        .with_extra("op_index", json!(op_index))
        .with_extra("sqlstate", json!(db.code().code()))
        .with_extra("message", json!(db.message()))
        .with_extra("detail", json!(db.detail()))
        .with_extra("hint", json!(db.hint()))
        .with_extra("position", position);
    if dry_run {
        err.with_extra("dry_run", json!(true))
    } else {
        err
    }
}

/// `dry_run: true` on a dry run's error (P10).
pub(super) fn on_dry(err: ApiError, dry_run: bool) -> ApiError {
    if dry_run {
        err.with_extra("dry_run", json!(true))
    } else {
        err
    }
}

/// A failure of one of our own reads: a lock not granted in time is 409 `lock_timeout`, a
/// timeout 504, a broken connection 503, anything else ours to fix (500, logged).
pub(super) fn read_failure(err: &tokio_postgres::Error, what: &str, dry_run: bool) -> ApiError {
    let out = match err.as_db_error() {
        None => failure(err, None, false),
        Some(db) if db.code().code() == "55P03" => ApiError::new(
            409,
            ErrorCode::LockTimeout,
            format!("{what}: a lock wasn't granted within `lock_timeout_ms`; try again"),
        ),
        Some(db) if db.code().code() == "57014" => {
            ApiError::new(504, ErrorCode::Timeout, format!("{what} took too long"))
        }
        Some(db) => {
            error!(
                sqlstate = db.code().code(),
                "table API change, {what}: {}",
                db.message()
            );
            ApiError::internal()
        }
    };
    on_dry(out, dry_run)
}

/// The dependents query's failure, for the query over `ops` (the request's operations from
/// index `base`): a missing drop target is 404 with its `op_index`; anything else as a failed
/// statement of the first drop.
fn dependents_failure(
    err: &tokio_postgres::Error,
    ops: &[Operation],
    base: usize,
    dry_run: bool,
) -> ApiError {
    if let Some(db) = err.as_db_error()
        && let Some(label) = missing_target(db)
    {
        let op_index = ops
            .iter()
            .position(|op| ddl::drop_target_label(op).as_deref() == Some(label))
            .map(|i| base + i);
        return on_dry(
            ApiError::not_found(format!("the drop target {label} doesn't exist"))
                .with_extra("op_index", json!(op_index)),
            dry_run,
        );
    }
    failure(err, first_drop(ops).map(|i| base + i), dry_run)
}

/// `table public.a` of the dependents query's `chelabase: drop target table public.a not found`.
fn missing_target(db: &DbError) -> Option<&str> {
    if db.code().code() != "22P02" {
        return None;
    }
    missing_label(db.message())
}

fn missing_label(message: &str) -> Option<&str> {
    let (_, rest) = message.split_once(MISSING_TARGET)?;
    rest.split_once(" not found").map(|(label, _)| label)
}

/// 409 `dependents_changed`, with the dependents as they are now.
fn dependents_changed(message: &str, now: &Dependents, dry_run: bool) -> ApiError {
    on_dry(
        ApiError::new(409, ErrorCode::DependentsChanged, message)
            .with_extra("dependents", serde_json::to_value(now).unwrap_or_default()),
        dry_run,
    )
}

/// 409 `has_dependents` (user, 2026-10-13): a drop has dependents and the request confirms no
/// cascade. `dependents` is what a cascade would also drop and the hash to confirm it with.
fn has_dependents(now: &Dependents, dry_run: bool) -> ApiError {
    on_dry(
        ApiError::new(
            409,
            ErrorCode::HasDependents,
            "other objects depend on what the request drops; nothing was changed. To drop them \
             too, review `dependents` and send its hash as `cascade.dependents_hash`",
        )
        .with_extra("dependents", serde_json::to_value(now).unwrap_or_default()),
        dry_run,
    )
}

fn dependents_of(objects: Vec<Dependent>) -> Dependents {
    Dependents {
        hash: ddl::dependents_hash(&objects),
        objects,
    }
}

/// The dependents of the drop targets of `ops` (the request's operations from index `base`),
/// or `None` when they drop nothing with dependents.
async fn read_dependents(
    conn: &mut Connected,
    ops: &[Operation],
    base: usize,
    dry_run: bool,
) -> Result<Option<Dependents>, ApiError> {
    let Some(sql) = ddl::dependents_sql(ops) else {
        return Ok(None);
    };
    let json = simple_text(conn, &sql)
        .await
        .map_err(|e| dependents_failure(&e, ops, base, dry_run))?
        .ok_or_else(|| {
            error!("table API change: the dependents query returned no row");
            ApiError::internal()
        })?;
    ddl::parse_dependents(&json)
        .map(|objects| Some(dependents_of(objects)))
        .map_err(|e| {
            error!("table API change: the dependents query's JSON: {e}");
            ApiError::internal()
        })
}

/// Each drop target of the request and its own parts (op indexes are the request's).
async fn read_target_parts(
    conn: &mut Connected,
    ops: &[Operation],
    dry_run: bool,
) -> Result<Vec<TargetParts>, ApiError> {
    let Some(sql) = ddl::target_parts_sql(ops) else {
        return Ok(Vec::new());
    };
    let json = simple_text(conn, &sql)
        .await
        .map_err(|e| failure(&e, first_drop(ops), dry_run))?
        .ok_or_else(|| {
            error!("table API change: the target parts query returned no row");
            ApiError::internal()
        })?;
    ddl::parse_target_parts(&json).map_err(|e| {
        error!("table API change: the target parts query's JSON: {e}");
        ApiError::internal()
    })
}

/// The oid (text) of the relation `schema.name`, if the catalog has one.
async fn relation_oid(
    conn: &mut Connected,
    schema: &str,
    name: &str,
    dry_run: bool,
) -> Result<Option<String>, ApiError> {
    let sql = format!(
        "SELECT c.oid::pg_catalog.text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n \
         ON n.oid = c.relnamespace WHERE n.nspname = {} AND c.relname = {}",
        literal(schema),
        literal(name)
    );
    simple_text(conn, &sql)
        .await
        .map_err(|e| read_failure(&e, "reading the catalog", dry_run))
}

/// What the request's tables are before it runs: the models (and sizes) of the tables it works
/// on, and the tables it names that the catalog has no relation for. Read on the session before
/// its transaction (`table_sql` is its own `BEGIN … COMMIT`), under the session's
/// `lock_timeout` and `statement_timeout`. A relation that isn't an ordinary table has no model.
async fn read_context(
    conn: &mut Connected,
    ops: &[Operation],
    dry_run: bool,
) -> Result<(Vec<ContextTable>, Vec<TableRef>), ApiError> {
    let mut tables = Vec::new();
    let mut absent = Vec::new();
    for t in ddl::context_tables(ops) {
        let json = simple_text(conn, &catalog::table_sql(&t.schema, &t.name))
            .await
            .map_err(|e| read_failure(&e, "reading the table", dry_run))?
            .unwrap_or_default();
        let table = match catalog::parse_table(&json) {
            Ok(Some(table)) => table,
            Ok(None) => {
                if relation_oid(conn, &t.schema, &t.name, dry_run)
                    .await?
                    .is_none()
                {
                    absent.push(t);
                }
                continue;
            }
            Err(e) => {
                error!("table API change: the table read's JSON: {e}");
                return Err(ApiError::internal());
            }
        };
        let size_sql = format!(
            "SELECT pg_catalog.pg_total_relation_size(c.oid)::pg_catalog.text \
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {} AND c.relname = {}",
            literal(&t.schema),
            literal(&t.name)
        );
        let total_bytes = simple_text(conn, &size_sql)
            .await
            .map_err(|e| read_failure(&e, "reading the table's size", dry_run))?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        tables.push(ContextTable { table, total_bytes });
    }
    Ok((tables, absent))
}

/// What one request runs with.
struct Plan<'a> {
    ops: &'a [Operation],
    dry_run: bool,
    lock_timeout_ms: u64,
    /// The hash the request confirmed (it cascades).
    confirmed: Option<&'a str>,
    set_role: Option<&'a str>,
    max_bytes: i64,
    /// The database owner's name (the session role of an owner run).
    owner: &'a str,
    /// The request holds a privilege op: the members hub's checks apply (spec Part 4).
    privileges: Option<PrivilegeChecks<'a>>,
}

impl Plan<'_> {
    /// The role the statements run as: the acting role, or the database owner.
    fn session_role(&self) -> &str {
        self.set_role.unwrap_or(self.owner)
    }
}

/// The result of one statement that ran.
fn ran(s: &RenderedStatement, notices: Notices) -> StatementResult {
    StatementResult {
        sql: s.sql.clone(),
        op_index: s.op_index,
        executed: true,
        skipped: None,
        notices: notices.kept,
        rewrites_table: s.rewrites_table,
        lock: s.lock,
    }
}

/// The index of the first operation that drops something with dependents.
fn first_drop(ops: &[Operation]) -> Option<usize> {
    ops.iter()
        .position(|op| ddl::dependents_sql(std::slice::from_ref(op)).is_some())
}

/// `LOCK TABLE …` statements of [`ddl::lock_sql`], each with the first operation dropping from
/// that table.
fn locks(ops: &[Operation]) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        for sql in ddl::lock_sql(std::slice::from_ref(op)) {
            if !out.iter().any(|(s, _)| *s == sql) {
                out.push((sql, i));
            }
        }
    }
    debug_assert_eq!(
        out.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(),
        ddl::lock_sql(ops)
    );
    out
}

fn descriptions(objects: &[Dependent]) -> BTreeSet<String> {
    objects.iter().map(|d| d.description.clone()).collect()
}

/// Whether a cascading drop's notices match what it was confirmed to remove (R4): Postgres's own
/// list (`None`: unreadable) must be exactly the statement target's dependents read just before
/// it ran, each of them confirmed or a part of another drop target of the request (drops that
/// reach each other: `others`).
fn cascade_matches(
    reported: Option<&BTreeSet<String>>,
    expected: &BTreeSet<String>,
    confirmed: &BTreeSet<String>,
    others: &BTreeSet<String>,
) -> bool {
    reported.is_some_and(|r| {
        r == expected && r.iter().all(|o| confirmed.contains(o) || others.contains(o))
    })
}

/// Whether the cascading drops of a request removed exactly what it confirmed (R4, at the end):
/// everything Postgres reported, less the parts of the request's own drop targets.
fn cascades_match(
    reported: &BTreeSet<String>,
    parts: &BTreeSet<String>,
    confirmed: &BTreeSet<String>,
) -> bool {
    reported.difference(parts).eq(confirmed.iter())
}

/// The parts of every drop target in `targets` other than operation `op_index`'s.
fn parts_of_others(targets: &[TargetParts], op_index: Option<usize>) -> BTreeSet<String> {
    targets
        .iter()
        .filter(|t| Some(t.op_index) != op_index)
        .flat_map(|t| t.parts.iter().cloned())
        .collect()
}

/// A statement that didn't run.
fn not_run(s: &RenderedStatement, reason: SkipReason) -> StatementResult {
    StatementResult {
        sql: s.sql.clone(),
        op_index: s.op_index,
        executed: false,
        skipped: Some(reason),
        notices: Vec::new(),
        rewrites_table: s.rewrites_table,
        lock: s.lock,
    }
}

/// Steps 4 and 6: the transaction, its statements, and the response. A privilege request
/// (`statements: None`) is rendered in the transaction, after its checks there.
async fn run_in_transaction(
    conn: &mut Connected,
    plan: &Plan<'_>,
    mut ctx: RenderContext,
    statements: Option<Vec<RenderedStatement>>,
) -> Result<SchemaChangeResponse, ApiError> {
    let dry = plan.dry_run;
    let timeout = if dry {
        DRY_RUN_STATEMENT_TIMEOUT_MS
    } else {
        REAL_STATEMENT_TIMEOUT_MS
    };
    let mut setup = vec![
        "BEGIN".to_string(),
        format!("SET LOCAL lock_timeout = {}", plan.lock_timeout_ms),
        format!("SET LOCAL statement_timeout = {timeout}"),
        // The cascade check reads Postgres's `drop cascades to …` notices.
        "SET LOCAL client_min_messages = notice".to_string(),
    ];
    if let Some(role) = plan.set_role {
        setup.push(format!("SET LOCAL ROLE {}", quote_ident(role)));
    }
    for sql in &setup {
        if let Err(e) = simple(conn, sql).await {
            rollback(conn).await;
            return Err(failure(&e, None, dry));
        }
    }
    let out = prepared_body(conn, plan, &mut ctx, statements).await;
    match out {
        Ok(response) if dry => {
            rollback(conn).await;
            Ok(response)
        }
        Ok(mut response) => match simple(conn, "COMMIT").await {
            Ok(_) => {
                response.committed = true;
                Ok(response)
            }
            Err(e) => {
                rollback(conn).await;
                Err(failure(&e, None, false))
            }
        },
        Err(err) => {
            rollback(conn).await;
            Err(err)
        }
    }
}

/// A privilege request's checks and render (in the transaction), then the body.
async fn prepared_body(
    conn: &mut Connected,
    plan: &Plan<'_>,
    ctx: &mut RenderContext,
    statements: Option<Vec<RenderedStatement>>,
) -> Result<SchemaChangeResponse, ApiError> {
    // T5-M2: the request's locks first, so the privilege checks below (the ACL hashes, the
    // owners) read what the statements will run against.
    for (sql, op_index) in locks(plan.ops) {
        simple(conn, &sql)
            .await
            .map_err(|e| failure(&e, Some(op_index), plan.dry_run))?;
    }
    let (statements, prepared) = match (statements, &plan.privileges) {
        (Some(statements), _) => (statements, None),
        (None, checks) => {
            let empty = BTreeSet::new();
            let none = PrivilegeChecks {
                expect_acl: &[],
                key_roles: &empty,
            };
            let checks = checks.as_ref().unwrap_or(&none);
            let owner_run = plan.set_role.is_none();
            let prepared =
                privilege_ops::prepare(conn, plan.ops, checks, owner_run, ctx, plan.dry_run)
                    .await?;
            let statements = ddl::render(plan.ops, ctx).map_err(|e| refused(&e))?;
            (statements, Some(prepared))
        }
    };
    transaction_body(conn, plan, ctx, &statements, prepared.as_ref()).await
}

async fn transaction_body(
    conn: &mut Connected,
    plan: &Plan<'_>,
    ctx: &RenderContext,
    statements: &[RenderedStatement],
    prepared: Option<&Prepared>,
) -> Result<SchemaChangeResponse, ApiError> {
    let dry = plan.dry_run;
    // What a cascade of the request's drops would remove beyond their targets and their parts.
    let dependents = read_dependents(conn, plan.ops, 0, dry).await?;
    if let Some(now) = &dependents {
        match plan.confirmed {
            // User, 2026-10-13: without a confirmed cascade, a drop with dependents is refused,
            // in a dry run as in a real one.
            None if !now.objects.is_empty() => return Err(has_dependents(now, dry)),
            Some(confirmed) if now.hash != confirmed => {
                return Err(dependents_changed(
                    "the objects the drop cascades to changed since the dry run; review them and confirm the new hash",
                    now,
                    dry,
                ));
            }
            _ => {}
        }
    }
    let targets = match &dependents {
        Some(_) => read_target_parts(conn, plan.ops, dry).await?,
        None => Vec::new(),
    };
    let confirmed = dependents
        .as_ref()
        .map(|d| descriptions(&d.objects))
        .unwrap_or_default();
    let skips = if dry {
        ddl::dry_run_plan(statements, ctx, plan.max_bytes)
    } else {
        vec![None; statements.len()]
    };

    let mut results = Vec::with_capacity(statements.len());
    // What Postgres reported the cascading drops removed.
    let mut reported_all: BTreeSet<String> = BTreeSet::new();
    // What the drops that ran removed: their targets' parts and what they cascaded to.
    let mut gone: BTreeSet<String> = BTreeSet::new();
    let mut cascades_complete = true;
    for (s, skip) in statements.iter().zip(skips) {
        let op = &plan.ops[s.op_index];
        let drops = ddl::dependents_sql(std::slice::from_ref(op)).is_some();
        let own = targets.iter().find(|t| t.op_index == s.op_index);
        if let Some(reason) = skip {
            cascades_complete &= !(ctx.cascade && drops);
            results.push(not_run(s, reason));
            continue;
        }
        // A drop whose target an earlier drop of the request removed has nothing left to do.
        if own.is_some_and(|t| gone.contains(&t.target)) {
            results.push(not_run(s, SkipReason::AlreadyDropped));
            continue;
        }
        // Every drop reads its target's dependents just before it runs.
        let expected = match drops {
            true => read_dependents(conn, std::slice::from_ref(op), s.op_index, dry).await?,
            false => None,
        };
        if let (false, Some(exp), Some(now)) = (ctx.cascade, &expected, &dependents)
            && !exp.objects.is_empty()
        {
            // Dependents within the request (parts of its other drop targets): still a
            // cascade to confirm.
            return Err(has_dependents(now, dry));
        }
        // D6 / P26: a statement on an object another role owns runs as that role (an owner
        // run only), `RESET ROLE` right after it.
        let run_as = match &s.run_as {
            Some(role) => {
                Some(privilege_ops::enter(conn, role, plan.session_role(), s.op_index, dry).await?)
            }
            None => None,
        };
        let mut notices = Notices::new();
        let (client, queue) = conn.client_and_notices();
        // The extended protocol: one statement, whatever the caller's expressions hold.
        collecting(client.execute(s.sql.as_str(), &[]), queue, &mut notices)
            .await
            .map_err(|e| failure(&e, Some(s.op_index), dry))?;
        if let Some(run) = run_as {
            privilege_ops::leave(conn, run, plan.session_role(), s.op_index, dry).await?;
        }
        if notices.not_applied {
            return Err(privilege_ops::warned_not_applied(
                conn,
                plan.ops,
                prepared,
                s.op_index,
                dry,
            )
            .await);
        }
        if let (true, Some(exp), Some(now)) = (ctx.cascade, &expected, &dependents) {
            let expected = descriptions(&exp.objects);
            let others = parts_of_others(&targets, Some(s.op_index));
            if !cascade_matches(notices.cascaded.as_ref(), &expected, &confirmed, &others) {
                warn!(
                    op_index = s.op_index,
                    "table API change: a cascading drop removed other objects than confirmed"
                );
                return Err(dependents_changed(
                    "the drop cascaded to other objects than the ones confirmed; nothing was changed",
                    now,
                    dry,
                ));
            }
            gone.extend(expected.iter().cloned());
            reported_all.extend(expected);
        }
        if let Some(own) = own {
            gone.extend(own.parts.iter().cloned());
        }
        results.push(ran(s, notices));
    }
    if let (true, true, Some(now)) = (ctx.cascade, cascades_complete, &dependents)
        && !cascades_match(&reported_all, &parts_of_others(&targets, None), &confirmed)
    {
        warn!("table API change: the cascading drops removed other objects than confirmed");
        return Err(dependents_changed(
            "the drops cascaded to other objects than the ones confirmed; nothing was changed",
            now,
            dry,
        ));
    }
    let every_ran = results
        .iter()
        .all(|r| r.executed || r.skipped == Some(SkipReason::AlreadyDropped));
    // MH-24: what the statements did, checked against what the request asked, whenever every
    // statement ran or was skipped as already dropped (T5-I2); a dry run's ceiling skip left
    // statements unrun, so there's nothing complete to check.
    if let Some(prepared) = prepared
        && every_ran
    {
        privilege_ops::verify(conn, plan.ops, prepared, dry).await?;
    }
    Ok(SchemaChangeResponse {
        dry_run: dry,
        committed: false,
        statements: results,
        dependents,
        would_succeed: (dry && every_ran).then_some(true),
        data_api_exposure: prepared.map(|p| p.exposure.clone()).unwrap_or_default(),
    })
}

/// Step 5: a single `CONCURRENTLY` index operation, outside a transaction.
async fn run_concurrently<M, J>(
    state: &AppState<M, J>,
    session: &mut Session,
    plan: &Plan<'_>,
    ctx: &RenderContext,
    statements: Vec<RenderedStatement>,
    owner: (&BranchRecord, &str, &str),
) -> Result<SchemaChangeResponse, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let conn = &mut session.connected;
    let op = &plan.ops[0];
    let dry = plan.dry_run;
    let timeout = if dry {
        DRY_RUN_STATEMENT_TIMEOUT_MS
    } else {
        CONCURRENT_STATEMENT_TIMEOUT_MS
    };
    // The checks run as the acting role too.
    let mut setup = vec![
        format!("SET lock_timeout = {}", plan.lock_timeout_ms),
        format!("SET statement_timeout = {timeout}"),
    ];
    if let Some(role) = plan.set_role {
        setup.push(format!("SET ROLE {}", quote_ident(role)));
    }
    for sql in &setup {
        simple(conn, sql)
            .await
            .map_err(|e| failure(&e, None, dry))?;
    }
    // A concurrent build's own index is one whose name nothing had before it ran.
    let mut name_was_free = false;
    let mut dependents = None;
    match op {
        Operation::CreateIndex(spec) if dry => check_index(conn, ctx, spec).await?,
        Operation::CreateIndex(spec) => {
            let name = spec.name.as_deref().unwrap_or_default();
            name_was_free = relation_oid(conn, &spec.table.schema, name, false)
                .await?
                .is_none();
        }
        _ => {
            dependents = read_dependents(conn, plan.ops, 0, dry).await?;
            // `DROP INDEX CONCURRENTLY` can't cascade.
            if let Some(now) = &dependents
                && !now.objects.is_empty()
            {
                return Err(has_dependents(now, dry));
            }
        }
    }
    if dry {
        let skips = ddl::dry_run_plan(&statements, ctx, plan.max_bytes);
        let results = statements
            .iter()
            .zip(skips)
            .map(|(s, skip)| StatementResult {
                sql: s.sql.clone(),
                op_index: s.op_index,
                executed: false,
                skipped: skip,
                notices: Vec::new(),
                rewrites_table: s.rewrites_table,
                lock: s.lock,
            })
            .collect();
        return Ok(SchemaChangeResponse {
            dry_run: true,
            committed: false,
            statements: results,
            dependents,
            would_succeed: None,
            data_api_exposure: Vec::new(),
        });
    }
    let mut results = Vec::with_capacity(statements.len());
    for s in &statements {
        let mut notices = Notices::new();
        let (client, queue) = conn.client_and_notices();
        let outcome = collecting(client.execute(s.sql.as_str(), &[]), queue, &mut notices).await;
        if let Err(e) = outcome {
            return Err(match (op, e.as_db_error()) {
                // Only a build that made its own catalog entry left an INVALID index of its
                // own: the name was free before it ran (so any index of that name is new),
                // and the failure is one raised while building, after the entry exists. A
                // failure before it (a lock timeout, the name taken meanwhile, a missing
                // column, a permission) made no entry, and an INVALID index of that name is
                // another session's, which stays.
                (Operation::CreateIndex(spec), Some(db)) if name_was_free && raised_while_building(db) => {
                    let name = spec.name.as_deref().unwrap_or_default();
                    match drop_invalid_index(
                        state,
                        owner,
                        &spec.table,
                        name,
                        plan.lock_timeout_ms,
                    )
                    .await
                    {
                        Some(dropped) => with_fields(
                            ApiError::new(409, ErrorCode::IndexBuildFailed, db.message()),
                            db,
                            Some(s.op_index),
                            false,
                        )
                        .with_extra("invalid_index_dropped", json!(dropped)),
                        None => db_failure(db, Some(s.op_index), false),
                    }
                }
                _ => failure(&e, Some(s.op_index), false),
            });
        }
        results.push(ran(s, notices));
    }
    Ok(SchemaChangeResponse {
        dry_run: false,
        committed: true,
        statements: results,
        dependents: None,
        would_succeed: None,
        data_api_exposure: Vec::new(),
    })
}

/// Whether a concurrent build's failure is one raised while building, after its catalog entry
/// exists: a data error or a constraint violation while scanning or validating (classes 22
/// and 23), a deadlock or cancel in the wait phases (40P01, 57014), running out of resources
/// (53, 54), a system or internal error (58, XX). Never the name taken by another session's
/// entry (42P07, or 23505 on the catalog's unique name index): no entry of its own. Every other
/// failure (a lock timeout taking the table's lock, 55P03; a missing table, column or
/// permission, 42xxx) comes before the entry.
fn raised_while_building(db: &DbError) -> bool {
    let code = db.code().code();
    if code == "23505" && db.constraint() == Some("pg_class_relname_nsp_index") {
        return false;
    }
    matches!(code, "40P01" | "57014")
        || ["22", "23", "53", "54", "58", "XX"]
            .iter()
            .any(|class| code.starts_with(class))
}

/// A concurrent index's dry run (step 5): its table exists (404), its columns exist and its
/// name is free (400, with the SQLSTATE Postgres would answer).
async fn check_index(
    conn: &mut Connected,
    ctx: &RenderContext,
    spec: &IndexSpec,
) -> Result<(), ApiError> {
    let table = &spec.table;
    let name = spec.name.as_deref().unwrap_or_default();
    let Some(model) = ctx.table(table) else {
        return Err(ApiError::not_found(format!(
            "no table {}.{}, or not an ordinary table",
            table.schema, table.name
        ))
        .with_extra("op_index", json!(0))
        .with_extra("dry_run", json!(true)));
    };
    let columns = spec
        .keys
        .iter()
        .filter_map(|k| match k {
            IndexKey::Column { name, .. } => Some(name),
            IndexKey::Expression { .. } => None,
        })
        .chain(spec.include.iter());
    for column in columns {
        if !model.table.columns.iter().any(|c| c.name == *column) {
            return Err(dry_check_failed(
                "42703",
                format!("column \"{column}\" does not exist"),
            ));
        }
    }
    if relation_oid(conn, &table.schema, name, true).await?.is_some() {
        return Err(dry_check_failed(
            "42P07",
            format!("relation \"{name}\" already exists"),
        ));
    }
    Ok(())
}

/// What the concurrent build would fail with, found by the dry run's catalog check.
fn dry_check_failed(sqlstate: &str, message: String) -> ApiError {
    let (status, code) = classify_failure(sqlstate, None);
    ApiError::new(status, code, message.clone())
        .with_extra("op_index", json!(0))
        .with_extra("sqlstate", json!(sqlstate))
        .with_extra("message", json!(message))
        .with_extra("detail", Value::Null)
        .with_extra("hint", Value::Null)
        .with_extra("position", Value::Null)
        .with_extra("dry_run", json!(true))
}

/// After a concurrent build that failed while building, whose name nothing had before it ran: as
/// the owner, drops the INVALID index of that name on the build's table (the build's own).
/// `None` when there is no such index (not a build failure); `Some(dropped)` otherwise.
async fn drop_invalid_index<M, J>(
    state: &AppState<M, J>,
    (branch, owner, database): (&BranchRecord, &str, &str),
    table: &TableRef,
    name: &str,
    lock_timeout_ms: u64,
) -> Option<bool>
where
    M: MetadataPort,
    J: JobPort,
{
    let mut conn = match connect_as_owner(state, branch, owner, database).await {
        Ok(conn) => conn,
        Err(err) => {
            error!(branch_id = %branch.id, "table API change: no owner connection to drop an INVALID index: {}", err.message);
            return Some(false);
        }
    };
    let schema = &table.schema;
    let invalid_sql = format!(
        "SELECT EXISTS (SELECT FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_class t ON t.oid = i.indrelid \
         WHERE n.nspname = {} AND c.relname = {} AND t.relname = {} \
         AND NOT i.indisvalid)::pg_catalog.text",
        literal(schema),
        literal(name),
        literal(&table.name)
    );
    let timeouts = [
        format!("SET lock_timeout = {lock_timeout_ms}"),
        format!("SET statement_timeout = {REAL_STATEMENT_TIMEOUT_MS}"),
    ];
    for sql in &timeouts {
        if let Err(err) = simple(&mut conn, sql).await {
            error!(branch_id = %branch.id, "table API change: before dropping an INVALID index: {err}");
            return Some(false);
        }
    }
    match simple_text(&mut conn, &invalid_sql).await {
        Ok(Some(found)) if found == "true" => {}
        Ok(_) => return None,
        Err(err) => {
            error!(branch_id = %branch.id, "table API change: reading an INVALID index: {err}");
            return Some(false);
        }
    }
    let drop = [format!(
        "DROP INDEX CONCURRENTLY IF EXISTS {}",
        qualified(schema, name)
    )];
    for sql in &drop {
        if let Err(err) = simple(&mut conn, sql).await {
            error!(branch_id = %branch.id, "table API change: dropping an INVALID index: {err}");
            return Some(false);
        }
    }
    Some(true)
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/schema-changes",
    operation_id = "applySchemaChanges",
    tag = "tables",
    summary = "Change tables, columns, constraints, indexes, policies, grants and enums",
    description = "Renders the operations to SQL (names quoted, expressions as given, each exactly one SQL expression) and runs them on one of the branch's Postgres databases as the owner (Admin only) or, with `role`, as that role (the SQL editor's session: your member role, then `SET ROLE`), in one transaction with `lock_timeout` (`lock_timeout_ms`, default 5000) and `statement_timeout` (60 s; 15 s for a dry run). A dry run runs the same statements and always rolls back; it evaluates your expressions (defaults, checks, `USING`), so their non-transactional side effects (e.g. `nextval`) remain. A statement that may rewrite a table over `CHELABASE_TABLE_API_DRY_RUN_MAX_BYTES` (default 256 MiB), and every one after it, is only rendered in a dry run (`skipped`), and then `would_succeed` is null. A table the request names that doesn't exist (and no earlier operation creates) is 404 with its `op_index`. Drops end `RESTRICT` and a request with drops returns `dependents`: what `CASCADE` would also remove beyond the request's own drop targets and their parts, and their `hash`. When something depends on a drop and the request confirms no cascade, the dry run and the real run alike answer 409 `has_dependents` with `dependents`, and change nothing. To cascade, send that hash as `cascade.dependents_hash`: the run (dry or real) locks the drop targets' tables, reads the dependents again, and refuses with 409 `dependents_changed` when they differ or when Postgres reports dropping anything but them and the parts of the request's other drop targets (drops that reach each other, e.g. a parent table and its child, need the hash of no dependents); a drop whose target an earlier drop of the request already removed is `skipped` (`already_dropped`). A `CONCURRENTLY` index operation must be the request's only operation: it runs outside a transaction (`statement_timeout` 900 s; a dry run only checks it), can't cascade, and when its build fails after creating its index, that INVALID index is dropped (another index of the same name never is). Postgres errors keep their fields (`op_index`, `sqlstate`, `message`, `detail`, `hint`, `position`; `dry_run: true` on a dry run). Each request is logged with its operation count, never its SQL.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
    ),
    request_body = SchemaChangeRequest,
    responses(
        (status = 200, description = "The statements (executed or skipped, with their notices, rewrite flag and lock), the dependents of the request's drops, whether the dry run would succeed (`null` when something was skipped) and whether the change was committed", body = SchemaChangeResponse),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, the body isn't well-formed JSON, or the renderer refuses an operation (`op_index`; e.g. an expression that isn't exactly one SQL expression); `schema_change_failed`: Postgres refused a statement; `constraint_violation`: existing rows break a constraint (SQLSTATE 23502, 23514, 22xxx). Postgres's failures carry `op_index`, `sqlstate`, `message`, `detail`, `hint`, `position`", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `permission_denied`: the role may not make this change (SQLSTATE 42501, with `op_index`). Besides the access refusals: `required: \"role:<name>\"` also when the role doesn't exist on the branch or your member role can't hold it; `required: \"connect\"` when your member role may not connect to this Postgres database", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist; or a table the request names or a drop target doesn't exist (`op_index`)", body = ErrorEnvelope),
        (status = 409, description = "`has_dependents`: a drop has dependents and the request confirms no cascade, dry run or not; nothing was changed (`error.dependents`: what a cascade would also drop and its hash); `dependents_changed`: the dependents differ from the confirmed hash, or a cascading drop removed other objects (`error.dependents`: the current list and hash); `index_build_failed`: the concurrent build failed after creating its index, and that INVALID index was dropped (`invalid_index_dropped`); `lock_timeout`: a lock wasn't granted within `lock_timeout_ms`, while reading the tables or running a statement (retry); `conflict`: existing rows break a unique, foreign-key or exclusion constraint, or a deadlock or serialization failure (40P01, 40001; retry); `branch_protected`: the branch is protected and DDL needs Admin; `owner_password_unknown`: the owner's password was set in SQL; `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body isn't the request's shape, `operations` is empty, `lock_timeout_ms` isn't 1 to 60000, `cascade.dependents_hash` isn't 64 lowercase hex characters, `role` is empty, or `role` is a system role", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached, is starting up or is out of connections right now, or the connection broke (`Retry-After`); `member_role_reconcile_failed`: your member role on the branch couldn't be brought up to date (`Retry-After`)", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: a statement ran past `statement_timeout`", body = ErrorEnvelope),
    ),
)]
async fn apply_schema_changes<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    // 1. Everything that needs no compute.
    let request: SchemaChangeRequest = or_answer!(parse_body(&body));
    let lock_timeout_ms = or_answer!(validate(&request));
    // 2. Access.
    let target = Target::Role {
        database: id,
        branch: branch_id,
        pg_database: request.database.as_deref().unwrap_or(""),
        role: request
            .role
            .as_deref()
            .map_or(Requested::Owner, Requested::Role),
    };
    let (authorized, record, branch) = or_answer!(
        load_authorized(
            &state,
            &principal,
            ep::APPLY_SCHEMA_CHANGES.policy.access,
            target,
            branch_id
        )
        .await
        // T5-M4: whether this is an owner run is `Requested::from_name` with the database's
        // real owner (`owner` in any case, or that owner's name), which only the access
        // decision knows here; it answers `required: "owner"` only for a request it resolved
        // so, which is every owner run.
        .map_err(|err| owner_run_needs_admin(err, true))
    );
    let row = or_answer!(pick_pg_database(&state, &branch, request.database.as_deref()).await);
    // Members hub: a privilege request's checks that need the metadata, before any compute.
    let privileged = request.operations.iter().any(privilege_ops::is_privilege_op);
    let key_roles = if privileged {
        or_answer!(
            privilege_ops::refuse_personal_logins(
                &state,
                record.database_id.as_uuid(),
                &request.operations
            )
            .await
        );
        if privilege_ops::needs_key_roles(&request.operations) {
            or_answer!(privilege_ops::key_roles(&state, branch.id).await)
        } else {
            BTreeSet::new()
        }
    } else {
        BTreeSet::new()
    };
    // 3. The session.
    let mut session = or_answer!(
        open_session(
            &state,
            &authorized,
            &principal,
            &record,
            &branch,
            &row.name,
            request.role.as_deref(),
            true,
        )
        .await
    );
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
    let owner = roles::owner_name(&record).to_owned();
    let set_role = session.set_role.clone();
    let plan = Plan {
        ops: &request.operations,
        dry_run: request.dry_run,
        lock_timeout_ms,
        confirmed: request.cascade.as_ref().map(|c| c.dependents_hash.as_str()),
        set_role: set_role.as_deref(),
        max_bytes: state.table_api_dry_run_max_bytes,
        owner: &owner,
        privileges: privileged.then_some(PrivilegeChecks {
            expect_acl: &request.expect_acl,
            key_roles: &key_roles,
        }),
    };
    let outcome = change(&state, &mut session, &plan, (&branch, &owner, &row.name)).await;
    // The hand-over (R5) ends this backend: the session goes first.
    drop(session);
    if let Some(after) = after {
        tokio::spawn(after);
    }
    let (status, committed) = match &outcome {
        Ok(response) => (200, response.committed),
        Err(err) => (err.status, false),
    };
    info!(
        target: AUDIT_TARGET,
        user = %principal.sub,
        database_id = %id,
        branch_id = %branch.id,
        pg_database = %row.name,
        role = %request.role.as_deref().unwrap_or("owner"),
        operations = request.operations.len(),
        dry_run = request.dry_run,
        committed,
        status,
        "schema change"
    );
    if privileged && committed {
        privilege_ops::record_event(
            &state,
            &principal,
            record.project_id.as_uuid(),
            record.database_id.as_uuid(),
            branch.id.as_uuid(),
            &row.name,
            set_role.as_deref(),
            &request.operations,
        )
        .await;
    }
    match outcome {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(err) => err.into_response(),
    }
}

/// Steps 3 (the context read and the re-render) to 6.
async fn change<M, J>(
    state: &AppState<M, J>,
    session: &mut Session,
    plan: &Plan<'_>,
    owner: (&BranchRecord, &str, &str),
) -> Result<SchemaChangeResponse, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let dry = plan.dry_run;
    let conn = &mut session.connected;
    // The context read waits for locks too (`pg_total_relation_size` opens the table): the
    // request's `lock_timeout` and a `statement_timeout` bound it.
    let timeout = if dry {
        DRY_RUN_STATEMENT_TIMEOUT_MS
    } else {
        REAL_STATEMENT_TIMEOUT_MS
    };
    for sql in [
        format!("SET lock_timeout = {}", plan.lock_timeout_ms),
        format!("SET statement_timeout = {timeout}"),
    ] {
        simple(conn, &sql)
            .await
            .map_err(|e| failure(&e, None, dry))?;
    }
    let (tables, absent) = match read_context(conn, plan.ops, dry).await {
        Ok(read) => read,
        Err(err) => {
            // `table_sql` is its own transaction: don't leave it aborted.
            rollback(conn).await;
            return Err(err);
        }
    };
    if let Some((op_index, t)) = ddl::first_missing_table(plan.ops, &absent) {
        return Err(on_dry(
            ApiError::not_found(format!("no table {}.{}", t.schema, t.name))
                .with_extra("op_index", json!(op_index)),
            dry,
        ));
    }
    // MH-31: MAINTAIN by the compute's own version.
    let server_version_num = simple_text(conn, "SHOW server_version_num")
        .await
        .map_err(|e| read_failure(&e, "reading the server version", dry))?
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);
    let ctx = RenderContext {
        tables,
        cascade: plan.confirmed.is_some(),
        // S5: the grantee allow-list's owner is the role the statements run as.
        session_role: plan.session_role().to_string(),
        server_version_num,
        ..Default::default()
    };
    // A privilege request is rendered in the transaction, once its owners and functions are
    // resolved there (S4, P11).
    if plan.privileges.is_some() {
        return run_in_transaction(&mut session.connected, plan, ctx, None).await;
    }
    let statements = ddl::render(plan.ops, &ctx).map_err(|e| refused(&e))?;
    if statements.iter().any(|s| s.concurrently) {
        run_concurrently(state, session, plan, &ctx, statements, owner).await
    } else {
        run_in_transaction(&mut session.connected, plan, ctx, Some(statements)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_types::table_api::CascadeConfirm;

    fn request(ops: Value) -> SchemaChangeRequest {
        serde_json::from_value(json!({ "operations": ops })).unwrap()
    }

    fn comment() -> Value {
        json!({ "set_table_comment": { "table": { "schema": "public", "name": "notes" }, "comment": "c" } })
    }

    #[test]
    fn hashes_are_64_lowercase_hex() {
        assert!(is_hash(&"0a".repeat(32)));
        assert!(!is_hash(&"0A".repeat(32)));
        assert!(!is_hash(&"0".repeat(63)));
        assert!(!is_hash(&"g".repeat(64)));
    }

    #[test]
    fn validation_bounds() {
        assert_eq!(
            validate(&request(json!([comment()]))).unwrap(),
            DEFAULT_LOCK_TIMEOUT_MS
        );
        let mut r = request(json!([comment()]));
        r.lock_timeout_ms = Some(60_000);
        assert_eq!(validate(&r).unwrap(), 60_000);
        r.cascade = Some(CascadeConfirm {
            dependents_hash: "f".repeat(64),
        });
        assert!(validate(&r).is_ok());
        r.role = Some("pg_read_all_data".into());
        assert_eq!(validate(&r).unwrap_err().status, 422);
    }

    // MH-33: an `expect_acl` hash is checked for shape before any compute.
    #[test]
    fn expect_acl_hashes_are_64_lowercase_hex() {
        let mut r: SchemaChangeRequest = serde_json::from_value(json!({
            "operations": [comment()],
            "expect_acl": [{ "object": { "relation": { "schema": "public", "name": "notes" } }, "acl_hash": "0".repeat(64) }],
        }))
        .unwrap();
        assert!(validate(&r).is_ok());
        r.expect_acl[0].acl_hash = "A".repeat(64);
        assert_eq!(validate(&r).unwrap_err().status, 422);
        r.expect_acl[0].acl_hash = String::new();
        assert_eq!(validate(&r).unwrap_err().status, 422);
    }

    // MH-31 / P11: what only the compute decides isn't refused before it: MAINTAIN (the
    // server's version) and a function op (resolved in the transaction); a grantee the
    // allow-list refuses whatever the session is, is.
    #[test]
    fn validation_leaves_the_server_version_and_functions_to_the_compute() {
        let r = request(json!([
            { "grant": { "table": { "schema": "public", "name": "notes" }, "grant": { "privileges": ["maintain"], "grantee": { "role": "r" } } } },
            { "grant_function": { "function": { "oid": 16384, "identity": "public.f(integer)" }, "grantee": { "role": "r" } } },
            { "revoke_function": { "function": { "oid": 16385, "identity": "x" }, "grantee": "public" } },
        ]));
        assert!(validate(&r).is_ok());
        for grantee in [json!({ "role": "authenticator" }), json!({ "role": "cb_m_x" }), json!("public")] {
            let r = request(json!([
                { "grant_schema": { "schema": "public", "grantee": grantee, "privileges": ["usage"] } },
            ]));
            let err = validate(&r).unwrap_err();
            assert_eq!((err.status, err.body()["error"]["op_index"].clone()), (400, json!(0)), "{grantee}");
        }
    }

    // R6 / MH-30: an owner run below Admin is told the route's level, `admin`; other refusals
    // and a role run are left as they are.
    #[test]
    fn an_owner_run_below_admin_requires_admin() {
        let owner = || ApiError::new(403, ErrorCode::Forbidden, "x").with_extra("required", json!("owner"));
        assert_eq!(owner_run_needs_admin(owner(), true).body()["error"]["required"], "admin");
        assert_eq!(owner_run_needs_admin(owner(), false).body()["error"]["required"], "owner");
        let role = ApiError::new(403, ErrorCode::Forbidden, "x").with_extra("required", json!("role:sales"));
        assert_eq!(owner_run_needs_admin(role, true).body()["error"]["required"], "role:sales");
        let other = ApiError::not_found("x");
        assert_eq!(owner_run_needs_admin(other, true).status, 404);
    }

    #[test]
    fn a_concurrent_index_needs_a_name() {
        let r = request(json!([{ "create_index": {
            "table": { "schema": "public", "name": "notes" },
            "keys": [{ "column": { "name": "body" } }],
            "concurrently": true
        } }]));
        let err = validate(&r).unwrap_err();
        assert_eq!((err.status, err.code), (400, ErrorCode::InvalidRequest));
        assert_eq!(err.body()["error"]["op_index"], 0);
    }

    #[test]
    fn the_dry_run_ceiling_setting() {
        assert_eq!(dry_run_max_bytes(None), Ok(268_435_456));
        assert_eq!(dry_run_max_bytes(Some("  ")), Ok(268_435_456));
        assert_eq!(dry_run_max_bytes(Some("1024")), Ok(1024));
        assert_eq!(dry_run_max_bytes(Some("0")), Ok(0));
        assert!(dry_run_max_bytes(Some("-1")).is_err());
        assert!(dry_run_max_bytes(Some("1GB")).is_err());
    }

    #[test]
    fn locks_carry_the_first_drop_on_their_table() {
        let ops: Vec<Operation> = serde_json::from_value(json!([
            comment(),
            { "drop_column": { "table": { "schema": "public", "name": "notes" }, "column": "x" } },
            { "drop_table": { "table": { "schema": "public", "name": "notes" } } },
            { "drop_constraint": { "table": { "schema": "app", "name": "t" }, "name": "k" } },
        ]))
        .unwrap();
        let got = locks(&ops);
        assert_eq!(got.iter().map(|(_, i)| *i).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(first_drop(&ops), Some(1));
    }

    // R4: a cascading drop matches only when Postgres's list is readable, equals the target's
    // dependents and holds nothing unconfirmed.
    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cascade_matching() {
        let confirmed = set(&["view a", "view b"]);
        let none = set(&[]);
        assert!(cascade_matches(
            Some(&set(&["view a"])),
            &set(&["view a"]),
            &confirmed,
            &none
        ));
        assert!(cascade_matches(Some(&set(&[])), &set(&[]), &confirmed, &none));
        assert!(!cascade_matches(None, &set(&[]), &confirmed, &none));
        assert!(!cascade_matches(
            Some(&set(&["view a"])),
            &set(&["view a", "view b"]),
            &confirmed,
            &none
        ));
        assert!(!cascade_matches(
            Some(&set(&["view c"])),
            &set(&["view c"]),
            &confirmed,
            &none
        ));
        // T6 review, Important 2: a part of another drop target of the request is allowed.
        let fk = "constraint child_pid_fkey on table child";
        assert!(cascade_matches(
            Some(&set(&[fk])),
            &set(&[fk]),
            &none,
            &set(&["table child", fk])
        ));
    }

    fn parts(op_index: usize, target: &str, parts: &[&str]) -> TargetParts {
        TargetParts {
            op_index,
            target: target.into(),
            parts: set(parts),
        }
    }

    #[test]
    fn the_end_check_leaves_out_the_targets_parts() {
        let fk = "constraint child_pid_fkey on table child";
        let targets = [
            parts(0, "table parent", &["table parent", "column id of table parent"]),
            parts(1, "table child", &["table child", fk]),
        ];
        assert_eq!(parts_of_others(&targets, Some(1)), set(&["table parent", "column id of table parent"]));
        let all = parts_of_others(&targets, None);
        assert!(cascades_match(&set(&[fk]), &all, &set(&[])));
        assert!(cascades_match(&set(&[fk, "view v"]), &all, &set(&["view v"])));
        // A confirmed object that wasn't dropped, or one dropped beyond the request.
        assert!(!cascades_match(&set(&[fk]), &all, &set(&["view v"])));
        assert!(!cascades_match(&set(&[fk, "view w"]), &all, &set(&[])));
    }

    // The invariant (R4 with drops reaching each other), exhaustively over a small universe:
    // whatever Postgres reports, a request passes the per-drop and the end checks only when
    // nothing outside its targets' parts went unconfirmed and everything confirmed went.
    #[test]
    fn nothing_beyond_the_request_goes_unconfirmed() {
        let universe = ["view a", "view b", "table child", "constraint k on table child"];
        let subsets = |mask: u32| -> BTreeSet<String> {
            universe
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, s)| s.to_string())
                .collect()
        };
        let all_parts = set(&["table child", "constraint k on table child"]);
        for confirmed_mask in 0..16 {
            // The confirmed set never holds a part of a target (the dependents query leaves
            // them out).
            let confirmed: BTreeSet<String> =
                subsets(confirmed_mask).difference(&all_parts).cloned().collect();
            for reported_mask in 0..16 {
                let reported = subsets(reported_mask);
                let per_drop = cascade_matches(Some(&reported), &reported, &confirmed, &all_parts);
                let end = cascades_match(&reported, &all_parts, &confirmed);
                let beyond: BTreeSet<String> = reported.difference(&all_parts).cloned().collect();
                assert_eq!(per_drop && end, beyond == confirmed, "{reported:?} {confirmed:?}");
            }
        }
    }

    #[test]
    fn a_missing_drop_target_is_read_from_the_error() {
        assert_eq!(
            missing_label(
                "invalid input syntax for type integer: \"chelabase: drop target column app.b.x not found\""
            ),
            Some("column app.b.x")
        );
        assert_eq!(
            missing_label("invalid input syntax for type integer: \"x\""),
            None
        );
    }
}
