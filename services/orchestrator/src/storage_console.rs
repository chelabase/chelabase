//! Studio's storage on a branch (m9): the object API under
//! `/v1/databases/{id}/branches/{branch_id}/storage/…`, proxied to the storage service's
//! console listener ([`StorageInternal`]) as an Act-as role.
//!
//! Each request, in order, before storage is called:
//! 1. `?role=` names the role to act as and `?claims=` the JWT claims to run with (a JSON
//!    object, `{}` by default). No role, `owner` or the owner's name is **owner mode** (m9,
//!    decided by the human): Admin on the database only (R7), no claims, and storage runs it as
//!    its own login (`BYPASSRLS`: past every policy, still held to the write-once and ticket
//!    triggers); objects written so have no owner (`owner` NULL);
//! 2. the member's access ([`authorize`] with the route's own `ActAs` level from
//!    `chelabase_routes`): reads, listing and resolve need the role held at Viewer, every write, a signed
//!    URL and a resumable upload's steps at Developer; an owner-level, LOGIN or reserved role
//!    is Admin-only, a personal login nobody's (the SQL console's rules);
//! 3. the branch is the database's, not a backup, ready; the Postgres database (`?database=`,
//!    main by default) is served by the data API;
//! 4. the worker's role check (it exists and isn't owner-level; remembered five minutes).
//!
//! Then the request goes to storage with the caller named in `x-chelabase-console` (the branch,
//! the database, the role, the claims, the member as the key) behind storage's bearer. Only a
//! few headers pass each way, and only `upsert` and `download` of the query. Bodies stream both
//! ways with no cap here (storage keeps its own limits).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use chelabase_metadata::{BranchKind, BranchStatus, DataApiState};
use chelabase_routes::{Endpoint, orchestrator as ep};
use chelabase_types::access::Requested;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::{ApiPath, BranchId};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::{error, info, warn};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{Target, authorize};
use crate::branches::{conflict, load_branch, unprocessable};
use crate::console::{
    check_role, data_api_disabled, database_not_found, pick_database, unavailable,
};
use crate::{AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, roles};

/// Who a request runs as, for storage's console listener (`storage::internal::CONSOLE_HEADER`).
pub const CONSOLE_HEADER: &str = "x-chelabase-console";

/// The largest `?claims=`: 4 KiB of JSON.
pub const MAX_CLAIMS_BYTES: usize = 4096;

/// How long storage may stay silent while answering (an upload's answer comes once its whole
/// body is in, as at the gateway).
const READ_TIMEOUT: Duration = Duration::from_secs(3600);

/// Where the storage service's console listener is, and the bearer it takes
/// (`CHELABASE_STORAGE_INTERNAL_URL`, `CHELABASE_STORAGE_INTERNAL_TOKEN`).
#[derive(Clone)]
pub struct StorageInternal {
    /// `http://host:port`, no path.
    pub url: String,
    pub token: String,
    pub http: chelabase_http::Client,
}

impl StorageInternal {
    /// A client that never follows a redirect (#26) and has no total timeout (bodies stream).
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        // `http://host:port` and nothing more: a path, query or fragment would change every
        // console route after it was authorized ([`storage_url`] refuses those calls).
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| format!("CHELABASE_STORAGE_INTERNAL_URL {url:?}: {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none_or(str::is_empty)
            || !matches!(parsed.path(), "" | "/")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(format!(
                "CHELABASE_STORAGE_INTERNAL_URL {url:?}: must be http(s)://host[:port], with no path, query or fragment"
            ));
        }
        let http = chelabase_http::client_builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| format!("the storage client: {e}"))?;
        Ok(StorageInternal {
            url: url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            http,
        })
    }
}

/// The request headers passed to storage.
const REQUEST_HEADERS: [HeaderName; 6] = [
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::RANGE,
    header::IF_NONE_MATCH,
    header::CACHE_CONTROL,
    HeaderName::from_static("x-upsert"),
];

/// The response headers passed back.
const RESPONSE_HEADERS: [HeaderName; 11] = [
    header::CONTENT_TYPE,
    header::CONTENT_LENGTH,
    header::CONTENT_RANGE,
    header::ACCEPT_RANGES,
    header::ETAG,
    header::LAST_MODIFIED,
    header::CACHE_CONTROL,
    header::CONTENT_DISPOSITION,
    header::CONTENT_SECURITY_POLICY,
    header::X_CONTENT_TYPE_OPTIONS,
    header::RETRY_AFTER,
];

/// The query every storage route takes.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StorageQuery {
    /// The role to act as: row level security on `storage.buckets` and `storage.objects`
    /// decides what it sees and may do. Absent, `owner` or the owner's name: owner mode, for an
    /// Admin of the database only, which sees and writes past every policy (write-once buckets
    /// still hold) and writes objects with no owner (`owner` null).
    pub role: Option<String>,
    /// The JWT claims to run with: a JSON object (at most 4 KiB), `{}` when absent. Acting as
    /// `authenticated` with `{"sub": "<user id>"}` shows what that app user sees.
    pub claims: Option<String>,
    /// The Postgres database; the branch's main database when absent. It must be served by the
    /// data API.
    pub database: Option<String>,
    /// Upload: replace an existing object.
    pub upsert: Option<bool>,
    /// Download as an attachment; the value is the file name (empty: the object's own).
    pub download: Option<String>,
}

/// The path ids every storage route has.
#[derive(Deserialize)]
struct Ids {
    id: Uuid,
    branch_id: Uuid,
}

/// The claims `?claims=` names: a JSON object of at most [`MAX_CLAIMS_BYTES`].
pub fn parse_claims(text: Option<&str>) -> Result<Map<String, Value>, ApiError> {
    let Some(text) = text else {
        return Ok(Map::new());
    };
    if text.len() > MAX_CLAIMS_BYTES {
        return Err(unprocessable("claims is at most 4096 bytes of JSON"));
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(unprocessable("claims must be a JSON object")),
    }
}

/// The storage path a console path names: the part after `…/storage`, under `/storage/v1`, as
/// sent (still percent-encoded).
pub fn storage_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/v1/databases/")?;
    let mut parts = rest.splitn(4, '/');
    let (_, branches, _, tail) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if branches != "branches" {
        return None;
    }
    let tail = tail.strip_prefix("storage/")?;
    Some(format!("/storage/v1/{tail}"))
}

/// Whether a path has a segment a URL parser removes or rewrites: `.` or `..`, raw or
/// percent-encoded in any case (`%2e`, `.%2E`, …), or a `\` (a separator to WHATWG URL
/// parsers). Such a path names one route here and another once parsed, so it's refused before
/// the access check (B1, m9's audit).
///
/// The invariant (tested exhaustively in `tests/storage_console.rs` and below): a console path
/// is either refused before storage is called, or storage receives exactly the route the
/// orchestrator matched and authorized, `/storage/v1` + the tail as sent.
pub fn has_dot_segment(path: &str) -> bool {
    path.contains('\\')
        || path.split('/').any(|segment| {
            let decoded = segment.to_ascii_lowercase().replace("%2e", ".");
            decoded == "." || decoded == ".."
        })
}

/// 400, as storage answers a name with a dot segment.
fn dot_segment() -> ApiError {
    ApiError::invalid_request(
        400,
        "a storage path has no `.` or `..` segment (raw or percent-encoded) and no `\\`",
    )
}

/// The URL storage is called at, or `None` when parsing it would change the path (defence in
/// depth behind [`has_dot_segment`]: storage must get the route that was authorized).
pub fn storage_url(base: &str, path: &str, query: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(&format!("{base}{path}?{query}")).ok()?;
    (url.path() == path).then_some(url)
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

/// Every step of a storage route (see the module docs).
#[allow(clippy::too_many_arguments)]
async fn proxy<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    endpoint: &Endpoint,
    ids: Ids,
    query: Result<Query<StorageQuery>, axum::extract::rejection::QueryRejection>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let Ok(Query(query)) = query else {
        return unprocessable("the query string is malformed").into_response();
    };
    if query.role.as_deref() == Some("") {
        return unprocessable("role must not be empty").into_response();
    }
    // The route as sent, checked before anything else: a dot segment would make storage run
    // another route than the one authorized below.
    let Some(path) = storage_path(uri.path()) else {
        error!(
            path = uri.path(),
            "storage console: a route outside …/storage"
        );
        return ApiError::internal().into_response();
    };
    if has_dot_segment(&path) {
        return dot_segment().into_response();
    }
    let claims = or_answer!(parse_claims(query.claims.as_deref()));
    // No role (or `owner`, or the owner's name) is owner mode: acting as the owner needs Admin
    // on the database (R7), decided here like any act-as.
    let target = Target::Role {
        database: ids.id,
        branch: ids.branch_id,
        pg_database: query.database.as_deref().unwrap_or(""),
        role: query
            .role
            .as_deref()
            .map_or(Requested::Owner, Requested::Role),
    };
    let authorized = or_answer!(authorize(state, principal, endpoint.policy.access, target).await);
    let Some(member_id) = principal.member.as_ref().map(|m| m.id) else {
        error!("storage console: an authorized caller without a member row");
        return ApiError::internal().into_response();
    };
    let record = or_answer!(authorized.into_record());
    let branch = or_answer!(load_branch(state, &record, ids.branch_id).await);
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    // Owner mode (m9, the human's option 3): storage runs it as its own login. Claims mean
    // nothing there (no role session), so they're refused rather than dropped.
    let role = query
        .role
        .as_deref()
        .filter(|r| Requested::from_name(r, roles::owner_name(&record)) != Requested::Owner);
    if role.is_none() && query.claims.is_some() {
        return unprocessable("claims apply to a role; owner mode takes none").into_response();
    }
    let rows = or_answer!(
        state
            .metadata
            .branch_databases(branch.id)
            .await
            .map_err(|err| {
                error!("list branch databases: {err}");
                ApiError::internal()
            })
    );
    let Some(row) = pick_database(rows, query.database.as_deref()) else {
        return database_not_found().into_response();
    };
    if row.data_api != DataApiState::On {
        return data_api_disabled(&row.name).into_response();
    }
    if let Some(role) = role {
        or_answer!(check_role(state, branch.id, role).await);
    }

    let response = forward(
        &state.storage,
        &Forward {
            branch: branch.id,
            database: &row.name,
            role,
            claims,
            member_id,
            request_id: &principal.request_id,
        },
        &method,
        &path,
        &query,
        &headers,
        body,
    )
    .await;
    if !method.is_safe() {
        info!(
            request_id = %principal.request_id,
            user = %principal.sub,
            branch = %branch.id,
            database = %row.name,
            role = role.unwrap_or("owner mode"),
            route = endpoint.name,
            status = response.status().as_u16(),
            "console storage write"
        );
    }
    response
}

/// Tells every storage replica to drop what it cached (m9's NATS revocation). Best effort: a
/// lost event is covered by storage's own cache lifetimes (5 s for a key's liveness, 60 s for a
/// branch's connection info), so a failure is only logged.
pub(crate) async fn tell_storage<M, J>(state: &AppState<M, J>, event: chelabase_nats::StorageEvent)
where
    M: MetadataPort,
    J: JobPort,
{
    if let Err(err) = state.js.publish_storage_event(&event).await {
        warn!(
            ?event,
            "publish the storage event (storage's cache lifetimes cover it): {err}"
        );
    }
}

/// Who a request runs as on storage.
struct Forward<'a> {
    branch: BranchId,
    database: &'a str,
    /// `None`: owner mode.
    role: Option<&'a str>,
    claims: Map<String, Value>,
    member_id: Uuid,
    request_id: &'a str,
}

/// Sends the request to storage's console listener and streams its answer back.
#[allow(clippy::too_many_arguments)]
async fn forward(
    storage: &StorageInternal,
    who: &Forward<'_>,
    method: &Method,
    path: &str,
    query: &StorageQuery,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let mut caller = json!({
        "branch_id": who.branch.as_uuid(),
        "database": who.database,
        "member_id": who.member_id,
        "request_id": who.request_id,
    });
    match who.role {
        Some(role) => {
            caller["role"] = json!(role);
            caller["claims"] = json!(who.claims);
        }
        None => caller["owner"] = json!(true),
    }
    let caller = caller.to_string();
    let mut pairs = vec![("database", who.database.to_owned())];
    if let Some(upsert) = query.upsert {
        pairs.push(("upsert", upsert.to_string()));
    }
    if let Some(download) = &query.download {
        pairs.push(("download", download.clone()));
    }
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let Ok(method) = reqwest::Method::from_bytes(method.as_str().as_bytes()) else {
        error!(method = %method, "storage console: the method can't be forwarded");
        return ApiError::internal().into_response();
    };
    let Some(url) = storage_url(&storage.url, path, &query) else {
        // Unreachable once `has_dot_segment` passed and the base URL was checked at startup.
        error!(path, "storage console: the parsed URL changes the route");
        return ApiError::internal().into_response();
    };
    let mut request = storage
        .http
        .request(method.clone(), url)
        .bearer_auth(&storage.token)
        .header(CONSOLE_HEADER, caller);
    let with_body = method != reqwest::Method::GET && method != reqwest::Method::HEAD;
    for name in &REQUEST_HEADERS {
        // A GET or HEAD sends no body on: no length of one either.
        if !with_body && name == header::CONTENT_LENGTH {
            continue;
        }
        for value in headers.get_all(name) {
            request = request.header(name.as_str(), value.as_bytes());
        }
    }
    if with_body {
        request = request.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let reply = match request.send().await {
        Ok(reply) => reply,
        Err(err) => {
            warn!(branch_id = %who.branch, "storage: {err}");
            return unavailable("object storage can't be reached right now; try again")
                .into_response();
        }
    };
    let status = StatusCode::from_u16(reply.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    // Never a redirect (the client follows none): no answer to pass on.
    if status.is_redirection() || status.is_informational() {
        warn!(branch_id = %who.branch, %status, "storage answered with no result");
        return unavailable("object storage can't be reached right now; try again").into_response();
    }
    // Storage's console listener answers 401 only for a bearer that isn't ours: Studio signs
    // the user out on a 401, so it's a 502 here.
    if status == StatusCode::UNAUTHORIZED {
        error!(branch_id = %who.branch, "storage refused the orchestrator's internal bearer");
        return ApiError::new(
            502,
            ErrorCode::Unavailable,
            "object storage refused the orchestrator's credential; this is a server misconfiguration, not your sign-in",
        )
        .into_response();
    }
    let mut response = Response::builder().status(status);
    for name in &RESPONSE_HEADERS {
        for value in reply.headers().get_all(name.as_str()) {
            response = response.header(name, value.as_bytes());
        }
    }
    response
        .body(Body::from_stream(reply.bytes_stream()))
        .unwrap_or_else(|err| {
            error!("storage console: building the forwarded response: {err}");
            ApiError::internal().into_response()
        })
}

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_buckets, create_bucket))
        .routes(routes!(update_bucket, delete_bucket))
        .routes(routes!(list_objects))
        .routes(routes!(
            download_object,
            upload_object,
            put_object,
            delete_object
        ))
        .routes(routes!(delete_objects))
        .routes(routes!(copy_object))
        .routes(routes!(move_object))
        .routes(routes!(sign_object))
        .routes(routes!(resolve_objects))
        .routes(routes!(create_upload))
        .routes(routes!(upload_part))
        .routes(routes!(list_parts, abort_upload))
        .routes(routes!(complete_upload))
}

/// One storage route: its OpenAPI operation and its handler.
macro_rules! storage_route {
    (
        $fn:ident, $method:ident, $path:literal, $op:literal, $endpoint:expr,
        summary = $summary:literal,
        description = $description:literal,
        params($($param:tt)*),
        $(body($($body:tt)*),)?
        ok = ($ok:literal, $ok_description:literal $(, content_type = $ct:literal)?)
    ) => {
        #[utoipa::path(
            $method,
            path = $path,
            operation_id = $op,
            tag = "storage",
            summary = $summary,
            description = $description,
            params(
                ("id" = Uuid, Path, description = "Database id"),
                ("branch_id" = Uuid, Path, description = "Branch id"),
                $($param)*
                StorageQuery,
            ),
            $(request_body($($body)*),)?
            responses(
                (status = $ok, description = $ok_description, $(content_type = $ct,)? body = Value),
                (status = 400, description = "`invalid_request`: a path id isn't a valid UUID; storage's `invalid_request`, `invalid_value` or `reserved_bucket_name`", body = ErrorEnvelope),
                (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
                (status = 403, description = "`forbidden`: the role isn't held at the level the route needs (Viewer or Developer, as its description says; `required`), or acting as it (or owner mode, `required: owner`) needs Admin; `role_refused`: the worker's check refused the role (it doesn't exist or is owner-level); storage's `permission_denied` (row level security refused it), `role_not_allowed` or `database_not_allowed`", body = ErrorEnvelope),
                (status = 404, description = "`not_found`: the database (or another tenant's, or one you have no access to), the branch or the Postgres database doesn't exist; storage's `bucket_not_found`, `object_not_found` or `upload_not_found`", body = ErrorEnvelope),
                (status = 409, description = "`data_api_disabled`: the Postgres database isn't served by the data API; `backup_branch`; `branch_not_ready`; storage's `conflict` or `write_once`", body = ErrorEnvelope),
                (status = 413, description = "storage's `too_large`: over the bucket's file size limit", body = ErrorEnvelope),
                (status = 415, description = "storage's `mime_not_allowed`", body = ErrorEnvelope),
                (status = 422, description = "`invalid_request`: `role` is empty, `claims` is given in owner mode, `claims` isn't a JSON object of at most 4096 bytes, or the query string is malformed", body = ErrorEnvelope),
                (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
                (status = 502, description = "`unavailable`: storage refused the orchestrator's credential (a server misconfiguration)", body = ErrorEnvelope),
                (status = 503, description = "`unavailable`: object storage can't be reached right now (`Retry-After`); `worker_unavailable`; storage's `storage_unavailable` or `busy`", body = ErrorEnvelope),
                (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
            ),
        )]
        #[allow(clippy::too_many_arguments)]
        async fn $fn<M, J>(
            State(state): State<Arc<AppState<M, J>>>,
            principal: AuthPrincipal,
            ApiPath(ids): ApiPath<Ids>,
            query: Result<Query<StorageQuery>, axum::extract::rejection::QueryRejection>,
            method: Method,
            uri: Uri,
            headers: HeaderMap,
            body: Body,
        ) -> Response
        where
            M: MetadataPort,
            J: JobPort,
        {
            proxy(&state, &principal, &$endpoint, ids, query, method, uri, headers, body).await
        }
    };
}

storage_route!(
    list_buckets,
    get,
    "/v1/databases/{id}/branches/{branch_id}/storage/bucket",
    "storageListBuckets",
    ep::STORAGE_LIST_BUCKETS,
    summary = "List the buckets a role sees",
    description = "The buckets the role's policies show, by name (storage's `GET /storage/v1/bucket`). Needs the role held at Viewer.",
    params(),
    ok = (200, "The buckets")
);
storage_route!(
    create_bucket,
    post,
    "/v1/databases/{id}/branches/{branch_id}/storage/bucket",
    "storageCreateBucket",
    ep::STORAGE_CREATE_BUCKET,
    summary = "Create a bucket as a role",
    description = "Storage's `POST /storage/v1/bucket` as the role: an insert policy on `storage.buckets` decides. Needs the role held at Developer.",
    params(),
    body(content = Value, content_type = "application/json"),
    ok = (201, "Created")
);
storage_route!(
    update_bucket,
    patch,
    "/v1/databases/{id}/branches/{branch_id}/storage/bucket/{name}",
    "storageUpdateBucket",
    ep::STORAGE_UPDATE_BUCKET,
    summary = "Update a bucket as a role",
    description = "Storage's `PATCH /storage/v1/bucket/{name}` as the role. Needs the role held at Developer.",
    params(("name" = String, Path, description = "Bucket name"),),
    body(content = Value, content_type = "application/json"),
    ok = (200, "Updated")
);
storage_route!(
    delete_bucket,
    delete,
    "/v1/databases/{id}/branches/{branch_id}/storage/bucket/{name}",
    "storageDeleteBucket",
    ep::STORAGE_DELETE_BUCKET,
    summary = "Delete an empty bucket as a role",
    description = "Storage's `DELETE /storage/v1/bucket/{name}` as the role. Needs the role held at Developer.",
    params(("name" = String, Path, description = "Bucket name"),),
    ok = (200, "Deleted")
);
storage_route!(
    list_objects,
    post,
    "/v1/databases/{id}/branches/{branch_id}/storage/object/list/{bucket}",
    "storageListObjects",
    ep::STORAGE_LIST_OBJECTS,
    summary = "List objects as a role",
    description = "Storage's `POST /storage/v1/object/list/{bucket}` as the role: only the objects its policies show. Needs the role held at Viewer.",
    params(("bucket" = String, Path, description = "Bucket name"),),
    body(content = Value, content_type = "application/json"),
    ok = (200, "The objects and folders under the prefix")
);
storage_route!(
    download_object,
    get,
    "/v1/databases/{id}/branches/{branch_id}/storage/object/{bucket}/{*name}",
    "storageDownloadObject",
    ep::STORAGE_DOWNLOAD_OBJECT,
    summary = "Download an object as a role",
    description = "Storage's download as the role, streamed (`Range` and `If-None-Match` pass through). Needs the role held at Viewer.",
    params(
        ("bucket" = String, Path, description = "Bucket name"),
        (
            "name" = String,
            Path,
            description = "Object name (may contain `/`)"
        ),
    ),
    ok = (
        200,
        "The object's bytes",
        content_type = "application/octet-stream"
    )
);
storage_route!(upload_object, post, "/v1/databases/{id}/branches/{branch_id}/storage/object/{bucket}/{*name}", "storageUploadObject", ep::STORAGE_UPLOAD_OBJECT,
    summary = "Upload an object as a role",
    description = "Storage's streamed upload as the role, with no body cap here (the bucket's limits apply). `x-upsert: true` or `?upsert=true` replaces an existing object. Needs the role held at Developer.",
    params(("bucket" = String, Path, description = "Bucket name"), ("name" = String, Path, description = "Object name (may contain `/`)"),),
    body(content = Vec<u8>, content_type = "application/octet-stream"),
    ok = (200, "Stored: `bucket`, `name`, `version_id`, `size`, `etag`"));
storage_route!(put_object, put, "/v1/databases/{id}/branches/{branch_id}/storage/object/{bucket}/{*name}", "storagePutObject", ep::STORAGE_PUT_OBJECT,
    summary = "Upload an object as a role (PUT)",
    description = "As the POST upload. Needs the role held at Developer.",
    params(("bucket" = String, Path, description = "Bucket name"), ("name" = String, Path, description = "Object name (may contain `/`)"),),
    body(content = Vec<u8>, content_type = "application/octet-stream"),
    ok = (200, "Stored: `bucket`, `name`, `version_id`, `size`, `etag`"));
storage_route!(
    delete_object,
    delete,
    "/v1/databases/{id}/branches/{branch_id}/storage/object/{bucket}/{*name}",
    "storageDeleteObject",
    ep::STORAGE_DELETE_OBJECT,
    summary = "Delete an object as a role",
    description = "Storage's delete as the role. Needs the role held at Developer.",
    params(
        ("bucket" = String, Path, description = "Bucket name"),
        (
            "name" = String,
            Path,
            description = "Object name (may contain `/`)"
        ),
    ),
    ok = (200, "Deleted")
);
storage_route!(
    delete_objects,
    delete,
    "/v1/databases/{id}/branches/{branch_id}/storage/object/{bucket}",
    "storageDeleteObjects",
    ep::STORAGE_DELETE_OBJECTS,
    summary = "Delete several objects as a role",
    description = "Storage's delete-many (`{\"names\": [...]}`) as the role. Needs the role held at Developer.",
    params(("bucket" = String, Path, description = "Bucket name"),),
    body(content = Value, content_type = "application/json"),
    ok = (200, "The names deleted")
);
storage_route!(
    copy_object,
    post,
    "/v1/databases/{id}/branches/{branch_id}/storage/object/copy",
    "storageCopyObject",
    ep::STORAGE_COPY_OBJECT,
    summary = "Copy an object as a role",
    description = "Storage's copy as the role. Needs the role held at Developer.",
    params(),
    body(content = Value, content_type = "application/json"),
    ok = (200, "Copied")
);
storage_route!(
    move_object,
    post,
    "/v1/databases/{id}/branches/{branch_id}/storage/object/move",
    "storageMoveObject",
    ep::STORAGE_MOVE_OBJECT,
    summary = "Move an object as a role",
    description = "Storage's move as the role. Needs the role held at Developer.",
    params(),
    body(content = Value, content_type = "application/json"),
    ok = (200, "Moved")
);
storage_route!(
    sign_object,
    post,
    "/v1/databases/{id}/branches/{branch_id}/storage/object/sign/{bucket}/{*name}",
    "storageSignObject",
    ep::STORAGE_SIGN_OBJECT,
    summary = "Create a signed download URL as a role",
    description = "Storage's signed download URL, minted as the role (it must be able to read the object). `expires_in` is 1 to 3600 seconds (default 3600). The URL works while it is the object's current version and while you still hold the role at Developer on the database; it stops when you are removed or the role's level drops below Developer. Needs the role held at Developer.",
    params(
        ("bucket" = String, Path, description = "Bucket name"),
        (
            "name" = String,
            Path,
            description = "Object name (may contain `/`)"
        ),
    ),
    body(content = Value, content_type = "application/json"),
    ok = (
        200,
        "`signed_url` (a path on the API host) and `expires_at` (seconds since the epoch)"
    )
);
storage_route!(
    resolve_objects,
    post,
    "/v1/databases/{id}/branches/{branch_id}/storage/object/resolve",
    "storageResolveObjects",
    ep::STORAGE_RESOLVE_OBJECTS,
    summary = "Resolve object refs to URLs as a role",
    description = "Storage's `POST /storage/v1/object/resolve` as the role: `{\"refs\": [...], \"expires_in\"?}`, at most 1000 refs, each `{bucket_id, name}`, `{bucket, name}` or `{id}`. For each ref, in order: a signed download URL when the role can read the bucket and the object (row level security), the public URL when the bucket is public, and `null` when it can't read it (missing or hidden alike). `expires_in` is 1 to 3600 seconds (default 3600). A signed URL works while it is the object's current version and while you hold the role at Developer on the database (Admin for one resolved in owner mode): one resolved as a role you hold only at Viewer is refused when opened. Needs the role held at Viewer.",
    params(),
    body(content = Value, content_type = "application/json"),
    ok = (
        200,
        "`urls`: one per ref, in order, each a path on the API host or `null`; `expires_at` (seconds since the epoch)"
    )
);
storage_route!(
    create_upload,
    post,
    "/v1/databases/{id}/branches/{branch_id}/storage/upload/{bucket}/{*name}",
    "storageCreateUpload",
    ep::STORAGE_CREATE_UPLOAD,
    summary = "Start a resumable upload as a role",
    description =
        "Storage's resumable upload session, yours alone. Needs the role held at Developer.",
    params(
        ("bucket" = String, Path, description = "Bucket name"),
        (
            "name" = String,
            Path,
            description = "Object name (may contain `/`)"
        ),
    ),
    body(content = Value, content_type = "application/json"),
    ok = (200, "The session")
);
storage_route!(upload_part, put, "/v1/databases/{id}/branches/{branch_id}/storage/upload/session/{session}/{part}", "storageUploadPart", ep::STORAGE_UPLOAD_PART,
    summary = "Upload one part of a resumable upload",
    description = "One part (5 MiB to 100 MiB, the last may be smaller), streamed with no body cap here. Needs the role held at Developer.",
    params(("session" = Uuid, Path, description = "Upload session id"), ("part" = u32, Path, description = "Part number, 1 to 10000"),),
    body(content = Vec<u8>, content_type = "application/octet-stream"),
    ok = (200, "The part's `etag` and `size`"));
storage_route!(
    list_parts,
    get,
    "/v1/databases/{id}/branches/{branch_id}/storage/upload/session/{session}",
    "storageListParts",
    ep::STORAGE_LIST_PARTS,
    summary = "List a resumable upload's parts",
    description = "Needs the role held at Developer.",
    params(("session" = Uuid, Path, description = "Upload session id"),),
    ok = (200, "The session and its parts")
);
storage_route!(
    complete_upload,
    post,
    "/v1/databases/{id}/branches/{branch_id}/storage/upload/session/{session}/complete",
    "storageCompleteUpload",
    ep::STORAGE_COMPLETE_UPLOAD,
    summary = "Complete a resumable upload",
    description = "Writes the object as the role (row level security decides). Needs the role held at Developer.",
    params(("session" = Uuid, Path, description = "Upload session id"),),
    body(content = Value, content_type = "application/json"),
    ok = (
        200,
        "Stored: `bucket`, `name`, `version_id`, `size`, `etag`"
    )
);
storage_route!(
    abort_upload,
    delete,
    "/v1/databases/{id}/branches/{branch_id}/storage/upload/session/{session}",
    "storageAbortUpload",
    ep::STORAGE_ABORT_UPLOAD,
    summary = "Abort a resumable upload",
    description = "Needs the role held at Developer.",
    params(("session" = Uuid, Path, description = "Upload session id"),),
    ok = (200, "Aborted")
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The storage path is the console path's tail under `/storage/v1`, as sent.
    #[test]
    fn a_console_path_names_its_storage_path() {
        let base = "/v1/databases/6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10/branches/7a1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10/storage";
        for (tail, want) in [
            ("/bucket", "/storage/v1/bucket"),
            ("/object/b/a%20b/c.txt", "/storage/v1/object/b/a%20b/c.txt"),
            ("/upload/session/s/3", "/storage/v1/upload/session/s/3"),
        ] {
            assert_eq!(
                storage_path(&format!("{base}{tail}")).as_deref(),
                Some(want),
                "{tail}"
            );
        }
        for bad in [
            "/v1/databases/x/keys/y/storage/bucket",
            "/v1/databases/x/branches/y/console/query",
            "/other",
        ] {
            assert_eq!(storage_path(bad), None, "{bad}");
        }
    }

    /// The B1 invariant, exhaustively: every path of up to four segments over route words,
    /// dots and their encodings (each case mix) is either refused by [`has_dot_segment`], or
    /// parsing it as storage's URL leaves the path exactly as authorized; and
    /// [`storage_url`] never hands on a URL whose path differs.
    /// Re-audit N5: storage's base URL is `scheme://host[:port]` with no path, query or
    /// fragment, checked at startup; a path there would make every console call fail later
    /// with an answer blaming the caller's path.
    #[test]
    fn the_storage_url_has_no_path() {
        for good in [
            "http://storage:8090",
            "http://storage:8090/",
            "https://10.0.0.5",
        ] {
            assert!(StorageInternal::new(good, "t").is_ok(), "{good}");
        }
        for bad in [
            "http://storage:8090/base",
            "http://storage:8090/?x=1",
            "http://storage:8090#f",
            "storage:8090",
            "ftp://storage",
            "http://",
        ] {
            assert!(StorageInternal::new(bad, "t").is_err(), "{bad}");
        }
    }

    #[test]
    fn a_path_is_refused_or_reaches_storage_unchanged() {
        const SEGMENTS: &[&str] = &[
            "b",
            "a.txt",
            "...",
            "a..",
            ".a",
            "",
            ".",
            "..",
            "%2e",
            "%2E",
            ".%2e",
            "%2E.",
            "%2e%2E",
            "%2E%2E",
            "%2e%2e%2e",
            "a\\..",
            "%5c",
            "%2F..",
        ];
        let base = "http://storage:5001";
        let mut paths = vec![String::from("/storage/v1")];
        let mut checked = 0;
        for _ in 0..4 {
            paths = paths
                .iter()
                .flat_map(|p| SEGMENTS.iter().map(move |s| format!("{p}/{s}")))
                .collect();
            for path in &paths {
                checked += 1;
                let url = storage_url(base, path, "database=web1");
                if let Some(url) = &url {
                    assert_eq!(url.path(), path, "storage_url changed {path}");
                }
                if !has_dot_segment(path) {
                    assert!(url.is_some(), "accepted {path}, but parsing changes it");
                }
            }
        }
        assert!(checked > 100_000, "{checked}");
        for dotted in [
            "/storage/v1/object/b/..",
            "/storage/v1/%2E%2e/x",
            "/storage/v1/.%2E",
            "/storage/v1/a\\b",
        ] {
            assert!(has_dot_segment(dotted), "{dotted}");
        }
        for fine in [
            "/storage/v1/object/b/...",
            "/storage/v1/object/b/a..b",
            "/storage/v1/%2e%2e%2e",
        ] {
            assert!(!has_dot_segment(fine), "{fine}");
        }
    }

    /// `?claims=` is a JSON object of at most 4 KiB; absent, `{}`.
    #[test]
    fn claims_are_a_small_json_object() {
        assert!(parse_claims(None).unwrap().is_empty());
        assert_eq!(parse_claims(Some(r#"{"sub":"u1"}"#)).unwrap()["sub"], "u1");
        for bad in ["[1]", "\"x\"", "not json", "null"] {
            assert_eq!(parse_claims(Some(bad)).unwrap_err().status, 422, "{bad}");
        }
        let big = format!(r#"{{"x":"{}"}}"#, "a".repeat(MAX_CLAIMS_BYTES));
        assert!(parse_claims(Some(&big)).is_err());
    }
}
