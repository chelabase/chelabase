//! `/storage/v1/object`: upload, download, head, delete, copy, move and list. Every route runs
//! its metadata statement in `storage.objects` as the caller (row level security decides)
//! before it touches the object store; an object the caller's policies hide is 404, as a
//! missing one. A bucket's id (a managed column) is looked up as storage itself.
//!
//! An upload (m3's tickets; no Postgres transaction is held across the body):
//! 1. pre-check as the caller: the existing row's version (if any) and, in a savepoint rolled
//!    back, the INSERT (or upsert) with the declared values: 42501 is 403, `CB003` (the policy
//!    passed, only the ticket is missing) continues;
//! 2. the body streams to a staging key through a byte counter (the bucket's limit: 413);
//! 3. storage writes a ticket for the real size and etag, the caller's role and `sub`;
//! 4. one statement as the caller writes the row; the trigger consumes the ticket. An upsert
//!    updates only the version the pre-check saw, so a lost race is 409 and the winner's row is
//!    unchanged;
//! 5. the staged blob moves to `{database_id}/{pg_database}/{version_id}`.
//!
//! Any refusal or failure deletes the staged blob ([`crate::names::settle`]).

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chelabase_auth::internal::DataIdentity;
use chelabase_pgsession::service::Type;
use chelabase_pgsession::{ConnSource, NamedStatement, named_statement};
use chelabase_storage::ByteRange;
use serde::Deserialize;
use serde_json::{Value, json};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::app::{AppState, identity};
use crate::buckets::{body, pg};
use crate::engine::Located;
use crate::error::Failure;
use crate::names::{self, Staged, check_object_name};
use crate::openapi::StorageErrorEnvelope;

/// A bucket's id and file size limit, by name (storage's own read: `bucket_id` is managed).
const BUCKET: NamedStatement = named_statement!(
    "bucket",
    "SELECT id, file_size_limit FROM storage.buckets WHERE name OPERATOR(pg_catalog.=) $1",
    &[Type::TEXT]
);

/// A ticket vouching for one row's managed columns, for the caller's role and owner, for five
/// minutes (the trigger stamps the timeline).
const TICKET: NamedStatement = named_statement!(
    "ticket",
    "INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role, owner) VALUES ($1, $2, $3, $4, $5, pg_catalog.clock_timestamp() OPERATOR(pg_catalog.+) interval '5 minutes', $6::pg_catalog.name, $7)",
    &[
        Type::UUID,
        Type::UUID,
        Type::TEXT,
        Type::INT8,
        Type::TEXT,
        Type::TEXT,
        Type::UUID
    ]
);

/// The trigger's code for a row its ticket doesn't vouch for.
const TICKET_MISSING: &str = "CB003";

/// The row an upload or copy writes.
struct Row {
    bucket: Uuid,
    name: String,
    owner: Option<Uuid>,
    size: Option<i64>,
    content_type: String,
    etag: Option<String>,
    version: Uuid,
}

const INSERT: &str = "INSERT INTO storage.objects (bucket_id, name, owner, size, content_type, etag, version_id) VALUES ($1, $2, $3, $4, $5, $6, $7)";
const UPSERT: &str = "INSERT INTO storage.objects (bucket_id, name, owner, size, content_type, etag, version_id) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (bucket_id, name) DO UPDATE SET owner = EXCLUDED.owner, size = EXCLUDED.size, content_type = EXCLUDED.content_type, etag = EXCLUDED.etag, version_id = EXCLUDED.version_id WHERE storage.objects.version_id IS NOT DISTINCT FROM $8";

/// The caller's `sub` claim as a uuid (an app user), else no owner.
fn owner_of(data: &DataIdentity) -> Option<Uuid> {
    data.claims
        .get("sub")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

fn sqlstate(f: &Failure) -> Option<&str> {
    f.0.sqlstate.as_deref()
}

/// The final write's failure: a missing ticket or a taken name is a lost race (409); RLS's
/// 42501 stays 403.
pub fn final_failure(f: Failure) -> Failure {
    match sqlstate(&f) {
        Some(TICKET_MISSING) | Some("23505") => Failure::object_conflict(),
        _ => f,
    }
}

impl<S: ConnSource, M: ConnSource> AppState<S, M> {
    /// `bucket`'s id and limit in the located database; 404 when there's none.
    async fn bucket(
        &self,
        data: &DataIdentity,
        at: &Located,
        bucket: &str,
        rid: &str,
    ) -> Result<(Uuid, Option<i64>), Failure> {
        let rows = self
            .engine
            .managed(data.branch_id, &at.database, &BUCKET, &[&bucket], rid)
            .await?;
        rows.first()
            .map(|r| (r.get(0), r.get(1)))
            .ok_or_else(|| Failure::bucket_not_found(bucket))
    }

    /// Pre-check (step 1): the version of the row the caller sees at the name (`None`: none),
    /// after the write with `row`'s declared values passed the policies in a rolled-back
    /// savepoint. An existing row without `upsert` is 409.
    async fn precheck(
        &self,
        data: &DataIdentity,
        at: &Located,
        row: &Row,
        upsert: bool,
        rid: &str,
    ) -> Result<Option<Uuid>, Failure> {
        let (seen, _) = self
            .engine
            .as_caller(data, Some(&at.database), rid, async |c, _| {
                let seen = c
                    .query_opt(
                        "SELECT version_id FROM storage.objects WHERE bucket_id = $1 AND name = $2",
                        &[&row.bucket, &row.name],
                    )
                    .await
                    .map_err(pg(rid))?
                    .map(|r| r.get::<_, Option<Uuid>>(0));
                if seen.is_some() && !upsert {
                    return Ok(Err(Failure::object_conflict()));
                }
                c.batch_execute("SAVEPOINT precheck")
                    .await
                    .map_err(pg(rid))?;
                let sql = if seen.is_some() { UPSERT } else { INSERT };
                let expected = seen.flatten();
                let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![
                    &row.bucket,
                    &row.name,
                    &row.owner,
                    &row.size,
                    &row.content_type,
                    &row.etag,
                    &row.version,
                ];
                if seen.is_some() {
                    params.push(&expected);
                }
                let tried = c.execute(sql, &params).await.map_err(pg(rid));
                c.batch_execute("ROLLBACK TO SAVEPOINT precheck")
                    .await
                    .map_err(pg(rid))?;
                Ok(match tried {
                    Err(f) if f.sqlstate.as_deref() == Some(TICKET_MISSING) => Ok(seen.flatten()),
                    Err(f) if f.sqlstate.as_deref() == Some("23505") => {
                        Err(Failure::object_conflict())
                    }
                    Err(f) => Err(Failure(f)),
                    // A ticket happened to match: the policy passed all the same.
                    Ok(_) => Ok(seen.flatten()),
                })
            })
            .await?;
        seen
    }

    /// Steps 3 and 4: the ticket, then the caller's own write of `row`. `expected`: the version
    /// the pre-check saw at the name (an upsert replaces only it).
    async fn write(
        &self,
        data: &DataIdentity,
        at: &Located,
        row: &Row,
        expected: Option<Uuid>,
        upsert: bool,
        rid: &str,
    ) -> Result<(), Failure> {
        let size = row.size.unwrap_or_default();
        self.engine
            .managed(
                data.branch_id,
                &at.database,
                &TICKET,
                &[
                    &row.version,
                    &row.bucket,
                    &row.name,
                    &size,
                    &row.etag,
                    &data.role,
                    &row.owner,
                ],
                rid,
            )
            .await?;
        let (written, _) = self
            .engine
            .as_caller(data, Some(&at.database), rid, async |c, _| {
                let base: [&(dyn tokio_postgres::types::ToSql + Sync); 7] = [
                    &row.bucket,
                    &row.name,
                    &row.owner,
                    &row.size,
                    &row.content_type,
                    &row.etag,
                    &row.version,
                ];
                if upsert && expected.is_some() {
                    let mut params = base.to_vec();
                    params.push(&expected);
                    c.execute(UPSERT, &params).await.map_err(pg(rid))
                } else {
                    c.execute(INSERT, &base).await.map_err(pg(rid))
                }
            })
            .await
            .map_err(final_failure)?;
        if written == 0 {
            // The upsert's row changed since the pre-check: someone else won.
            return Err(Failure::object_conflict());
        }
        Ok(())
    }
}

/// `?database=&upsert=&download=`.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ObjectParams {
    /// The Postgres database (the key's main one when left out).
    pub database: Option<String>,
    /// Replace an existing object (also the `x-upsert: true` header).
    pub upsert: Option<bool>,
    /// Download as an attachment; the value is the file name (empty: the object's own).
    pub download: Option<String>,
}

fn header(headers: &HeaderMap, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn upload_object<S: ConnSource, M: ConnSource>(
    state: Arc<AppState<S, M>>,
    headers: HeaderMap,
    (bucket, name): (String, String),
    q: ObjectParams,
    request: Body,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    check_object_name(&name)?;
    let upsert = q.upsert.unwrap_or(false) || header(&headers, "x-upsert") == Some("true");
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let (bucket_id, limit) = state.bucket(&data, &at, &bucket, &rid).await?;
    let declared = header(&headers, header::CONTENT_LENGTH).and_then(|v| v.parse::<i64>().ok());
    if let (Some(limit), Some(declared)) = (limit, declared)
        && declared > limit
    {
        return Err(Failure::too_large(limit));
    }
    let mut row = Row {
        bucket: bucket_id,
        name,
        owner: owner_of(&data),
        size: declared,
        content_type: header(&headers, header::CONTENT_TYPE)
            .unwrap_or("application/octet-stream")
            .to_owned(),
        etag: None,
        version: Uuid::new_v4(),
    };
    let expected = state.precheck(&data, &at, &row, upsert, &rid).await?;
    let staging = names::staging_key(&at.prefix, Uuid::new_v4());
    let (stream, counter) =
        names::counted(request.into_data_stream(), limit.map(|l| l.max(0) as u64));
    let meta = names::stage(
        state.store.as_ref(),
        &staging,
        stream,
        &counter,
        Some(row.content_type.clone()),
    )
    .await
    .map_err(|e| match e {
        Staged::TooLarge => Failure::too_large(limit.unwrap_or_default()),
        Staged::Store(e) => Failure::from_store(&e, &rid),
    })?;
    row.size = Some(meta.size as i64);
    row.etag = Some(meta.etag.trim_matches('"').to_owned());
    let written = state.write(&data, &at, &row, expected, upsert, &rid).await;
    let key = names::version_key(&at.prefix, row.version);
    names::settle(state.store.as_ref(), &staging, &key, written, &rid).await?;
    Ok(Json(json!({
        "bucket": bucket, "name": row.name, "version_id": row.version, "size": row.size, "etag": row.etag,
    }))
    .into_response())
}

#[utoipa::path(
    post,
    path = "/storage/v1/object/{bucket}/{*name}",
    operation_id = "uploadObject",
    tag = "storage",
    summary = "Upload an object",
    description = "Streams the body (any length, chunked included). Row level security on `storage.objects` decides, before any byte is stored and again when the row is written.",
    params(("bucket" = String, Path), ("name" = String, Path, description = "The object's name: no leading `/`, no empty or `..` segment, at most 1024 bytes"), ObjectParams),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Stored: `bucket`, `name`, `version_id`, `size`, `etag`", body = Value),
        (status = 400, description = "`invalid_request`", body = StorageErrorEnvelope),
        (status = 403, description = "`permission_denied`: no policy allows it; `database_not_allowed`, `role_not_allowed`", body = StorageErrorEnvelope),
        (status = 404, description = "`bucket_not_found`", body = StorageErrorEnvelope),
        (status = 409, description = "`conflict`: the name is taken (no upsert), or another upload changed it first", body = StorageErrorEnvelope),
        (status = 413, description = "`too_large`: over the bucket's file size limit", body = StorageErrorEnvelope),
        (status = 502, description = "`storage_error`: the object store failed", body = StorageErrorEnvelope),
        (status = 503, description = "`storage_unavailable`, `unavailable`", body = StorageErrorEnvelope),
    ),
)]
pub async fn upload<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path(path): Path<(String, String)>,
    Query(q): Query<ObjectParams>,
    request: Body,
) -> Result<Response, Failure> {
    upload_object(state, headers, path, q, request).await
}

#[utoipa::path(
    put,
    path = "/storage/v1/object/{bucket}/{*name}",
    operation_id = "putObject",
    tag = "storage",
    summary = "Upload an object (as POST)",
    params(("bucket" = String, Path), ("name" = String, Path), ObjectParams),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    security(("bearer" = [])),
    responses((status = 200, description = "As POST", body = Value)),
)]
pub async fn put<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path(path): Path<(String, String)>,
    Query(q): Query<ObjectParams>,
    request: Body,
) -> Result<Response, Failure> {
    upload_object(state, headers, path, q, request).await
}

/// What a read needs of the row the caller sees.
struct Found {
    version: Uuid,
    size: i64,
    etag: String,
    content_type: String,
    owner: Option<Uuid>,
}

impl<S: ConnSource, M: ConnSource> AppState<S, M> {
    /// The object `name` of `bucket` as the caller sees it; 404 when missing or hidden.
    async fn find(
        &self,
        data: &DataIdentity,
        at: &Located,
        bucket: &str,
        name: &str,
        rid: &str,
    ) -> Result<(Uuid, Found), Failure> {
        let (bucket_id, _) = self.bucket(data, at, bucket, rid).await?;
        let (row, _) = self
            .engine
            .as_caller(data, Some(&at.database), rid, async |c, _| {
                c.query_opt(
                    "SELECT version_id, size, etag, content_type, owner FROM storage.objects WHERE bucket_id = $1 AND name = $2 AND version_id IS NOT NULL",
                    &[&bucket_id, &name],
                )
                .await
                .map_err(pg(rid))
            })
            .await?;
        let row = row.ok_or_else(Failure::object_not_found)?;
        Ok((
            bucket_id,
            Found {
                version: row.get(0),
                size: row.get::<_, Option<i64>>(1).unwrap_or_default(),
                etag: row.get::<_, Option<String>>(2).unwrap_or_default(),
                content_type: row
                    .get::<_, Option<String>>(3)
                    .unwrap_or_else(|| "application/octet-stream".into()),
                owner: row.get(4),
            },
        ))
    }
}

fn object_headers(found: &Found, name: &str, download: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    let set = |h: &mut HeaderMap, k: header::HeaderName, v: String| {
        if let Ok(v) = HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
    };
    set(&mut h, header::ETAG, names::quoted_etag(&found.etag));
    set(&mut h, header::CONTENT_TYPE, found.content_type.clone());
    set(
        &mut h,
        header::CONTENT_DISPOSITION,
        names::content_disposition(name, download),
    );
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h
}

#[utoipa::path(
    get,
    path = "/storage/v1/object/{bucket}/{*name}",
    operation_id = "downloadObject",
    tag = "storage",
    summary = "Download an object",
    description = "`Range` (one range) answers 206 with `Content-Range`; a matching `If-None-Match` answers 304.",
    params(("bucket" = String, Path), ("name" = String, Path), ObjectParams),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The object", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 206, description = "The range asked for"),
        (status = 304, description = "Not modified"),
        (status = 404, description = "`object_not_found`: missing, or hidden from the caller", body = StorageErrorEnvelope),
        (status = 416, description = "`range_not_satisfiable`", body = StorageErrorEnvelope),
    ),
)]
pub async fn download<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((bucket, name)): Path<(String, String)>,
    Query(q): Query<ObjectParams>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    check_object_name(&name)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let (_, found) = state.find(&data, &at, &bucket, &name, &rid).await?;
    let mut out = object_headers(&found, &name, q.download.as_deref());
    if header(&headers, header::IF_NONE_MATCH).is_some_and(|v| names::none_match(v, &found.etag)) {
        return Ok((StatusCode::NOT_MODIFIED, out).into_response());
    }
    let range = match header(&headers, header::RANGE) {
        Some(r) => Some(ByteRange::parse(r).map_err(|_| Failure::range_not_satisfiable())?),
        None => None,
    };
    let span = match range {
        Some(r) => Some(
            r.resolve(found.size.max(0) as u64)
                .map_err(|_| Failure::range_not_satisfiable())?,
        ),
        None => None,
    };
    let key = names::version_key(&at.prefix, found.version);
    let object = state
        .store
        .get(&key, range)
        .await
        .map_err(|e| Failure::from_store(&e, &rid))?;
    let status = match span {
        Some(span) => {
            let v = format!(
                "bytes {}-{}/{}",
                span.start,
                span.end.saturating_sub(1),
                object.total_size
            );
            out.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&v).expect("ascii"),
            );
            out.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from(span.end - span.start),
            );
            StatusCode::PARTIAL_CONTENT
        }
        None => {
            out.insert(header::CONTENT_LENGTH, HeaderValue::from(object.total_size));
            StatusCode::OK
        }
    };
    Ok((status, out, Body::from_stream(object.body)).into_response())
}

#[utoipa::path(
    head,
    path = "/storage/v1/object/{bucket}/{*name}",
    operation_id = "headObject",
    tag = "storage",
    summary = "An object's headers",
    params(("bucket" = String, Path), ("name" = String, Path), ObjectParams),
    security(("bearer" = [])),
    responses((status = 200, description = "The object's headers"), (status = 404, description = "Missing or hidden")),
)]
pub async fn head<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((bucket, name)): Path<(String, String)>,
    Query(q): Query<ObjectParams>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    check_object_name(&name)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let (_, found) = state.find(&data, &at, &bucket, &name, &rid).await?;
    let mut out = object_headers(&found, &name, None);
    out.insert(header::CONTENT_LENGTH, HeaderValue::from(found.size.max(0)));
    Ok((StatusCode::OK, out).into_response())
}

impl<S: ConnSource, M: ConnSource> AppState<S, M> {
    /// Deletes the rows `names` of `bucket` as the caller; the names deleted. The blobs stay
    /// for the sweeper (m8).
    async fn delete_rows(
        &self,
        data: &DataIdentity,
        q: &ObjectParams,
        bucket: &str,
        names: &[String],
        rid: &str,
    ) -> Result<Vec<String>, Failure> {
        for name in names {
            check_object_name(name)?;
        }
        let at = self.engine.locate(data, q.database.as_deref(), rid).await?;
        let (bucket_id, _) = self.bucket(data, &at, bucket, rid).await?;
        let (deleted, _) = self
            .engine
            .as_caller(data, Some(&at.database), rid, async |c, _| {
                let rows = c
                    .query("DELETE FROM storage.objects WHERE bucket_id = $1 AND name = ANY($2) RETURNING name", &[&bucket_id, &names])
                    .await
                    .map_err(pg(rid))?;
                Ok(rows.iter().map(|r| r.get(0)).collect::<Vec<String>>())
            })
            .await?;
        Ok(deleted)
    }
}

#[utoipa::path(
    delete,
    path = "/storage/v1/object/{bucket}/{*name}",
    operation_id = "deleteObject",
    tag = "storage",
    summary = "Delete an object",
    params(("bucket" = String, Path), ("name" = String, Path), ObjectParams),
    security(("bearer" = [])),
    responses((status = 204, description = "Deleted"), (status = 404, description = "`object_not_found`: missing, or hidden", body = StorageErrorEnvelope)),
)]
pub async fn delete<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((bucket, name)): Path<(String, String)>,
    Query(q): Query<ObjectParams>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let deleted = state.delete_rows(&data, &q, &bucket, &[name], &rid).await?;
    if deleted.is_empty() {
        return Err(Failure::object_not_found());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Objects to delete.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteObjects {
    /// At most 1000.
    pub names: Vec<String>,
}

#[utoipa::path(
    delete,
    path = "/storage/v1/object/{bucket}",
    operation_id = "deleteObjects",
    tag = "storage",
    summary = "Delete several objects",
    params(("bucket" = String, Path), ObjectParams),
    request_body = DeleteObjects,
    security(("bearer" = [])),
    responses((status = 200, description = "`deleted`: the names deleted (those missing or hidden are left out)", body = Value)),
)]
pub async fn delete_many<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path(bucket): Path<String>,
    Query(q): Query<ObjectParams>,
    json: Result<Json<DeleteObjects>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let names = body(json)?.names;
    if names.is_empty() || names.len() > 1000 {
        return Err(Failure::invalid_request("names lists 1 to 1000 objects"));
    }
    let deleted = state.delete_rows(&data, &q, &bucket, &names, &rid).await?;
    Ok(Json(json!({ "deleted": deleted })).into_response())
}

/// A copy or a move.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Relocate {
    pub bucket: String,
    pub from: String,
    pub to: String,
    /// The destination bucket (default: the same).
    pub to_bucket: Option<String>,
}

#[utoipa::path(
    post,
    path = "/storage/v1/object/copy",
    operation_id = "copyObject",
    tag = "storage",
    summary = "Copy an object",
    description = "Reads the source as the caller, checks the destination's policies, copies the blob to a new version and writes the destination row as the caller.",
    params(ObjectParams),
    request_body = Relocate,
    security(("bearer" = [])),
    responses((status = 200, description = "`version_id` of the copy", body = Value), (status = 404, description = "The source is missing or hidden", body = StorageErrorEnvelope), (status = 409, description = "The destination exists", body = StorageErrorEnvelope)),
)]
pub async fn copy<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Query(q): Query<ObjectParams>,
    json: Result<Json<Relocate>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let r = body(json)?;
    check_object_name(&r.from)?;
    check_object_name(&r.to)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let (_, src) = state.find(&data, &at, &r.bucket, &r.from, &rid).await?;
    let to_bucket = r.to_bucket.as_deref().unwrap_or(&r.bucket);
    let (bucket_id, _) = state.bucket(&data, &at, to_bucket, &rid).await?;
    let row = Row {
        bucket: bucket_id,
        name: r.to.clone(),
        owner: owner_of(&data),
        size: Some(src.size),
        content_type: src.content_type.clone(),
        etag: Some(src.etag.clone()),
        version: Uuid::new_v4(),
    };
    let expected = state.precheck(&data, &at, &row, false, &rid).await?;
    // Only after the caller may read the source and write the destination: the blob is copied
    // to the new version's key, deleted again if the row isn't written.
    let key = names::version_key(&at.prefix, row.version);
    let meta = state
        .store
        .copy(&names::version_key(&at.prefix, src.version), &key)
        .await
        .map_err(|e| Failure::from_store(&e, &rid))?;
    let row = Row {
        etag: Some(meta.etag.trim_matches('"').to_owned()),
        size: Some(meta.size as i64),
        ..row
    };
    if let Err(f) = state.write(&data, &at, &row, expected, false, &rid).await {
        let _ = state.store.delete(&key).await;
        return Err(f);
    }
    Ok(
        Json(json!({ "bucket": to_bucket, "name": row.name, "version_id": row.version }))
            .into_response(),
    )
}

#[utoipa::path(
    post,
    path = "/storage/v1/object/move",
    operation_id = "moveObject",
    tag = "storage",
    summary = "Move or rename an object",
    description = "Metadata only: the blob stays where it is. Moving to another bucket needs a ticket for the same version, written by storage once the caller can see the source.",
    params(ObjectParams),
    request_body = Relocate,
    security(("bearer" = [])),
    responses((status = 204, description = "Moved"), (status = 404, description = "The source is missing or hidden", body = StorageErrorEnvelope), (status = 409, description = "The destination exists", body = StorageErrorEnvelope)),
)]
pub async fn r#move<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Query(q): Query<ObjectParams>,
    json: Result<Json<Relocate>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let r = body(json)?;
    check_object_name(&r.from)?;
    check_object_name(&r.to)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let (from_id, src) = state.find(&data, &at, &r.bucket, &r.from, &rid).await?;
    let (to_id, _) = match r.to_bucket.as_deref() {
        Some(b) if b != r.bucket => state.bucket(&data, &at, b, &rid).await?,
        _ => (from_id, None),
    };
    if to_id != from_id {
        let size = src.size;
        let etag = src.etag.clone();
        state
            .engine
            .managed(
                data.branch_id,
                &at.database,
                &TICKET,
                &[
                    &src.version,
                    &to_id,
                    &r.to,
                    &size,
                    &etag,
                    &data.role,
                    &src.owner,
                ],
                &rid,
            )
            .await?;
    }
    let (moved, _) = state
        .engine
        .as_caller(&data, Some(&at.database), &rid, async |c, _| {
            c.execute(
                "UPDATE storage.objects SET bucket_id = $3, name = $4 WHERE bucket_id = $1 AND name = $2 AND version_id = $5",
                &[&from_id, &r.from, &to_id, &r.to, &src.version],
            )
            .await
            .map_err(pg(&rid))
        })
        .await
        .map_err(final_failure)?;
    if moved == 0 {
        return Err(Failure::object_not_found());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A page of a bucket's objects.
#[derive(Deserialize, ToSchema, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ListObjects {
    /// Only names starting with it.
    pub prefix: String,
    /// Group names by the next occurrence of it after the prefix into folders (`/`, typically).
    pub delimiter: Option<String>,
    /// 1 to 1000 (default 100).
    pub limit: Option<i64>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<String>,
    /// `name` (default), `size` or `updated_at`.
    pub sort_by: Option<String>,
    /// `asc` (default) or `desc`.
    pub order: Option<String>,
    /// Only names containing it (case-insensitive).
    pub search: Option<String>,
}

/// The list statement for a sort column and order (both from fixed lists); the parameters:
/// bucket, prefix, delimiter (empty: none), search, cursor value, cursor key, limit.
pub fn list_sql(sort_by: &str, desc: bool) -> Option<String> {
    let (value, cast) = match sort_by {
        "name" => ("key", "text"),
        "size" => ("coalesce(size, -1)", "bigint"),
        "updated_at" => (
            "coalesce(updated_at, '-infinity'::timestamptz)",
            "timestamptz",
        ),
        _ => return None,
    };
    let (cmp, dir) = if desc { ("<", "DESC") } else { (">", "ASC") };
    Some(format!(
        "WITH e AS (
  SELECT CASE WHEN $3 <> '' AND strpos(substr(name, length($2) + 1), $3) > 0
              THEN $2 || split_part(substr(name, length($2) + 1), $3, 1) || $3 ELSE name END AS key,
         name, size, etag, content_type, updated_at
  FROM storage.objects
  WHERE bucket_id = $1 AND starts_with(name, $2) AND ($4::text IS NULL OR strpos(lower(name), lower($4)) > 0)
), g AS (
  SELECT key, bool_or(key <> name) AS folder, max(size) FILTER (WHERE key = name) AS size,
         max(etag) FILTER (WHERE key = name) AS etag, max(content_type) FILTER (WHERE key = name) AS content_type,
         max(updated_at) FILTER (WHERE key = name) AS updated_at
  FROM e GROUP BY key
)
SELECT key, folder, size, etag, content_type, updated_at, ({value})::text FROM g
WHERE $6::text IS NULL OR ({value}, key) {cmp} ($5::text::{cast}, $6)
ORDER BY {value} {dir}, key {dir} LIMIT $7"
    ))
}

#[utoipa::path(
    post,
    path = "/storage/v1/object/list/{bucket}",
    operation_id = "listObjects",
    tag = "storage",
    summary = "List a bucket's objects",
    description = "As the caller: only the objects its policies show. With a delimiter, each folder and each file once.",
    params(("bucket" = String, Path), ObjectParams),
    request_body = ListObjects,
    security(("bearer" = [])),
    responses((status = 200, description = "`items` (`name`, `folder`, `size`, `etag`, `content_type`, `updated_at`) and `next_cursor`", body = Value)),
)]
pub async fn list<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path(bucket): Path<String>,
    Query(q): Query<ObjectParams>,
    json: Result<Json<ListObjects>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let l = body(json)?;
    let limit = l.limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err(Failure::invalid_request("limit is 1 to 1000"));
    }
    let desc = match l.order.as_deref() {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(_) => return Err(Failure::invalid_request("order is `asc` or `desc`")),
    };
    let sql = list_sql(l.sort_by.as_deref().unwrap_or("name"), desc)
        .ok_or_else(|| Failure::invalid_request("sort_by is `name`, `size` or `updated_at`"))?;
    let (cursor_value, cursor_key) = match l.cursor.as_deref().map(names::parse_cursor) {
        Some(Some((v, k))) => (Some(v.to_owned()), Some(k.to_owned())),
        Some(None) => return Err(Failure::invalid_request("an unknown cursor")),
        None => (None, None),
    };
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let (bucket_id, _) = state.bucket(&data, &at, &bucket, &rid).await?;
    let delimiter = l.delimiter.unwrap_or_default();
    let fetch = limit + 1;
    let (rows, _) = state
        .engine
        .as_caller(&data, Some(&at.database), &rid, async |c, _| {
            c.query(
                &sql,
                &[
                    &bucket_id,
                    &l.prefix,
                    &delimiter,
                    &l.search,
                    &cursor_value,
                    &cursor_key,
                    &fetch,
                ],
            )
            .await
            .map_err(pg(&rid))
        })
        .await?;
    let more = rows.len() as i64 > limit;
    let page = &rows[..rows.len().min(limit as usize)];
    let items: Vec<Value> = page
        .iter()
        .map(|r| {
            let updated: Option<std::time::SystemTime> = r.get(5);
            json!({
                "name": r.get::<_, String>(0), "folder": r.get::<_, bool>(1), "size": r.get::<_, Option<i64>>(2),
                "etag": r.get::<_, Option<String>>(3), "content_type": r.get::<_, Option<String>>(4),
                "updated_at": updated.map(|t| t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())),
            })
        })
        .collect();
    let next = if more {
        page.last()
            .map(|r| names::make_cursor(&r.get::<_, String>(6), &r.get::<_, String>(0)))
    } else {
        None
    };
    Ok(Json(json!({ "items": items, "next_cursor": next })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_query::QueryFailure;

    fn failure(sqlstate: &str) -> Failure {
        let mut f = QueryFailure::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "x");
        f.sqlstate = Some(sqlstate.into());
        Failure(f)
    }

    /// AC: a lost race (the ticket no longer matches, or the name was taken) is 409; a policy
    /// that changed since the pre-check is still RLS's 403.
    #[test]
    fn the_final_write_maps_a_lost_race_to_409_and_a_policy_to_403() {
        assert_eq!(
            final_failure(failure("CB003")).0.status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            final_failure(failure("23505")).0.status,
            StatusCode::CONFLICT
        );
        let mut rls = QueryFailure::new(StatusCode::FORBIDDEN, "permission_denied", "rls");
        rls.sqlstate = Some("42501".into());
        assert_eq!(final_failure(Failure(rls)).0.status, StatusCode::FORBIDDEN);
    }

    /// The list statement takes only its fixed sort columns; the cursor compares on the same
    /// (value, key) pair it orders by.
    #[test]
    fn the_list_statement_sorts_only_by_its_columns() {
        assert!(list_sql("name; DROP TABLE x", false).is_none());
        let sql = list_sql("size", true).unwrap();
        assert!(
            sql.contains("(coalesce(size, -1), key) < ($5::text::bigint, $6)"),
            "{sql}"
        );
        assert!(
            sql.contains("ORDER BY coalesce(size, -1) DESC, key DESC LIMIT $7"),
            "{sql}"
        );
        assert!(
            list_sql("name", false)
                .unwrap()
                .contains("(key, key) > ($5::text::text, $6)")
        );
    }

    /// The owner is the caller's `sub` when it's a uuid (an app user), else none.
    #[test]
    fn the_owner_is_the_callers_sub() {
        let mut data: DataIdentity = serde_json::from_value(json!({
            "branch_id": Uuid::nil(), "key_id": Uuid::nil(), "kind": "public", "role": "authenticated", "claims": {}
        }))
        .unwrap();
        assert_eq!(owner_of(&data), None);
        let user = Uuid::new_v4();
        data.claims = json!({ "sub": user.to_string() });
        assert_eq!(owner_of(&data), Some(user));
        data.claims = json!({ "sub": "not-a-uuid" });
        assert_eq!(owner_of(&data), None);
    }
}
