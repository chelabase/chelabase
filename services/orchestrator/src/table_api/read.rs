//! `GET …/objects`, `GET …/tables/{schema}/{table}` and `GET …/types`: the scoped catalog reads
//! of `chelabase-schema`, run on the owner's pooled connection.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_query::{Lease, QueryClient, QueryFailure};
use chelabase_routes::orchestrator as ep;
use chelabase_schema::SchemaError;
use chelabase_schema::catalog;
use chelabase_schema::model::Relation;
use chelabase_types::access::{Access, Effective, PlatformRole, RoleSet};
use chelabase_types::role_name::member_role_name;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::table_api::{ObjectList, TypeCatalog};
use chelabase_types::ApiPath;
use serde::Deserialize;
use tokio_postgres::SimpleQueryMessage;
use tracing::{error, warn};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::Target;
use crate::console::{owner_pool_for, pool_failure, RETRY_AFTER_SECS};
use crate::sql_editor::run::RunTls;
use crate::branches::unprocessable;
use crate::sql_editor::{load_authorized, pick_pg_database, reconcile_member_role};
use crate::{roles, AppState, AuthPrincipal, JobPort, MetadataPort};

/// The longest one catalog read may run.
pub(crate) const READ_TIMEOUT_SQL: &str = "SET statement_timeout = 15000";

pub(super) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_objects))
        .routes(routes!(get_table))
        .routes(routes!(list_types))
}

/// The query string of the table API's reads.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TableReadQuery {
    /// The Postgres database; the branch's main database when absent.
    pub database: Option<String>,
}

/// What names one catalog read: the database, the branch and the access its route needs.
struct ReadRequest {
    id: Uuid,
    branch_id: Uuid,
    access: Access,
}

type ReadQuery = Result<axum::extract::Query<TableReadQuery>, axum::extract::rejection::QueryRejection>;

/// A read's failure in the management API's shape: a fixed message per code, never Postgres's
/// text (that goes to the log with the request id).
pub(crate) fn read_failure(f: QueryFailure) -> ApiError {
    warn!(
        status = f.status.as_u16(),
        code = f.code,
        sqlstate = f.sqlstate.as_deref(),
        message = f.message.as_str(),
        detail = f.detail.as_deref(),
        hint = f.hint.as_deref(),
        "table API catalog read failed"
    );
    match (f.sqlstate.as_deref(), f.status) {
        (Some("42501"), _) => ApiError::new(
            403,
            ErrorCode::PermissionDenied,
            "the database owner may not read the catalog",
        ),
        (Some("57014"), _) => ApiError::new(504, ErrorCode::Timeout, "the read took too long"),
        (_, StatusCode::SERVICE_UNAVAILABLE) => ApiError::new(
            503,
            ErrorCode::Unavailable,
            "the database can't be reached right now",
        )
        .retry_after(RETRY_AFTER_SECS),
        _ => ApiError::internal(),
    }
}

/// Runs `script` (its own `BEGIN … COMMIT`) as the database owner and returns the one JSON
/// document it selects, or the response to answer with. The access check comes first, then
/// the query string, then the database lookup, so an unknown database is a 404 before any
/// compute is looked up.
async fn read_document<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    req: ReadRequest,
    query: ReadQuery,
    script: String,
) -> Result<String, Box<Response>>
where
    M: MetadataPort,
    J: JobPort,
{
    // Grants have no Postgres-database level, so the name doesn't change the decision.
    let target = Target::PgDatabase {
        database: req.id,
        branch: req.branch_id,
        pg_database: "",
    };
    let (authorized, record, branch) =
        load_authorized(state, principal, req.access, target, req.branch_id)
            .await
            .map_err(|e| Box::new(e.into_response()))?;
    let Ok(axum::extract::Query(query)) = query else {
        return Err(Box::new(unprocessable("the query string is malformed").into_response()));
    };
    let row = pick_pg_database(state, &branch, query.database.as_deref())
        .await
        .map_err(|e| Box::new(e.into_response()))?;
    let owner = roles::owner_name(&record).to_owned();
    let (pool, tls) = owner_pool_for(state, &branch, &row, &owner)
        .await
        .map_err(|e| Box::new(e.into_response()))?;
    let rid = principal.request_id.as_str();
    let member = principal.member.as_ref().map(|m| m.id);
    let subject = connect_subject(authorized.grant.as_ref(), member, &owner);
    // C1 (R16): the subject is the member's own login, which only exists once the member-role
    // reconcile has run on this branch; a missing login asks for that reconcile once, then the
    // check runs again. An existing login without CONNECT is refused with no reconcile.
    let login_member = member.filter(|_| subject != owner);
    let mut reconciled = false;
    loop {
        match (read_once(&pool, &tls, &subject, &script, rid).await, login_member) {
            (Ok(Read::Document(json)), _) => return Ok(json),
            (Ok(Read::NoLogin), Some(member)) if !reconciled => {
                reconcile_member_role(state, branch.id, member)
                    .await
                    .map_err(|e| Box::new(e.into_response()))?;
                reconciled = true;
            }
            (Ok(Read::NoLogin | Read::NoConnect), _) => {
                return Err(Box::new(cannot_connect().into_response()));
            }
            (Err(f), _) => return Err(Box::new(read_failure(f).into_response())),
        }
    }
}

/// What one CONNECT check and read gave.
enum Read {
    /// The catalog document.
    Document(String),
    /// The subject role doesn't exist on the compute.
    NoLogin,
    /// The subject role exists but may not connect.
    NoConnect,
}

/// The CONNECT check as `subject`, then (when it passes) `script`, on one pooled connection.
async fn read_once(
    pool: &deadpool_postgres::Pool,
    tls: &RunTls,
    subject: &str,
    script: &str,
    rid: &str,
) -> Result<Read, QueryFailure> {
    // Not marked done on failure: the connection is closed rather than pooled.
    let mut lease = Lease::new(pool.get().await.map_err(|e| pool_failure(e, rid, tls))?);
    let client = lease.client();
    client
        .simple_query(READ_TIMEOUT_SQL)
        .await
        .map_err(|e| QueryFailure::from_pg(&e, rid))?;
    let row = client
        .query_one(MAY_CONNECT_SQL, &[&subject])
        .await
        .map_err(|e| QueryFailure::from_pg(&e, rid))?;
    let (exists, may_connect): (bool, bool) = (row.get(0), row.get(1));
    if !may_connect {
        // The timeout is reset, so the connection goes back to the pool clean.
        client
            .simple_query("RESET statement_timeout")
            .await
            .map_err(|e| QueryFailure::from_pg(&e, rid))?;
        lease.done();
        return Ok(if exists { Read::NoConnect } else { Read::NoLogin });
    }
    let messages = client
        .simple_query(script)
        .await
        .map_err(|e| QueryFailure::from_pg(&e, rid))?;
    client
        .simple_query("RESET statement_timeout")
        .await
        .map_err(|e| QueryFailure::from_pg(&e, rid))?;
    lease.done();
    messages
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
            _ => None,
        })
        .map(Read::Document)
        .ok_or_else(|| QueryFailure::internal(rid, "the catalog read returned no document"))
}

/// The Postgres role a read's CONNECT check is made as (R10, R12): what Postgres decides when
/// the caller logs in. A non-Admin member (an effective `RoleSet::All` is always Admin)
/// connects as their own login (`cb_m_<id>`, created NOINHERIT, so only PUBLIC's CONNECT or
/// one granted to the login itself counts). Admin and a caller with no grant (an
/// instance-level principal the policy already admitted) are the owner.
fn connect_subject(grant: Option<&Effective>, member: Option<Uuid>, owner: &str) -> String {
    match (grant, member) {
        (Some(g), Some(member)) if g.platform_role != PlatformRole::Admin && !matches!(g.roles, RoleSet::All { .. }) => {
            member_role_name(member)
        }
        _ => owner.to_owned(),
    }
}

/// One query on the owner's connection: whether the role `$1` exists, and whether it holds
/// CONNECT on the current database (a missing role doesn't).
const MAY_CONNECT_SQL: &str = "SELECT r.oid IS NOT NULL, COALESCE(pg_catalog.has_database_privilege(r.rolname, pg_catalog.current_database(), 'CONNECT'), false) FROM (SELECT 1) one LEFT JOIN pg_catalog.pg_roles r ON r.rolname = $1::text";

/// 403 `permission_denied`: the read's refusal when no role of the caller may connect.
fn cannot_connect() -> ApiError {
    ApiError::new(
        403,
        ErrorCode::PermissionDenied,
        "your login may not connect to this database",
    )
}

/// A parse failure of a document Postgres just produced: ours to fix, so a 500.
fn unreadable(err: SchemaError) -> Response {
    error!("table API catalog read: {err}");
    ApiError::internal().into_response()
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/objects",
    operation_id = "listObjects",
    tag = "tables",
    summary = "The database's tables, views and other relations",
    description = "Every relation the table API shows in one Postgres database of a branch (system schemas and Chelabase's own objects left out), with its kind, estimated rows, size and whether the table API edits it (`editable`: ordinary tables only).",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        TableReadQuery,
    ),
    responses(
        (status = 200, description = "The objects, sorted by schema and name", body = ObjectList),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: below Viewer on the project; `permission_denied`: your login may not connect to this database, or the database owner may not read the catalog", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`owner_password_unknown`: the owner's password was set in SQL; `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the query string is malformed", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached or is out of connections right now (`Retry-After`); `member_role_reconcile_failed`: your login on the branch didn't exist yet and couldn't be prepared (`Retry-After`)", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the read took over 15 s", body = ErrorEnvelope),
    ),
)]
async fn list_objects<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    query: ReadQuery,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let json = match read_document(
        &state,
        &principal,
        ReadRequest { id, branch_id, access: ep::LIST_OBJECTS.policy.access },
        query,
        catalog::objects_sql(),
    )
    .await
    {
        Ok(json) => json,
        Err(response) => return *response,
    };
    match catalog::parse_objects(&json) {
        Ok(objects) => (StatusCode::OK, axum::Json(ObjectList { objects })).into_response(),
        Err(err) => unreadable(err),
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}",
    operation_id = "getTable",
    tag = "tables",
    summary = "One table's or other relation's structure",
    description = "Any relation the object list shows, with its `kind`. An ordinary table (`kind: table`) carries its columns, constraints, indexes, policies, privileges, options and comment, as the schema merge reads them; a table that inherits from another is read the same way. A view, materialized view, foreign table or partitioned table is read-only: its columns (in `attnum` order, with `format_type`'s text and `not_null`) and comment, every other list empty; a view's definition and a foreign table's options are never returned. A partition, an index, a sequence, a composite type, a relation in a schema the object list leaves out, or a missing one is 404.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("schema" = String, Path, description = "Schema name"),
        ("table" = String, Path, description = "Table name"),
        TableReadQuery,
    ),
    responses(
        (status = 200, description = "The relation and its kind", body = Relation),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: below Viewer on the project; `permission_denied`: your login may not connect to this database, or the database owner may not read the catalog", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the Postgres database or the relation doesn't exist, or it isn't one the object list shows", body = ErrorEnvelope),
        (status = 409, description = "`owner_password_unknown`: the owner's password was set in SQL; `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the query string is malformed", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached or is out of connections right now (`Retry-After`); `member_role_reconcile_failed`: your login on the branch didn't exist yet and couldn't be prepared (`Retry-After`)", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the read took over 15 s", body = ErrorEnvelope),
    ),
)]
async fn get_table<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, schema, table)): ApiPath<(Uuid, Uuid, String, String)>,
    query: ReadQuery,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let json = match read_document(
        &state,
        &principal,
        ReadRequest { id, branch_id, access: ep::GET_TABLE.policy.access },
        query,
        catalog::relation_sql(&schema, &table),
    )
    .await
    {
        Ok(json) => json,
        Err(response) => return *response,
    };
    match catalog::parse_relation(&json) {
        Ok(Some(relation)) => (StatusCode::OK, axum::Json(relation)).into_response(),
        Ok(None) => ApiError::not_found("no such table or relation").into_response(),
        Err(err) => unreadable(err),
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/types",
    operation_id = "listTypes",
    tag = "tables",
    summary = "The database's types",
    description = "Every type a column can take in one Postgres database of a branch (base, enum, domain, composite, range and multirange types, with their oids), for a type picker and for joining a table's columns by `type_oid`.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        TableReadQuery,
    ),
    responses(
        (status = 200, description = "The types, sorted by schema and name", body = TypeCatalog),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: below Viewer on the project; `permission_denied`: your login may not connect to this database, or the database owner may not read the catalog", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`owner_password_unknown`: the owner's password was set in SQL; `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the query string is malformed", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached or is out of connections right now (`Retry-After`); `member_role_reconcile_failed`: your login on the branch didn't exist yet and couldn't be prepared (`Retry-After`)", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the read took over 15 s", body = ErrorEnvelope),
    ),
)]
async fn list_types<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    query: ReadQuery,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let json = match read_document(
        &state,
        &principal,
        ReadRequest { id, branch_id, access: ep::LIST_TYPES.policy.access },
        query,
        catalog::types_sql(),
    )
    .await
    {
        Ok(json) => json,
        Err(response) => return *response,
    };
    match catalog::parse_types(&json) {
        Ok(types) => (StatusCode::OK, axum::Json(TypeCatalog { types })).into_response(),
        Err(err) => unreadable(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_types::access::{Grant, GrantSources, Narrowing, PgRole};

    fn failure(status: StatusCode, code: &'static str, sqlstate: Option<&str>) -> QueryFailure {
        let mut f = QueryFailure::new(status, code, "Postgres said: secret detail");
        f.detail = Some("row contents".into());
        f.hint = Some("hint text".into());
        f.sqlstate = sqlstate.map(Into::into);
        f
    }

    fn grant(platform_role: PlatformRole, roles: RoleSet, db: Option<Narrowing>) -> Effective {
        GrantSources::new(Some(Grant { platform_role, roles }), Vec::new(), db).effective().unwrap()
    }

    // R10, R12: which role a read's CONNECT check is made as.
    #[test]
    fn connect_subject_follows_the_effective_grant() {
        let m = Uuid::from_u128(7);
        let a = || RoleSet::Only([PgRole::Named("a".into())].into());
        let only = grant(PlatformRole::Viewer, a(), None);
        assert_eq!(connect_subject(None, None, "own"), "own");
        assert_eq!(connect_subject(None, Some(m), "own"), "own");
        assert_eq!(connect_subject(Some(&grant(PlatformRole::Admin, RoleSet::all(), None)), Some(m), "own"), "own");
        let listed = Narrowing { platform_role: None, roles: Some([PgRole::Named("a".into())].into()) };
        let admin = grant(PlatformRole::Admin, RoleSet::all(), Some(listed));
        assert!(matches!(admin.roles, RoleSet::Only(_)));
        assert_eq!(connect_subject(Some(&admin), Some(m), "own"), "own");
        assert_eq!(connect_subject(Some(&only), Some(m), "own"), member_role_name(m));
        // An Admin narrowed below Admin connects as their own login.
        let narrowed = Narrowing { platform_role: Some(PlatformRole::Developer), roles: None };
        assert_eq!(connect_subject(Some(&grant(PlatformRole::Admin, RoleSet::all(), Some(narrowed))), Some(m), "own"), member_role_name(m));
        assert_eq!(connect_subject(Some(&grant(PlatformRole::Viewer, RoleSet::Only(Default::default()), None)), Some(m), "own"), member_role_name(m));
    }

    #[test]
    fn cannot_connect_is_a_typed_403() {
        let e = cannot_connect();
        assert_eq!((e.status, e.code), (403, ErrorCode::PermissionDenied));
        assert!(e.body().to_string().contains("your login may not connect to this database"));
    }

    // T5 review, Important: a read's failure answers in the management API's own shape, with a
    // known `ErrorCode` and nothing of Postgres's text.
    #[test]
    fn read_failures_are_management_api_errors() {
        for (f, status, code) in [
            (failure(StatusCode::FORBIDDEN, "permission_denied", Some("42501")), 403, ErrorCode::PermissionDenied),
            (failure(StatusCode::GATEWAY_TIMEOUT, "timeout", Some("57014")), 504, ErrorCode::Timeout),
            (failure(StatusCode::SERVICE_UNAVAILABLE, "unavailable", None), 503, ErrorCode::Unavailable),
            (failure(StatusCode::INTERNAL_SERVER_ERROR, "internal", Some("XX000")), 500, ErrorCode::Internal),
            (failure(StatusCode::CONFLICT, "retry", Some("40001")), 500, ErrorCode::Internal),
        ] {
            let e = read_failure(f);
            assert_eq!((e.status, e.code), (status, code));
            let body = e.body().to_string();
            for leak in ["secret detail", "row contents", "hint text", "sqlstate", "XX000"] {
                assert!(!body.contains(leak), "{leak} in {body}");
            }
        }
    }
}
