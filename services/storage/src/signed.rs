//! Signed object URLs (m6). A caller who can read an object mints a download URL for its
//! current version; one who can see a bucket mints a one-time upload URL for a name. Either
//! is served without a key: the token ([`crate::token`]) is the credential.
//!
//! Serving a download, in order, before any object-store read:
//! 1. the token verifies for this purpose, bucket and name, unexpired (401 or 403);
//! 2. the key that minted it is still live on the token's branch, and still reaches its
//!    database (403; cached [`crate::control::LIVENESS_TTL`]);
//! 3. the token's database is still served on that branch, the same Chelabase database and the
//!    same Postgres database (its OID), and a row of that name still exists there, read as
//!    storage itself (404). The version served is the token's: an overwrite doesn't change
//!    what the URL serves, a delete ends it.
//!
//! An upload token is recorded in the control plane when minted and spent there by its one
//! upload (on any replica, across restarts); the upload then runs as the minting key's current
//! role with the minting request's claims, exactly as m5's upload (row level security decides).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use chelabase_pgsession::service::Type;
use chelabase_pgsession::{ConnSource, NamedStatement, named_statement};
use serde::Deserialize;
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::app::{AppState, identity};
use crate::buckets::body;
use crate::engine::datoid_of;
use crate::error::Failure;
use crate::names::{self, check_object_name};
use crate::objects::{Found, ObjectParams, serve, upload_as};
use crate::openapi::StorageErrorEnvelope;
use crate::token::{self, Claims, Purpose, TokenError};

/// A URL's default lifetime, and the longest one.
pub const DEFAULT_TTL: i64 = 3600;
pub const MAX_TTL: i64 = chelabase_storage::MAX_SIGNED_URL_TTL.as_secs() as i64;

/// The database's OID and whether a row of the name still exists in the bucket (storage's own
/// read: the token's minter could read it, so no policy is asked again).
const SIGNED_ROW: NamedStatement = named_statement!(
    "signed row",
    "SELECT (SELECT oid::pg_catalog.int8 FROM pg_catalog.pg_database WHERE datname OPERATOR(pg_catalog.=) pg_catalog.current_database()), EXISTS (SELECT 1 FROM storage.objects o JOIN storage.buckets b ON b.id OPERATOR(pg_catalog.=) o.bucket_id WHERE b.name OPERATOR(pg_catalog.=) $1 AND o.name OPERATOR(pg_catalog.=) $2)",
    &[Type::TEXT, Type::TEXT]
);

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The gateway's request id (a public route carries no identity token).
pub fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| Uuid::new_v4().to_string(), str::to_owned)
}

/// A token's refusal: changed or unknown and expired are 401, another object's 403.
pub fn token_failure(e: TokenError) -> Failure {
    match e {
        TokenError::Invalid => Failure::invalid_token(),
        TokenError::Expired => Failure::token_expired(),
        TokenError::Mismatch => Failure::token_refused(),
    }
}

/// A URL's lifetime in seconds: [`DEFAULT_TTL`], or 1 to [`MAX_TTL`] (else 400).
pub fn ttl_of(expires_in: Option<i64>) -> Result<i64, Failure> {
    match expires_in.unwrap_or(DEFAULT_TTL) {
        ttl @ 1..=MAX_TTL => Ok(ttl),
        _ => Err(Failure::invalid_request(
            "expires_in is 1 to 604800 seconds (7 days)",
        )),
    }
}

/// A signed URL's lifetime.
#[derive(Deserialize, ToSchema, Default)]
#[serde(deny_unknown_fields, default)]
pub struct SignBody {
    /// Seconds, 1 to 604800 (7 days); default 3600.
    pub expires_in: Option<i64>,
}

/// `?token=` (and a download's `download=`, an upload's `upsert=`).
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SignedParams {
    /// The token the signed URL carries.
    pub token: Option<String>,
    /// Download as an attachment; the value is the file name (empty: the object's own).
    pub download: Option<String>,
    /// Replace an existing object (an upload URL).
    pub upsert: Option<bool>,
}

fn signed_url(route: &str, bucket: &str, name: &str, token: &str) -> String {
    format!(
        "/storage/v1/object/{route}/{}/{}?token={token}",
        names::encode_path(bucket),
        names::encode_path(name)
    )
}

#[utoipa::path(
    post,
    path = "/storage/v1/object/sign/{bucket}/{*name}",
    operation_id = "signObject",
    tag = "storage",
    summary = "Create a signed download URL",
    description = "The caller must be able to read the object (row level security). The URL serves the object's current version without a key until it expires, the object is deleted or the key is revoked.",
    params(("bucket" = String, Path), ("name" = String, Path), ObjectParams),
    request_body = SignBody,
    security(("bearer" = [])),
    responses(
        (status = 200, description = "`signed_url` (a path on the API host) and `expires_at` (seconds since the epoch)", body = Value),
        (status = 400, description = "`invalid_request`: `expires_in` outside 1 to 604800", body = StorageErrorEnvelope),
        (status = 403, description = "`not_allowed`", body = StorageErrorEnvelope),
        (status = 404, description = "`object_not_found`: missing, or hidden from the caller", body = StorageErrorEnvelope),
    ),
)]
pub async fn sign<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((bucket, name)): Path<(String, String)>,
    Query(q): Query<ObjectParams>,
    json: Result<Json<SignBody>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    check_object_name(&name)?;
    let ttl = ttl_of(body(json)?.expires_in)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let (_, found) = state.find(&data, &at, &bucket, &name, &rid).await?;
    let key = state.urls.key(&rid).await?;
    let exp = now() + ttl as u64;
    let claims = Claims {
        purpose: Purpose::Download,
        database_id: at.database_id,
        branch_id: data.branch_id,
        database: at.database,
        datoid: Some(found.datoid),
        key_id: data.key_id,
        bucket,
        name,
        version_id: Some(found.version),
        size: Some(found.size),
        etag: Some(found.etag),
        content_type: Some(found.content_type),
        token_id: Uuid::new_v4(),
        exp,
    };
    let token = token::sign(key, &claims);
    let url = signed_url("sign", &claims.bucket, &claims.name, &token);
    Ok(Json(json!({ "signed_url": url, "expires_at": exp })).into_response())
}

impl<S: ConnSource, M: ConnSource> AppState<S, M> {
    /// The token's claims once it verifies and its key is still live on its branch and still
    /// reaches its database; nothing is read from the branch or the store before.
    async fn open_token(
        &self,
        token: Option<&str>,
        purpose: Purpose,
        bucket: &str,
        name: &str,
        rid: &str,
    ) -> Result<Claims, Failure> {
        check_object_name(name)?;
        let token = token.ok_or_else(Failure::invalid_token)?;
        let key = self.urls.key(rid).await?;
        token::verify(key, token, purpose, bucket, name, now()).map_err(token_failure)
    }
}

/// Whether `key` (live) may still use `claims`: the same branch, and its database list (if
/// any) still names the token's database.
pub fn key_still_reaches(key: &crate::control::LiveKey, claims: &Claims) -> bool {
    key.key_id == claims.key_id
        && key.branch_id == claims.branch_id
        && key
            .databases
            .as_ref()
            .is_none_or(|list| list.contains(&claims.database))
}

#[utoipa::path(
    get,
    path = "/storage/v1/object/sign/{bucket}/{*name}",
    operation_id = "signedDownload",
    tag = "storage",
    summary = "Download through a signed URL",
    description = "No key: the token is the credential. `Range` and `If-None-Match` as a download. Served sandboxed (`Content-Security-Policy: sandbox`, `nosniff`); anything but an image (not SVG), video, audio, PDF or plain text is an attachment.",
    params(("bucket" = String, Path), ("name" = String, Path), SignedParams),
    responses(
        (status = 200, description = "The object", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 401, description = "`invalid_token`, `token_expired`", body = StorageErrorEnvelope),
        (status = 403, description = "`token_not_valid`: another object's token, or its key was revoked", body = StorageErrorEnvelope),
        (status = 404, description = "`object_not_found`: deleted since", body = StorageErrorEnvelope),
    ),
)]
pub async fn signed_download<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((bucket, name)): Path<(String, String)>,
    Query(q): Query<SignedParams>,
) -> Result<Response, Failure> {
    let rid = request_id(&headers);
    let claims = state
        .open_token(q.token.as_deref(), Purpose::Download, &bucket, &name, &rid)
        .await?;
    let live = state.urls.live_key(claims.key_id, &rid).await?;
    if !live.is_some_and(|k| key_still_reaches(&k, &claims)) {
        return Err(Failure::token_refused());
    }
    let (Some(version), Some(datoid), Some(size)) = (claims.version_id, claims.datoid, claims.size)
    else {
        return Err(Failure::invalid_token());
    };
    let at = state
        .engine
        .locate_on(claims.branch_id, Some(&claims.database), None, &rid)
        .await
        .map_err(|f| match f.0.status.as_u16() {
            401 | 403 => Failure::object_not_found(),
            _ => f,
        })?;
    if at.database_id != claims.database_id {
        return Err(Failure::object_not_found());
    }
    let rows = state
        .engine
        .managed(
            claims.branch_id,
            &at.database,
            &SIGNED_ROW,
            &[&bucket, &name],
            &rid,
        )
        .await?;
    let row = rows.first().ok_or_else(Failure::storage_unavailable)?;
    if datoid_of(row.get(0)) != Some(datoid) || !row.get::<_, bool>(1) {
        return Err(Failure::object_not_found());
    }
    let found = Found {
        datoid,
        version,
        size,
        etag: claims.etag.clone().unwrap_or_default(),
        content_type: claims
            .content_type
            .clone()
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
    // A browser may keep it, but asks again each time: a revoked key's URL stops serving.
    out.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    Ok(out)
}

#[utoipa::path(
    post,
    path = "/storage/v1/object/upload/sign/{bucket}/{*name}",
    operation_id = "signUpload",
    tag = "storage",
    summary = "Create a one-time upload URL",
    description = "The caller must be able to see the bucket. The URL takes one upload (`PUT`) without a key, as the caller: row level security decides then.",
    params(("bucket" = String, Path), ("name" = String, Path), ObjectParams),
    request_body = SignBody,
    security(("bearer" = [])),
    responses(
        (status = 200, description = "`signed_url` (a path on the API host) and `expires_at`", body = Value),
        (status = 400, description = "`invalid_request`", body = StorageErrorEnvelope),
        (status = 403, description = "`not_allowed`", body = StorageErrorEnvelope),
    ),
)]
pub async fn sign_upload<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((bucket, name)): Path<(String, String)>,
    Query(q): Query<ObjectParams>,
    json: Result<Json<SignBody>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    check_object_name(&name)?;
    let ttl = ttl_of(body(json)?.expires_in)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    state.bucket(&data, &at, &bucket, &rid).await?;
    let key = state.urls.key(&rid).await?;
    let exp = now() + ttl as u64;
    let claims = Claims {
        purpose: Purpose::Upload,
        database_id: at.database_id,
        branch_id: data.branch_id,
        database: at.database,
        datoid: None,
        key_id: data.key_id,
        bucket,
        name,
        version_id: None,
        size: None,
        etag: None,
        content_type: None,
        token_id: Uuid::new_v4(),
        exp,
    };
    let recorded = state
        .urls
        .control()
        .record_upload(claims.token_id, data.key_id, &data.claims, exp)
        .await
        .map_err(|e| crate::control::unreachable(e, &rid))?;
    if !recorded {
        return Err(Failure::unauthorized("the key isn't live"));
    }
    let token = token::sign(key, &claims);
    let url = signed_url("upload/sign", &claims.bucket, &claims.name, &token);
    Ok(Json(json!({ "signed_url": url, "expires_at": exp })).into_response())
}

#[utoipa::path(
    put,
    path = "/storage/v1/object/upload/sign/{bucket}/{*name}",
    operation_id = "signedUpload",
    tag = "storage",
    summary = "Upload through a one-time URL",
    description = "No key: the token is the credential, once. Streams the body as an upload by the URL's minter.",
    params(("bucket" = String, Path), ("name" = String, Path), SignedParams),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Stored: `bucket`, `name`, `version_id`, `size`, `etag`", body = Value),
        (status = 401, description = "`invalid_token`, `token_expired`", body = StorageErrorEnvelope),
        (status = 403, description = "`token_not_valid`: used already, another object's, or its key was revoked; `not_allowed`", body = StorageErrorEnvelope),
        (status = 409, description = "`conflict`", body = StorageErrorEnvelope),
        (status = 408, description = "`request_timeout`: the body sent nothing for `CHELABASE_STORAGE_BODY_IDLE_SECS` (60 s by default)", body = StorageErrorEnvelope),
        (status = 413, description = "`too_large`", body = StorageErrorEnvelope),
    ),
)]
pub async fn signed_upload<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((bucket, name)): Path<(String, String)>,
    Query(q): Query<SignedParams>,
    request: Body,
) -> Result<Response, Failure> {
    let rid = request_id(&headers);
    let claims = state
        .open_token(q.token.as_deref(), Purpose::Upload, &bucket, &name, &rid)
        .await?;
    // Spent before the body is read: a second use (any replica) is refused here.
    let spent = state
        .urls
        .control()
        .consume_upload(claims.token_id)
        .await
        .map_err(|e| crate::control::unreachable(e, &rid))?;
    let Some((key, minted)) = spent.filter(|(k, _)| key_still_reaches(k, &claims)) else {
        return Err(Failure::token_refused());
    };
    let data = key.identity(minted);
    let params = ObjectParams {
        database: Some(claims.database.clone()),
        upsert: q.upsert,
        download: None,
    };
    upload_as(
        &state,
        &data,
        &rid,
        &headers,
        (bucket, name),
        params,
        request,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC: a TTL over 7 days (or not positive) is 400; the default is an hour.
    #[test]
    fn a_ttl_is_one_second_to_seven_days() {
        assert_eq!(ttl_of(None).unwrap(), 3600);
        assert_eq!(ttl_of(Some(1)).unwrap(), 1);
        assert_eq!(ttl_of(Some(604_800)).unwrap(), 604_800);
        for bad in [0, -1, 604_801, i64::MAX] {
            assert_eq!(ttl_of(Some(bad)).unwrap_err().0.status.as_u16(), 400);
        }
    }

    /// AC: a genuine token whose key now lives on another branch (or another key's, or one no
    /// longer reaching its database) doesn't open the object.
    #[test]
    fn a_token_needs_its_key_on_its_branch() {
        let claims = Claims {
            purpose: Purpose::Download,
            database_id: Uuid::new_v4(),
            branch_id: Uuid::new_v4(),
            database: "app".into(),
            datoid: Some(1),
            key_id: Uuid::new_v4(),
            bucket: "b".into(),
            name: "n".into(),
            version_id: Some(Uuid::new_v4()),
            size: Some(1),
            etag: None,
            content_type: None,
            token_id: Uuid::new_v4(),
            exp: 10,
        };
        let key = crate::control::LiveKey {
            key_id: claims.key_id,
            branch_id: claims.branch_id,
            kind: chelabase_types::data_key::DataKeyKind::Public,
            role: "anon".into(),
            databases: None,
            role_oid: None,
        };
        assert!(key_still_reaches(&key, &claims));
        let other_branch = crate::control::LiveKey {
            branch_id: Uuid::new_v4(),
            ..key.clone()
        };
        let other_key = crate::control::LiveKey {
            key_id: Uuid::new_v4(),
            ..key.clone()
        };
        let other_list = crate::control::LiveKey {
            databases: Some(vec!["other".into()]),
            ..key.clone()
        };
        for k in [other_branch, other_key, other_list] {
            assert!(!key_still_reaches(&k, &claims), "{k:?}");
        }
        assert_eq!(token_failure(TokenError::Mismatch).0.status.as_u16(), 403);
        assert_eq!(token_failure(TokenError::Invalid).0.status.as_u16(), 401);
        assert_eq!(token_failure(TokenError::Expired).0.status.as_u16(), 401);
    }
}
