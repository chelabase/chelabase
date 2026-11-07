//! Public buckets (m6): `GET /storage/v1/object/public/{branch}/{bucket}/{*name}` serves an
//! object of a bucket with `public = true` without a key. `buckets.public` is read as storage
//! itself, in one narrow statement with the object's row, never as the caller's role; a
//! private bucket, a missing object, an unknown branch or a database not served all answer
//! the same 404.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use chelabase_pgsession::service::Type;
use chelabase_pgsession::{ConnSource, NamedStatement, named_statement};
use uuid::Uuid;

use crate::app::AppState;
use crate::engine::datoid_of;
use crate::error::Failure;
use crate::names::check_object_name;
use crate::objects::{Found, ObjectParams, serve};
use crate::openapi::StorageErrorEnvelope;
use crate::signed::request_id;

/// How long a public object may be cached anywhere.
pub const PUBLIC_CACHE_CONTROL: &str = "public, max-age=3600";

/// A public bucket's object of that name, with the database's OID.
const PUBLIC_ROW: NamedStatement = named_statement!(
    "public row",
    "SELECT o.version_id, o.size, o.etag, o.content_type, (SELECT oid::pg_catalog.int8 FROM pg_catalog.pg_database WHERE datname OPERATOR(pg_catalog.=) pg_catalog.current_database()) FROM storage.objects o JOIN storage.buckets b ON b.id OPERATOR(pg_catalog.=) o.bucket_id WHERE b.public AND b.name OPERATOR(pg_catalog.=) $1 AND o.name OPERATOR(pg_catalog.=) $2 AND o.version_id IS NOT NULL AND o.size IS NOT NULL",
    &[Type::TEXT, Type::TEXT]
);

/// What an unauthenticated caller learns of a failure: 503s (try again) stay, anything else
/// (an unknown branch, a database not served) is the object's 404.
pub fn hide(f: Failure) -> Failure {
    if f.0.status.is_server_error() {
        f
    } else {
        Failure::object_not_found()
    }
}

#[utoipa::path(
    get,
    path = "/storage/v1/object/public/{branch}/{bucket}/{*name}",
    operation_id = "publicDownload",
    tag = "storage",
    summary = "Download from a public bucket",
    description = "No key. Only a bucket with `public = true`; cached for an hour (`Cache-Control: public, max-age=3600`). `Range`, `If-None-Match` and the sandboxed headers as a signed download.",
    params(("branch" = Uuid, Path, description = "The branch the bucket is on"), ("bucket" = String, Path), ("name" = String, Path), ObjectParams),
    responses(
        (status = 200, description = "The object", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 404, description = "`object_not_found`: missing, or the bucket isn't public", body = StorageErrorEnvelope),
    ),
)]
pub async fn public_download<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((branch, bucket, name)): Path<(String, String, String)>,
    Query(q): Query<ObjectParams>,
) -> Result<Response, Failure> {
    let rid = request_id(&headers);
    check_object_name(&name)?;
    let branch: Uuid = branch.parse().map_err(|_| Failure::object_not_found())?;
    let at = state
        .engine
        .locate_on(branch, q.database.as_deref(), None, &rid)
        .await
        .map_err(hide)?;
    let rows = state
        .engine
        .managed(branch, &at.database, &PUBLIC_ROW, &[&bucket, &name], &rid)
        .await?;
    let row = rows.first().ok_or_else(Failure::object_not_found)?;
    let datoid = datoid_of(row.get(4)).ok_or_else(Failure::storage_unavailable)?;
    let found = Found {
        datoid,
        version: row.get(0),
        size: row.get(1),
        etag: row.get::<_, Option<String>>(2).unwrap_or_default(),
        content_type: row
            .get::<_, Option<String>>(3)
            .unwrap_or_else(|| "application/octet-stream".into()),
        owner: None,
    };
    let mut out = serve(
        &state,
        &headers,
        at.database_id,
        &found,
        &name,
        q.download.as_deref(),
        &rid,
    )
    .await?;
    out.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(PUBLIC_CACHE_CONTROL),
    );
    Ok(out)
}
