//! `GET …/pg-databases/{name}/privileges`: one role's privileges in one Postgres database of a
//! branch (members hub, spec Part 4), a schema page at a time, read on the owner's pooled
//! connection with `chelabase-schema`'s privileges read. Project-level (Viewer, MH-32): every
//! grantor's entries, each object's ACL hash (P10) and whether the owner run can act as its
//! owner (D6, P27a).
//!
//! Also the extended-protocol JSON values the privileges reads bind and return (here and in
//! the schema-change route's in-transaction checks).

use std::error::Error;
use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::{BufMut, BytesMut};
use chelabase_query::{Lease, QueryClient, QueryFailure};
use chelabase_routes::orchestrator as ep;
use chelabase_schema::privileges::{PrivilegesPage, parse_privileges, privileges_sql};
use chelabase_types::ApiPath;
use chelabase_types::api_error::{ApiError, ErrorEnvelope};
use serde::Deserialize;
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type, to_sql_checked};
use tracing::error;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::Target;
use crate::branches::unprocessable;
use crate::console::{owner_pool_for, pool_failure};
use crate::sql_editor::run::RunTls;
use crate::sql_editor::{load_authorized, pick_pg_database};
use crate::table_api::{READ_TIMEOUT_SQL, read_failure};
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort, ResetError, roles};

/// `limit` when the query names none, and the most it may name.
const DEFAULT_LIMIT: i32 = 200;
const MAX_LIMIT: i32 = 500;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new().routes(routes!(get_privileges))
}

/// A `json`/`jsonb`/`text` value, as its JSON text.
pub(crate) struct JsonText(pub(crate) String);

impl<'a> FromSql<'a> for JsonText {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let text = if *ty == Type::JSONB {
            match raw.split_first() {
                Some((1, rest)) => rest,
                _ => return Err("unsupported jsonb format version".into()),
            }
        } else {
            raw
        };
        Ok(JsonText(std::str::from_utf8(text)?.to_owned()))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::JSON | Type::JSONB | Type::TEXT)
    }
}

/// A JSON text bound as a `json`/`jsonb` parameter.
#[derive(Debug)]
pub(crate) struct Jsonb(pub(crate) String);

impl ToSql for Jsonb {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        if *ty == Type::JSONB {
            out.put_u8(1);
        }
        out.put_slice(self.0.as_bytes());
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::JSON | Type::JSONB)
    }

    to_sql_checked!();
}

/// Whether the read's `role` (`$1`) is one it lists: `public` (PUBLIC), or a role of the
/// cluster that isn't a member login (`cb_m_*`) or a marked role (a personal login), the same
/// rule as the read's `target` (MH-13, MH-32).
pub(crate) const ROLE_LISTED_SQL: &str = "SELECT $1::text = 'public' OR EXISTS (\
     SELECT FROM pg_catalog.pg_roles r \
     WHERE r.rolname = $1::text \
       AND pg_catalog.lower(r.rolname) NOT LIKE 'cb\\_m\\_%' \
       AND NOT EXISTS (\
         SELECT FROM pg_catalog.pg_db_role_setting s \
         WHERE s.setrole = r.oid AND s.setdatabase = 0 \
           AND EXISTS (SELECT FROM pg_catalog.unnest(s.setconfig) AS c \
                       WHERE pg_catalog.starts_with(c, 'chelabase.managed_role='))))";

/// The query string of the privileges read.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PrivilegesQuery {
    /// The grantee whose entries are read (required); `public` (lower case) reads PUBLIC's.
    pub role: Option<String>,
    /// The schema whose objects are listed; without it, the schema list and the default
    /// privileges.
    pub schema: Option<String>,
    /// The previous page's `next_after`.
    pub after: Option<String>,
    /// Objects (or schemas) per page, 1 to 500 (default 200).
    pub limit: Option<String>,
}

/// A checked privileges read: `$1` to `$4` of [`privileges_sql`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PageParams {
    pub(crate) role: String,
    pub(crate) schema: Option<String>,
    pub(crate) after: Option<String>,
    pub(crate) limit: i32,
}

/// A text value the read binds: non-empty, at most 1024 bytes (a name, or a name with its
/// paging prefix), no NUL.
fn plain(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.contains('\0')
}

/// An object page's `after` (`<class>/<oid>/<name>`, as `next_after` gives it): class 0 or 1,
/// a whole oid, a non-empty name (which may hold `/`).
fn object_after(after: &str) -> bool {
    let mut parts = after.splitn(3, '/');
    let (Some(class), Some(oid), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    matches!(class, "0" | "1")
        && oid.parse::<u32>().is_ok()
        && !oid.starts_with('+')
        && !name.is_empty()
}

/// The query's checks, which need no compute.
pub(crate) fn page_params(query: PrivilegesQuery) -> Result<PageParams, ApiError> {
    let role = query
        .role
        .filter(|r| plain(r))
        .ok_or_else(|| unprocessable("`role` is required: a role name, or `public` for PUBLIC"))?;
    if query.schema.as_deref().is_some_and(|s| !plain(s)) {
        return Err(unprocessable("`schema` must be a schema name"));
    }
    let limit = match query.limit.as_deref() {
        None => DEFAULT_LIMIT,
        Some(l) => l
            .parse::<i32>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| unprocessable("`limit` must be 1 to 500"))?,
    };
    if let Some(after) = query.after.as_deref() {
        let ok = match query.schema {
            Some(_) => plain(after) && object_after(after),
            None => plain(after),
        };
        if !ok {
            return Err(unprocessable(
                "`after` must be the previous page's `next_after`",
            ));
        }
    }
    Ok(PageParams {
        role,
        schema: query.schema,
        after: query.after,
        limit,
    })
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/privileges",
    operation_id = "getPrivileges",
    tag = "tables",
    summary = "One role's privileges in a Postgres database",
    description = "The privileges `role` holds in one Postgres database of a branch (one of the branch's recorded databases), read as the database owner, a page at a time. Without `schema`: the schemas (paged by name) with the role's schema entries, and the default privileges of those schemas. With `schema`: every table, view, materialized view, partitioned or foreign table, sequence, function and procedure of it (paged), with the role's entries on it and on its columns, the hub's \"all rows\" policy for the role, its owner and whether an owner run can act as that owner (`owner_settable`). Every entry carries its grantor and grant option. Each schema, object and default-privileges row carries `acl_hash`, a hash over its whole ACL (every grantee; a relation's covers its columns): send it back as `expect_acl` with a schema change. `role=public` reads PUBLIC's entries. Member logins and personal logins aren't listed.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("name" = String, Path, description = "The Postgres database"),
        PrivilegesQuery,
    ),
    responses(
        (status = 200, description = "One page of the role's privileges; `next_after` names the next page, `null` on the last", body = PrivilegesPage),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: below Viewer on the project; `permission_denied`: the database owner may not read the catalog", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist (only the branch's recorded databases are read), or the role isn't one the read lists", body = ErrorEnvelope),
        (status = 409, description = "`owner_password_unknown`: the owner's password was set in SQL; `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the query string is malformed, `role` is missing or empty, `limit` isn't 1 to 500, or `after` isn't a `next_after`", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached or is out of connections right now (`Retry-After`)", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the read took over 15 s", body = ErrorEnvelope),
    ),
)]
async fn get_privileges<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, name)): ApiPath<(Uuid, Uuid, String)>,
    query: Result<axum::extract::Query<PrivilegesQuery>, axum::extract::rejection::QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    // Grants have no Postgres-database level, so the name doesn't change the decision.
    let target = Target::PgDatabase {
        database: id,
        branch: branch_id,
        pg_database: "",
    };
    let (_, record, branch) = match load_authorized(
        &state,
        &principal,
        ep::GET_PRIVILEGES.policy.access,
        target,
        branch_id,
    )
    .await
    {
        Ok(loaded) => loaded,
        Err(err) => return err.into_response(),
    };
    let Ok(axum::extract::Query(query)) = query else {
        return unprocessable("the query string is malformed").into_response();
    };
    let params = match page_params(query) {
        Ok(params) => params,
        Err(err) => return err.into_response(),
    };
    // MH-31: only a database the branch records, never a template or `postgres`.
    let row = match pick_pg_database(&state, &branch, Some(&name)).await {
        Ok(row) => row,
        Err(err) => return err.into_response(),
    };
    let owner = roles::owner_name(&record).to_owned();
    let (pool, tls) = match owner_pool_for(&state, &branch, &row, &owner).await {
        Ok(pool) => pool,
        Err(err) => return err.into_response(),
    };
    match read(&pool, &tls, &params, principal.request_id.as_str()).await {
        Ok(Some(json)) => match parse_privileges(&json) {
            Ok(page) => (StatusCode::OK, Json(page)).into_response(),
            Err(err) => {
                error!("privileges read: {err}");
                ApiError::internal().into_response()
            }
        },
        Ok(None) => ApiError::not_found("no such role on the branch").into_response(),
        Err(f) => read_failure(f).into_response(),
    }
}

/// The page's JSON, or `None` when the read doesn't list `params.role`.
async fn read(
    pool: &deadpool_postgres::Pool,
    tls: &RunTls,
    params: &PageParams,
    rid: &str,
) -> Result<Option<String>, QueryFailure> {
    // Not marked done on failure: the connection is closed rather than pooled.
    let mut lease = Lease::new(pool.get().await.map_err(|e| pool_failure(e, rid, tls))?);
    let client = lease.client();
    let pg = |e: tokio_postgres::Error| QueryFailure::from_pg(&e, rid);
    client.simple_query(READ_TIMEOUT_SQL).await.map_err(pg)?;
    let listed: bool = client
        .query_one(ROLE_LISTED_SQL, &[&params.role])
        .await
        .map_err(pg)?
        .get(0);
    let json = match listed {
        true => Some(
            client
                .query_one(
                    privileges_sql(),
                    &[
                        &params.role as &(dyn ToSql + Sync),
                        &params.schema,
                        &params.after,
                        &params.limit,
                    ],
                )
                .await
                .map_err(pg)?
                .get::<_, JsonText>(0)
                .0,
        ),
        false => None,
    };
    client
        .simple_query("RESET statement_timeout")
        .await
        .map_err(pg)?;
    lease.done();
    Ok(json)
}

/// EOS-H1 (user ruling 2026-10-23: fail closed, via the worker): asks the worker whether a key
/// create (`change` absent; `starting` is the key's role) or a role's membership change would
/// let a data API role reach a role with the hub's "all rows" policy on the branch. The worker
/// reads every live database as `cloud_admin`. `Ok(())` only on the worker's `clear`: an
/// exposure is 422 `exposes_data_api` with the chain (`what` names the change), and a check
/// that can't complete is 503 (`unavailable` when the worker failed or timed out,
/// `worker_unavailable` when it can't be reached). Never allowed on a failure.
pub(crate) async fn check_all_rows<M, J>(
    state: &AppState<M, J>,
    branch: chelabase_types::BranchId,
    starting: std::collections::BTreeSet<String>,
    change: Option<chelabase_nats::MembershipChange>,
    what: &str,
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    use chelabase_nats::{AllRowsCheckReply, AllRowsCheckRequest};
    use chelabase_types::api_error::ErrorCode;
    let request = AllRowsCheckRequest {
        branch_id: branch,
        starting: starting.into_iter().collect(),
        change,
    };
    let unavailable = || {
        ApiError::new(
            503,
            ErrorCode::Unavailable,
            "the data API exposure check couldn't be completed; nothing was changed, try again",
        )
        .retry_after(crate::console::RETRY_AFTER_SECS)
    };
    match state.js.check_all_rows(request).await {
        Ok(AllRowsCheckReply::Clear) => Ok(()),
        Ok(AllRowsCheckReply::Exposed { chain }) => Err(exposes_all_rows(what, chain)),
        Ok(AllRowsCheckReply::Failed { message }) => {
            tracing::warn!(branch_id = %branch, "all-rows check failed: {message}");
            Err(unavailable())
        }
        Err(ResetError::Timeout) => {
            tracing::warn!(branch_id = %branch, "all-rows check: the worker didn't answer in time");
            Err(unavailable())
        }
        Err(ResetError::Transport(err)) => {
            error!(branch_id = %branch, "all-rows check: {err}");
            Err(ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable"))
        }
    }
}

/// EOS-H1: 422 `exposes_data_api` for a key create or role change that would let a data API
/// key's role (or `anon`, `authenticated`, `authenticator`) reach a role with the hub's "all
/// rows" policy, with the chain from that role.
pub(crate) fn exposes_all_rows(what: &str, chain: Vec<String>) -> ApiError {
    use chelabase_types::api_error::ErrorCode;
    ApiError::new(
        422,
        ErrorCode::ExposesDataApi,
        format!(
            "{what} would let every row of {} reach the data API through {}; nothing was changed",
            chain.last().map(String::as_str).unwrap_or_default(),
            chain.join(" → ")
        ),
    )
    .with_extra("chain", serde_json::json!(chain))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(
        role: Option<&str>,
        schema: Option<&str>,
        after: Option<&str>,
        limit: Option<&str>,
    ) -> PrivilegesQuery {
        PrivilegesQuery {
            role: role.map(Into::into),
            schema: schema.map(Into::into),
            after: after.map(Into::into),
            limit: limit.map(Into::into),
        }
    }

    // MH-32, T4-6: the read's query is checked before any compute; a malformed object-mode
    // `after` never reaches the SQL (where it would be a cast error).
    #[test]
    fn the_query_is_checked_before_the_read() {
        let ok = page_params(query(
            Some("r"),
            Some("s"),
            Some("1/16384/f/g"),
            Some("500"),
        ))
        .unwrap();
        assert_eq!((ok.limit, ok.after.as_deref()), (500, Some("1/16384/f/g")));
        assert_eq!(
            page_params(query(Some("r"), None, None, None))
                .unwrap()
                .limit,
            200
        );
        // Schema mode: `after` is a schema name.
        assert!(page_params(query(Some("r"), None, Some("public"), Some("1"))).is_ok());
        for bad in [
            query(None, None, None, None),
            query(Some(""), None, None, None),
            query(Some("r\0"), None, None, None),
            query(Some("r"), Some(""), None, None),
            query(Some("r"), None, None, Some("0")),
            query(Some("r"), None, None, Some("501")),
            query(Some("r"), None, None, Some("-1")),
            query(Some("r"), None, None, Some("1.5")),
            query(Some("r"), Some("s"), Some(""), None),
            query(Some("r"), Some("s"), Some("2/1/x"), None),
            query(Some("r"), Some("s"), Some("0/1/"), None),
            query(Some("r"), Some("s"), Some("0/4294967296/x"), None),
            query(Some("r"), Some("s"), Some("0/+1/x"), None),
            query(Some("r"), Some("s"), Some("0/-1/x"), None),
            query(Some("r"), Some("s"), Some("0/1"), None),
            query(Some("r"), None, Some("x\0"), None),
            query(Some(&"r".repeat(1025)), None, None, None),
        ] {
            let err = page_params(bad).unwrap_err();
            assert_eq!(err.status, 422);
        }
    }

    #[test]
    fn json_values_cross_the_wire_as_text() {
        let v = JsonText::from_sql(&Type::JSONB, b"\x01{\"a\":1}").unwrap();
        assert_eq!(v.0, "{\"a\":1}");
        assert!(JsonText::from_sql(&Type::JSONB, b"\x02{}").is_err());
        assert_eq!(JsonText::from_sql(&Type::JSON, b"[]").unwrap().0, "[]");
        let mut out = BytesMut::new();
        Jsonb("[1]".into()).to_sql(&Type::JSONB, &mut out).unwrap();
        assert_eq!(&out[..], b"\x01[1]");
        let mut out = BytesMut::new();
        Jsonb("[1]".into()).to_sql(&Type::JSON, &mut out).unwrap();
        assert_eq!(&out[..], b"[1]");
        assert!(!<Jsonb as ToSql>::accepts(&Type::TEXT));
    }
}
