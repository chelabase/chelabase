//! `GET /v1/databases/{id}/branches/{branch_id}/data-api/types`: Studio's types download.
//!
//! The types are what `GET /data/v1/types` gives a secret key of `role` (default
//! `authenticated`, a new key's default) with no database list: rest's internal
//! `/internal/types/{branch}` builds them through the public route's own code, from the role's
//! view (`GET /data/v1/schema`'s, not `/internal/schema`'s, which carries privileges). The
//! orchestrator can't call the public route itself: it holds no key's plaintext and can't mint
//! a data key identity.
//!
//! Who may call it is who may create a secret key for `role` on the branch, by the checks
//! createKey runs on the caller and the role, in its order and with its answers: the
//! `Project(Developer)` access check, `load_branch`, MH-12 (`refuse_personal_login_key`: never a
//! personal login), the key-role check (`authorize_key_role`, T6: the role in the caller's roles
//! or reachable through a granted role's chain, owner-level, LOGIN and reserved roles
//! Admin-only), and the worker's `check_role`. Nothing reaches rest before they pass.
//!
//! Not reused: createKey's all-rows exposure check (EOS-H1 `check_all_rows`, 422
//! `exposes_data_api`, 503 when it can't complete). It guards a key handing out every row;
//! types expose the schema, never rows, so a role createKey refuses for that reason still gets
//! its types here.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chelabase_metadata::{BranchKind, BranchStatus, DataApiState};
use chelabase_routes::orchestrator as ep;
use chelabase_types::ApiPath;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use tracing::{error, warn};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{Target, authorize, authorize_key_role, refuse_personal_login_key};
use crate::branches::{conflict, load_branch, unprocessable};
use crate::console::{
    RestReply, data_api_disabled, database_not_found, pass_on, pick_database, unavailable,
};
use crate::data_keys::{DEFAULT_ROLE, check_role, no_store};
use crate::{AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort};

/// Longest wait for rest's types (one schema read per database with `all_databases`).
const TYPES_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new().routes(routes!(data_api_types))
}

/// The route's query, for the document: [`options`] parses it.
#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[allow(dead_code)]
struct TypesQuery {
    /// The secret key role whose types to generate; `authenticated` (a new key's default) when
    /// absent. You must be allowed to create a secret key for it.
    role: Option<String>,
    /// `ts` (the default): the TypeScript module (`db.ts`). `json`: the runtime `types` map
    /// alone.
    #[param(inline)]
    format: Option<TypesFormat>,
    /// Generate this Postgres database (not the main one) as `Database`. It must be served by
    /// the data API. Not with `all_databases`.
    database: Option<String>,
    /// `true`: every database the data API serves on the branch, as `Databases` and a `types`
    /// map keyed by name (main as `main`).
    all_databases: Option<bool>,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
enum TypesFormat {
    Ts,
    Json,
}

/// The parsed query.
#[derive(Debug, PartialEq, Eq)]
struct Options {
    role: String,
    json: bool,
    database: Option<String>,
    all: bool,
}

/// `role`, `format`, `database` and `all_databases`, each once, with the data API's meanings;
/// anything else, a repeat, an empty value or `database` with `all_databases=true` is refused.
fn options(
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Options, ApiError> {
    let Ok(Query(pairs)) = query else {
        return Err(unprocessable("the query string is malformed"));
    };
    let (mut role, mut format, mut database, mut all) = (None, None, None, None);
    for (name, value) in pairs {
        let slot = match name.as_str() {
            "role" => &mut role,
            "format" => &mut format,
            "database" => &mut database,
            "all_databases" => &mut all,
            _ => {
                return Err(unprocessable(
                    "the types route takes `role`, `format`, `database` and `all_databases` only",
                ));
            }
        };
        if slot.replace(value).is_some() {
            return Err(unprocessable(&format!("`{name}` is given more than once")));
        }
    }
    let json = match format.as_deref() {
        None | Some("ts") => false,
        Some("json") => true,
        Some(_) => return Err(unprocessable("`format` is `ts` or `json`")),
    };
    let all = match all.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(unprocessable("`all_databases` is `true` or `false`")),
    };
    if database.as_deref() == Some("") {
        return Err(unprocessable(
            "`database` must not be empty (omit it for the main database)",
        ));
    }
    if database.is_some() && all {
        return Err(unprocessable(
            "`database` and `all_databases=true` can't be combined",
        ));
    }
    let role = role.unwrap_or_else(|| DEFAULT_ROLE.to_string());
    if role.is_empty() {
        return Err(unprocessable("role must not be empty"));
    }
    Ok(Options {
        role,
        json,
        database,
        all,
    })
}

impl Options {
    /// The query rest's internal route gets: the role, the format, and the database choice.
    fn rest_query(&self) -> String {
        let mut q = format!(
            "role={}&format={}",
            urlencoding::encode(&self.role),
            if self.json { "json" } else { "ts" }
        );
        if let Some(name) = &self.database {
            q.push_str(&format!("&database={}", urlencoding::encode(name)));
        }
        if self.all {
            q.push_str("&all_databases=true");
        }
        q
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/data-api/types",
    operation_id = "getDataApiTypes",
    tag = "data-api",
    summary = "The branch's generated types for a secret key role",
    description = "What `GET /data/v1/types` serves a secret key of `role` on this branch (with no database list), byte for byte: the TypeScript module `db.ts` (`Database` and the runtime `types` map for `@chelabase/client`), or with `format=json` the `types` map alone. Studio's Download types. You may call it when you pass `createKey`'s checks on the caller and the role: Developer access on the branch, not a personal login's role, and a role you may use. Its all-rows exposure check (`exposes_data_api`) does not apply, since types expose the schema and never rows. The `ETag` is the body's hash: send it back as `If-None-Match` for a 304 while the schema is unchanged. Not cached (`Cache-Control: no-store`).",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        TypesQuery,
    ),
    responses(
        (status = 200, description = "The module (`format=ts`, `text/typescript`) or the runtime `types` map (`format=json`)",
            content((String = "text/typescript"), (Object = "application/json")),
            headers(
                ("ETag" = String, description = "A strong tag: the body's SHA-256"),
                ("Cache-Control" = String, description = "`no-store`"),
            )),
        (status = 304, description = "`If-None-Match` names the current `ETag`: the body is unchanged (and not sent)",
            headers(
                ("ETag" = String, description = "The current tag"),
                ("Cache-Control" = String, description = "`no-store`"),
            )),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden` as on `createKey`: `required` is `developer` below Developer, `role:<name>` when the role isn't one you may use (in your roles, or reachable through a granted role) or is a personal login, or `admin` for a LOGIN or owner-level role; `no_access_yet`, `pending_approval`, `member_suspended`, `member_offboarded` for those member states", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the named Postgres database doesn't exist; the data API's `unknown_role` when the role was dropped after the worker's check", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready; `data_api_disabled`: the Postgres database isn't served by the data API; `conflict`: the schema can't be typed (a schema the role reaches has a dot in its name, or with `all_databases` another database is named `main`, the key main always takes)", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an option isn't `role=<name>`, `format=ts|json`, `database=<name>` or `all_databases=true|false`, is repeated or empty, or `database` came with `all_databases=true`; `role_refused`: the worker refused the role (it doesn't exist or may not back a key; the message says why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 502, description = "`unavailable`: the data API answered something unexpected", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the data API or the branch's database can't be reached; retry after `Retry-After`; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope, headers(("Retry-After" = u32, description = "Seconds to wait before retrying"))),
        (status = 504, description = "`timeout`: the worker didn't answer in time; the data API's `timeout` when reading the schema ran past the statement timeout", body = ErrorEnvelope),
    ),
)]
async fn data_api_types<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
    headers: HeaderMap,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    // createKey's checks, in its order: the route's access, the branch, the request, MH-12,
    // then the key role.
    let authorized = match authorize(
        &state,
        &principal,
        ep::DATA_API_TYPES.policy.access,
        Target::Branch {
            database: id,
            branch: branch_id,
        },
    )
    .await
    {
        Ok(authorized) => authorized,
        Err(resp) => return resp.into_response(),
    };
    let Ok(record) = authorized.clone().into_record() else {
        return ApiError::internal().into_response();
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    let options = match options(query) {
        Ok(options) => options,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = refuse_personal_login_key(&state, &record, &options.role).await {
        return err.into_response();
    }
    if let Err(err) =
        authorize_key_role(&state, &authorized, branch.id.as_uuid(), &options.role).await
    {
        return err.into_response();
    }
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    // The database named (or main, which `all_databases` starts from): known and served.
    let rows = match state.metadata.branch_databases(branch.id).await {
        Ok(rows) => rows,
        Err(err) => {
            error!("list branch databases: {err}");
            return ApiError::internal().into_response();
        }
    };
    let Some(row) = pick_database(rows, options.database.as_deref()) else {
        return database_not_found().into_response();
    };
    if row.data_api != DataApiState::On {
        return data_api_disabled(&row.name).into_response();
    }
    // The worker's check, as a new key's: the role may back a key, and `authenticator` can
    // set it.
    if let Err(err) = check_role(&state, branch.id, &options.role).await {
        return err.into_response();
    }

    let url = format!(
        "{}/internal/types/{}?{}",
        state.rest.url.trim_end_matches('/'),
        branch.id.as_uuid(),
        options.rest_query()
    );
    let mut request = state
        .rest
        .http
        .get(url)
        .bearer_auth(&state.rest.token)
        .timeout(TYPES_TIMEOUT);
    if let Some(tag) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        request = request.header(header::IF_NONE_MATCH, tag);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            warn!(branch_id = %branch.id, "data API types: rest unreachable: {err}");
            return unavailable("the data API can't be reached right now; try again")
                .into_response();
        }
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let copy = |name: header::HeaderName| {
        response
            .headers()
            .get(name.as_str())
            .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok())
    };
    let (etag, content_type, retry_after) = (
        copy(header::ETAG),
        copy(header::CONTENT_TYPE),
        copy(header::RETRY_AFTER),
    );
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(err) => {
            warn!(branch_id = %branch.id, "data API types: reading rest's answer: {err}");
            return unavailable("the data API can't be reached right now; try again")
                .into_response();
        }
    };
    let unexpected = || {
        error!(branch_id = %branch.id, %status, "data API types: unexpected answer from rest");
        ApiError::new(
            502,
            ErrorCode::Unavailable,
            "data API returned an unexpected answer",
        )
        .into_response()
    };
    let known_type = |v: &HeaderValue| {
        v.as_bytes() == b"text/typescript; charset=utf-8" || v.as_bytes() == b"application/json"
    };
    match status {
        StatusCode::OK => {
            let (Some(etag), Some(content_type)) = (etag, content_type.filter(known_type)) else {
                return unexpected();
            };
            let mut resp = (status, body).into_response();
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
            resp.headers_mut().insert(header::ETAG, etag);
            no_store(resp)
        }
        StatusCode::NOT_MODIFIED => {
            let Some(etag) = etag else {
                return unexpected();
            };
            let mut resp = StatusCode::NOT_MODIFIED.into_response();
            resp.headers_mut().insert(header::ETAG, etag);
            no_store(resp)
        }
        _ => {
            let code = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"]["code"].as_str().map(str::to_string));
            let answer = match (status, code.as_deref()) {
                // Rest's own branch answers, in ours.
                (StatusCode::NOT_FOUND, Some("unknown_branch")) => {
                    ApiError::not_found("branch not found").into_response()
                }
                (StatusCode::CONFLICT, None) => {
                    conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response()
                }
                // A data API error (the role's own refusals, `unknown_database`, `conflict`,
                // `unavailable`, …) as the console passes them on; a 3xx is no answer.
                _ => {
                    let reply = RestReply {
                        status,
                        retry_after,
                        body: axum::body::Bytes::from(body.to_vec()),
                    };
                    pass_on(&state, branch.id, &row.name, Ok(reply)).await
                }
            };
            no_store(answer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    fn parse(query: &str) -> Result<Options, ApiError> {
        let uri: Uri = format!("http://x/{query}").parse().unwrap();
        options(Query::try_from_uri(&uri))
    }

    #[test]
    fn options_default_to_a_new_keys_role_and_the_module() {
        assert_eq!(
            parse("").unwrap(),
            Options {
                role: "authenticated".into(),
                json: false,
                database: None,
                all: false
            }
        );
        assert_eq!(
            parse("").unwrap().rest_query(),
            "role=authenticated&format=ts"
        );
        assert_eq!(
            parse("?role=a%26b&format=json&database=x%20y")
                .unwrap()
                .rest_query(),
            "role=a%26b&format=json&database=x%20y"
        );
        assert_eq!(
            parse("?all_databases=true").unwrap().rest_query(),
            "role=authenticated&format=ts&all_databases=true"
        );
    }
}
