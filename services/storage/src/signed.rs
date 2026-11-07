//! Signed object URLs (m6). A caller who can read an object mints a download URL for its
//! current version; one who can see a bucket mints a one-time upload URL for a name. Either
//! is served without a key: the token ([`crate::token`]) is the credential.
//!
//! Serving a download, in order, before any object-store read:
//! 1. the token verifies for this purpose, bucket and name, unexpired (401 or 403);
//! 2. the key that minted it is still live on the token's branch, and still reaches its
//!    database (403; cached [`crate::control::LIVENESS_TTL`]);
//! 3. the token's database is still served on that branch, the same Chelabase database and the
//!    same Postgres database (its OID), and the version the token was signed for is still the
//!    current version of that name there, read as storage itself (404). An overwrite or a
//!    delete ends the URL, and re-creating the name never revives it: a new upload is a new
//!    version. A branch reset or restore puts the branch back where the URL was signed, so the
//!    URL serves again until it expires (decided 2026-11-06), and that holds after an overwrite
//!    too: the overwrite only gave the row a new version. An overwrite or a delete ends a URL
//!    until a reset or restore; only revoking the key that minted it (or the URL expiring) ends
//!    it for good.
//!
//! An upload token is recorded in the control plane when minted and spent there by its one
//! upload (on any replica, across restarts). The token carries the minting identity: the role
//! it ran as (and the role's OID), the app session behind an app user's request, and whether
//! the upload may replace an existing object (the minter's choice; the uploader's `x-upsert`
//! is ignored). The upload runs as that role with the minting request's claims, exactly as
//! m5's upload (row level security decides), only while a key's role is still that key's and
//! an app user's session is still live (the control plane refuses to spend it otherwise, and
//! an app user's URL never outlives the session).

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

use chelabase_auth::internal::DataIdentity;
use serde_json::Value;

use crate::app::{AppState, identity};
use crate::buckets::body;
use crate::control::LiveKey;
use crate::engine::{DATOID_SQL, Located, datoid_of};
use crate::error::Failure;
use crate::names::{self, check_object_name};
use crate::objects::{Found, ObjectParams, pg, serve, upload_as};
use crate::openapi::StorageErrorEnvelope;
use crate::token::{self, Claims, Purpose, TokenError};

/// A URL's default lifetime, and the longest one.
pub const DEFAULT_TTL: i64 = 3600;
pub const MAX_TTL: i64 = chelabase_storage::MAX_SIGNED_URL_TTL.as_secs() as i64;

/// The database's OID and the current version of the name in the bucket, if any (storage's own
/// read: the token's minter could read it, so no policy is asked again).
const SIGNED_ROW: NamedStatement = named_statement!(
    "signed row",
    "SELECT (SELECT oid::pg_catalog.int8 FROM pg_catalog.pg_database WHERE datname OPERATOR(pg_catalog.=) pg_catalog.current_database()), (SELECT o.version_id FROM storage.objects o JOIN storage.buckets b ON b.id OPERATOR(pg_catalog.=) o.bucket_id WHERE b.name OPERATOR(pg_catalog.=) $1 AND o.name OPERATOR(pg_catalog.=) $2)",
    &[Type::TEXT, Type::TEXT]
);

/// A download token still opens its object: the same Postgres database (`oid`) and its
/// version still the name's current one (`current`); else 404.
pub fn still_current(
    claims: &Claims,
    oid: Option<u32>,
    current: Option<Uuid>,
) -> Result<(), Failure> {
    match (claims.datoid, claims.version_id) {
        (Some(datoid), Some(version)) if oid == Some(datoid) && current == Some(version) => {
            Ok(())
        }
        _ => Err(Failure::object_not_found()),
    }
}

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

/// An upload URL's lifetime, and whether its upload may replace an existing object.
#[derive(Deserialize, ToSchema, Default)]
#[serde(deny_unknown_fields, default)]
pub struct SignUploadBody {
    /// Seconds, 1 to 604800 (7 days); default 3600. An app user's URL never outlives the
    /// user's session.
    pub expires_in: Option<i64>,
    /// The upload may replace an existing object (also `?upsert=true` or `x-upsert: true` on
    /// this request). Fixed here: the uploader can't change it.
    pub upsert: Option<bool>,
}

/// `?token=` (and a download's `download=`).
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SignedParams {
    /// The token the signed URL carries.
    pub token: Option<String>,
    /// Download as an attachment; the value is the file name (empty: the object's own).
    pub download: Option<String>,
}

/// The app session behind an app user's request (its claims' `sid`), if any.
pub fn session_of(data: &DataIdentity) -> Result<Option<Uuid>, Failure> {
    match data.claims.get("sid") {
        None => Ok(None),
        Some(sid) => sid
            .as_str()
            .and_then(|s| s.parse().ok())
            .map(Some)
            .ok_or_else(|| Failure::unauthorized("the user token's session isn't readable")),
    }
}

/// Who a spent upload token's upload runs as: the role it was minted with (and its OID) and
/// the minting claims, on the key as it is now. A key's own role must still be the key's role
/// (and OID); an app user's token must name the session its minting claims carry (the control
/// plane checked that session is still live). `None`: refused.
pub fn upload_identity(key: LiveKey, minted: Value, claims: &Claims) -> Option<DataIdentity> {
    let role = claims.role.clone()?;
    match claims.session_id {
        Some(sid) => {
            if minted.get("sid").and_then(Value::as_str) != Some(sid.to_string().as_str()) {
                return None;
            }
            let mut data = key.identity(minted);
            data.role = role;
            data.role_oid = claims.role_oid;
            Some(data)
        }
        None => (key.role == role && key.role_oid == claims.role_oid)
            .then(|| key.identity(minted)),
    }
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
    description = "The caller must be able to read the object (row level security). The URL serves that version of the object without a key while it is the object's current version, until the URL expires or the key is revoked: an overwrite or a delete ends it (404), and re-creating the name never revives it. A branch reset or restore puts the branch back where the URL was signed, so the URL serves again until it expires, after an overwrite as after a delete; only revoking the key that minted it (or the URL expiring) ends it for good. Sign again to link to a new version.",
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
    check_console_ttl(ttl)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let (_, found) = state.find(&data, &at, &bucket, &name, &rid).await?;
    let key = state.urls.key(&rid).await?;
    let exp = now() + ttl as u64;
    let claims = download_claims(&data, &at, bucket, name, found, exp);
    let token = token::sign(key, &claims);
    let url = signed_url("sign", &claims.bucket, &claims.name, &token);
    Ok(Json(json!({ "signed_url": url, "expires_at": exp })).into_response())
}

/// A URL minted from the console lives at most [`crate::internal::MAX_CONSOLE_TTL`] (else 400).
fn check_console_ttl(ttl: i64) -> Result<(), Failure> {
    if crate::internal::console().is_some() && ttl > crate::internal::MAX_CONSOLE_TTL {
        return Err(Failure::invalid_request(
            "a URL minted from the console lives at most 3600 seconds (an hour)",
        ));
    }
    Ok(())
}

/// A download token's claims for the version `found` of `bucket`/`name` in `at`, which `data`
/// was just shown it can read, expiring at `exp`.
fn download_claims(
    data: &DataIdentity,
    at: &Located,
    bucket: String,
    name: String,
    found: Found,
    exp: u64,
) -> Claims {
    Claims {
        purpose: Purpose::Download,
        database_id: at.database_id,
        branch_id: data.branch_id,
        database: at.database.clone(),
        datoid: Some(found.datoid),
        key_id: data.key_id,
        bucket,
        name,
        version_id: Some(found.version),
        size: Some(found.size),
        etag: Some(found.etag),
        content_type: Some(found.content_type),
        // Owner mode (m9) marks its URL: it lives only while its member is still an Admin.
        role: crate::internal::owner_mode().map(|_| crate::internal::STORAGE_ROLE.to_owned()),
        role_oid: None,
        session_id: None,
        upsert: false,
        token_id: Uuid::new_v4(),
        exp,
    }
}

/// The most refs one resolve takes.
pub const MAX_REFS: usize = 1000;

/// A resolve's refs: 0 to [`MAX_REFS`] (else 400).
pub fn check_refs(count: usize) -> Result<(), Failure> {
    if count > MAX_REFS {
        return Err(Failure::invalid_request("refs lists at most 1000 objects"));
    }
    Ok(())
}

/// `refs`, refused while it parses at the first ref over [`MAX_REFS`] (m11 audit N2): a body
/// up to the JSON limit isn't parsed whole before the count is checked.
fn capped_refs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<ObjectRef>, D::Error> {
    struct Capped;
    impl<'de> serde::de::Visitor<'de> for Capped {
        type Value = Vec<ObjectRef>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a list of at most {MAX_REFS} refs")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut refs = Vec::new();
            while let Some(r) = seq.next_element()? {
                if refs.len() == MAX_REFS {
                    return Err(serde::de::Error::custom("refs lists at most 1000 objects"));
                }
                refs.push(r);
            }
            Ok(refs)
        }
    }
    d.deserialize_seq(Capped)
}

/// A public bucket's object's URL on `branch`, naming the database.
pub fn public_url(branch: Uuid, database: &str, bucket: &str, name: &str) -> String {
    format!(
        "/storage/v1/object/public/{branch}/{}/{}?database={}",
        names::encode_path(bucket),
        names::encode_path(name),
        names::encode_path(database)
    )
}

/// The refs' buckets and objects, as the caller (row level security on both tables decides).
/// `$1`..`$4` are the refs side by side (`bucket_id`, `bucket`, `name`, `id`; `NULL` where a
/// ref's shape has no such field); one row per ref the caller can read, numbered by its place
/// (from 1) in the arrays, with the object's own name. A ref whose bucket or object is missing
/// or hidden has no row. Each shape is its own branch, so each joins on an index.
const RESOLVE_SQL: &str = "WITH r AS (SELECT * FROM ROWS FROM (pg_catalog.unnest($1::pg_catalog.uuid[]), pg_catalog.unnest($2::pg_catalog.text[]), pg_catalog.unnest($3::pg_catalog.text[]), pg_catalog.unnest($4::pg_catalog.uuid[])) WITH ORDINALITY AS r(bucket_id, bucket, name, id, ord)) \
SELECT r.ord, b.name, b.public, o.name, o.version_id, o.size, o.etag, o.content_type FROM r JOIN storage.buckets b ON b.id OPERATOR(pg_catalog.=) r.bucket_id JOIN storage.objects o ON o.bucket_id OPERATOR(pg_catalog.=) r.bucket_id AND o.name OPERATOR(pg_catalog.=) r.name WHERE o.version_id IS NOT NULL AND o.size IS NOT NULL \
UNION ALL SELECT r.ord, b.name, b.public, o.name, o.version_id, o.size, o.etag, o.content_type FROM r JOIN storage.buckets b ON b.name OPERATOR(pg_catalog.=) r.bucket JOIN storage.objects o ON o.bucket_id OPERATOR(pg_catalog.=) b.id AND o.name OPERATOR(pg_catalog.=) r.name WHERE o.version_id IS NOT NULL AND o.size IS NOT NULL \
UNION ALL SELECT r.ord, b.name, b.public, o.name, o.version_id, o.size, o.etag, o.content_type FROM r JOIN storage.objects o ON o.id OPERATOR(pg_catalog.=) r.id JOIN storage.buckets b ON b.id OPERATOR(pg_catalog.=) o.bucket_id WHERE o.version_id IS NOT NULL AND o.size IS NOT NULL";

/// An object by its bucket's id and its name: a `storage.object_ref` value.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BucketIdRef {
    pub bucket_id: Uuid,
    pub name: String,
}

/// An object by its bucket's name and its name: a text column marked
/// `@storage_ref bucket=<bucket>` holds the name.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BucketRef {
    pub bucket: String,
    pub name: String,
}

/// An object by its id: a column with a foreign key to `storage.objects(id)`.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IdRef {
    pub id: Uuid,
}

/// One object to resolve, named one of three ways.
#[derive(Deserialize, ToSchema)]
#[serde(untagged)]
pub enum ObjectRef {
    BucketId(BucketIdRef),
    Bucket(BucketRef),
    Id(IdRef),
}

impl ObjectRef {
    /// The ref's columns in [`RESOLVE_SQL`]'s arrays: `(bucket_id, bucket, name, id)`.
    fn columns(&self) -> (Option<Uuid>, Option<String>, Option<String>, Option<Uuid>) {
        match self {
            ObjectRef::BucketId(r) => (Some(r.bucket_id), None, Some(r.name.clone()), None),
            ObjectRef::Bucket(r) => (None, Some(r.bucket.clone()), Some(r.name.clone()), None),
            ObjectRef::Id(r) => (None, None, None, Some(r.id)),
        }
    }

    /// A name no object can have: the ref stays null without being asked about.
    fn unservable(&self) -> bool {
        match self {
            ObjectRef::BucketId(BucketIdRef { name, .. })
            | ObjectRef::Bucket(BucketRef { name, .. }) => check_object_name(name).is_err(),
            ObjectRef::Id(_) => false,
        }
    }
}

/// The refs to resolve, and the signed URLs' lifetime.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveBody {
    /// At most 1000, each `{bucket_id, name}`, `{bucket, name}` or `{id}`.
    #[serde(deserialize_with = "capped_refs")]
    pub refs: Vec<ObjectRef>,
    /// A signed URL's lifetime: seconds, 1 to 604800 (7 days); default 3600.
    #[serde(default)]
    pub expires_in: Option<i64>,
}

#[utoipa::path(
    post,
    path = "/storage/v1/object/resolve",
    operation_id = "resolveObjects",
    tag = "storage",
    summary = "Resolve object refs to URLs",
    description = "For each ref, in order (`{bucket_id, name}` as a `storage.object_ref` holds it, `{bucket, name}` for a text column marked `@storage_ref bucket=`, or `{id}` for a foreign key to `storage.objects(id)`): a signed download URL (exactly as `signObject` mints it) when the caller can read the bucket and the object (row level security on both, as the caller), the public URL instead when the bucket is public, and `null` when the caller can't read it (missing or hidden alike) or the name isn't a valid object name. All refs are read in one statement.",
    params(ObjectParams),
    request_body = ResolveBody,
    security(("bearer" = [])),
    responses(
        (status = 200, description = "`urls`: one per ref, in order, each a path on the API host or `null`; `expires_at`: the signed URLs' expiry (seconds since the epoch)", body = Value),
        (status = 400, description = "`invalid_request`: over 1000 refs, a malformed ref, or `expires_in` outside 1 to 604800", body = StorageErrorEnvelope),
        (status = 403, description = "`not_allowed`, `database_not_allowed`", body = StorageErrorEnvelope),
    ),
)]
pub async fn resolve<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Query(q): Query<ObjectParams>,
    json: Result<Json<ResolveBody>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let asked = body(json)?;
    let refs = asked.refs;
    check_refs(refs.len())?;
    let ttl = ttl_of(asked.expires_in)?;
    check_console_ttl(ttl)?;
    let exp = now() + ttl as u64;
    // The database first, even for a batch with nothing to ask (m11 audit N1): one the key
    // may not use is 403 whatever the refs are.
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let mut urls: Vec<Option<String>> = vec![None; refs.len()];
    let sent: Vec<usize> = (0..refs.len()).filter(|&i| !refs[i].unservable()).collect();
    if sent.is_empty() {
        return Ok(Json(json!({ "urls": urls, "expires_at": exp })).into_response());
    }
    let (mut bucket_ids, mut buckets, mut names, mut ids) = (vec![], vec![], vec![], vec![]);
    for &i in &sent {
        let (bucket_id, bucket, name, id) = refs[i].columns();
        bucket_ids.push(bucket_id);
        buckets.push(bucket);
        names.push(name);
        ids.push(id);
    }
    // The OID in the same transaction as the rows: it decides the blobs' keys, as in `find`.
    let ((rows, oid), _) = state
        .caller(&data, &at, &rid, async |c, _| {
            let rows = c
                .query(RESOLVE_SQL, &[&bucket_ids, &buckets, &names, &ids])
                .await
                .map_err(pg(&rid))?;
            let oid: i64 = c.query_one(DATOID_SQL, &[]).await.map_err(pg(&rid))?.get(0);
            Ok((rows, oid))
        })
        .await?;
    if rows.is_empty() {
        return Ok(Json(json!({ "urls": urls, "expires_at": exp })).into_response());
    }
    let datoid = datoid_of(Some(oid)).ok_or_else(Failure::storage_unavailable)?;
    let key = state.urls.key(&rid).await?;
    for row in rows {
        let ord: i64 = row.get(0);
        let Some(&i) = usize::try_from(ord - 1).ok().and_then(|k| sent.get(k)) else {
            continue;
        };
        let bucket: String = row.get(1);
        // The object's own name (an id ref names none): one no URL can carry stays null.
        let name: String = row.get(3);
        if check_object_name(&name).is_err() {
            continue;
        }
        let url = if row.get::<_, Option<bool>>(2).unwrap_or(false) {
            public_url(data.branch_id, &at.database, &bucket, &name)
        } else {
            let found = Found {
                datoid,
                version: row.get(4),
                size: row.get(5),
                etag: row.get::<_, Option<String>>(6).unwrap_or_default(),
                content_type: row
                    .get::<_, Option<String>>(7)
                    .unwrap_or_else(|| "application/octet-stream".into()),
                owner: None,
            };
            let claims = download_claims(&data, &at, bucket, name, found, exp);
            signed_url(
                "sign",
                &claims.bucket,
                &claims.name,
                &token::sign(key, &claims),
            )
        };
        urls[i] = Some(url);
    }
    Ok(Json(json!({ "urls": urls, "expires_at": exp })).into_response())
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
        (status = 404, description = "`object_not_found`: overwritten or deleted since (the signed version is no longer current; a branch reset or restore that makes it current again serves it again)", body = StorageErrorEnvelope),
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
    let owner = claims.role.as_deref() == Some(crate::internal::STORAGE_ROLE);
    let live = state
        .urls
        .live_key(claims.key_id, claims.branch_id, owner, &rid)
        .await?;
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
    still_current(&claims, datoid_of(row.get(0)), row.get::<_, Option<Uuid>>(1))?;
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
    description = "The caller must be able to see the bucket. The URL takes one upload (`PUT`) without a key, as the caller (its role and claims): row level security decides then. Whether it may replace an existing object is fixed here (`upsert`). An app user's URL never outlives the user's session and fails once the session ends.",
    params(("bucket" = String, Path), ("name" = String, Path), ObjectParams),
    request_body = SignUploadBody,
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
    json: Result<Json<SignUploadBody>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    check_object_name(&name)?;
    let asked = body(json)?;
    let ttl = ttl_of(asked.expires_in)?;
    let upsert = asked.upsert.unwrap_or(false)
        || q.upsert.unwrap_or(false)
        || headers.get("x-upsert").and_then(|v| v.to_str().ok()) == Some("true");
    let session_id = session_of(&data)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    state.bucket(&data, &at, &bucket, &rid).await?;
    let key = state.urls.key(&rid).await?;
    let token_id = Uuid::new_v4();
    let exp = state
        .urls
        .control()
        .record_upload(token_id, data.key_id, &data.claims, now() + ttl as u64)
        .await
        .map_err(|e| crate::control::unreachable(e, &rid))?
        .ok_or_else(|| Failure::unauthorized("the key or the user's session isn't live"))?;
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
        role: Some(data.role.clone()),
        role_oid: data.role_oid,
        session_id,
        upsert,
        token_id,
        exp,
    };
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
    description = "No key: the token is the credential, once. Streams the body as an upload by the URL's minter; whether it may replace an existing object is the URL's (`x-upsert` and `?upsert=` are ignored here). `Content-Type` must be one `type/subtype` with unquoted parameters.",
    params(("bucket" = String, Path), ("name" = String, Path), SignedParams),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Stored: `bucket`, `name`, `version_id`, `size`, `etag`", body = Value),
        (status = 400, description = "`invalid_request`: a `Content-Type` that isn't one `type/subtype` with unquoted parameters (refused before the URL is spent, so it takes the corrected upload)", body = StorageErrorEnvelope),
        (status = 401, description = "`invalid_token`, `token_expired`", body = StorageErrorEnvelope),
        (status = 403, description = "`token_not_valid`: used already, another object's, its key was revoked or its role changed, or the app user's session ended; `not_allowed`", body = StorageErrorEnvelope),
        (status = 409, description = "`conflict`", body = StorageErrorEnvelope),
        (status = 408, description = "`request_timeout`: the body sent nothing for `CHELABASE_STORAGE_BODY_IDLE_SECS` (60 s by default)", body = StorageErrorEnvelope),
        (status = 413, description = "`too_large`: over the bucket's file size limit (the URL is spent)", body = StorageErrorEnvelope),
        (status = 415, description = "`mime_not_allowed`: the type isn't on the bucket's `allowed_mime_types` (the URL is spent)", body = StorageErrorEnvelope),
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
    // A malformed Content-Type is refused before the token is spent, so the corrected retry can
    // still use the URL (`upload_as` checks it again, as for any upload). The bucket's limits
    // (415, 413) need the caller's identity, which spending the token gives: they spend it.
    names::check_content_type(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream"),
    )?;
    // Spent before the body is read: a second use (any replica) is refused here.
    let spent = state
        .urls
        .control()
        .consume_upload(claims.token_id)
        .await
        .map_err(|e| crate::control::unreachable(e, &rid))?;
    let data = spent
        .filter(|(k, _)| key_still_reaches(k, &claims))
        .and_then(|(key, minted)| upload_identity(key, minted, &claims))
        .ok_or_else(Failure::token_refused)?;
    let params = ObjectParams {
        database: Some(claims.database.clone()),
        upsert: Some(claims.upsert),
        download: None,
    };
    // The URL's choice only: the uploader's `x-upsert` is dropped.
    let mut headers = headers;
    headers.remove("x-upsert");
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
            role: None,
            role_oid: None,
            session_id: None,
            upsert: false,
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

    /// m6 audit B1, S3: a spent upload token's upload runs as the role it was minted with:
    /// an app user's as `authenticated` with the user's claims (not the public key's `anon`),
    /// only for the session its claims name; a key's only while the key still has that role.
    #[test]
    fn an_upload_runs_as_its_minting_identity() {
        let key = crate::control::LiveKey {
            key_id: Uuid::new_v4(),
            branch_id: Uuid::new_v4(),
            kind: chelabase_types::data_key::DataKeyKind::Public,
            role: "anon".into(),
            databases: None,
            role_oid: None,
        };
        let sid = Uuid::new_v4();
        let user = json!({ "sub": "u1", "sid": sid.to_string(), "role": "authenticated" });
        let claims = Claims {
            purpose: Purpose::Upload,
            database_id: Uuid::new_v4(),
            branch_id: key.branch_id,
            database: "app".into(),
            datoid: None,
            key_id: key.key_id,
            bucket: "b".into(),
            name: "n".into(),
            version_id: None,
            size: None,
            etag: None,
            content_type: None,
            role: Some("authenticated".into()),
            role_oid: None,
            session_id: Some(sid),
            upsert: false,
            token_id: Uuid::new_v4(),
            exp: 10,
        };
        let data = upload_identity(key.clone(), user.clone(), &claims).expect("the user's upload");
        assert_eq!((data.role.as_str(), data.role_oid, &data.claims), ("authenticated", None, &user));
        // Another session's claims, or none, don't fit the token.
        let other = json!({ "sub": "u1", "sid": Uuid::new_v4().to_string() });
        assert!(upload_identity(key.clone(), other, &claims).is_none());
        assert!(upload_identity(key.clone(), json!({}), &claims).is_none());
        // A key's own token: its role (and OID) must still be the key's.
        let own = Claims { role: Some("anon".into()), session_id: None, ..claims.clone() };
        assert_eq!(upload_identity(key.clone(), json!({}), &own).unwrap().role, "anon");
        let renamed = crate::control::LiveKey { role: "other".into(), ..key.clone() };
        assert!(upload_identity(renamed, json!({}), &own).is_none());
        let reoided = crate::control::LiveKey { role_oid: Some(5), ..key.clone() };
        assert!(upload_identity(reoided, json!({}), &own).is_none());
        // A key's token can't claim an app user's role, nor a token minted with no role.
        let lifted = Claims { session_id: None, ..claims.clone() };
        assert!(upload_identity(key.clone(), json!({}), &lifted).is_none());
        assert!(upload_identity(key, json!({}), &Claims { role: None, ..own }).is_none());
    }

    /// #102: a resolve takes 0 to 1000 refs (more is 400, before anything is read).
    #[test]
    fn a_resolve_takes_at_most_a_thousand_refs() {
        assert_eq!(MAX_REFS, 1000);
        for ok in [0, 1, 1000] {
            assert!(check_refs(ok).is_ok(), "{ok}");
        }
        let err = check_refs(1001).unwrap_err();
        assert_eq!(
            (err.0.status.as_u16(), err.0.code),
            (400, "invalid_request")
        );
    }

    /// m11 audit N2: the cap holds while the body parses. A 1001st ref fails there, before
    /// the rest of the body (here, not even JSON) is read.
    #[test]
    fn the_ref_cap_holds_while_parsing() {
        let one = r#"{"id":"00000000-0000-0000-0000-0000000000a1"}"#;
        let at_cap = format!(r#"{{"refs":[{}]}}"#, vec![one; MAX_REFS].join(","));
        assert_eq!(serde_json::from_str::<ResolveBody>(&at_cap).unwrap().refs.len(), MAX_REFS);
        let over = format!(r#"{{"refs":[{},{one},{one}, this is not JSON"#, vec![one; MAX_REFS].join(","));
        let err = serde_json::from_str::<ResolveBody>(&over).err().expect("over the cap");
        assert!(err.to_string().contains("at most 1000"), "{err}");
    }

    /// #102: a ref names an object one way: `{bucket_id, name}` (a `storage.object_ref`),
    /// `{bucket, name}` (a text column's bucket and name) or `{id}` (a foreign key to
    /// `storage.objects(id)`); a mix, an unknown field or a partial ref is 400.
    #[test]
    fn a_ref_names_an_object_one_way() {
        let b = "00000000-0000-0000-0000-0000000000b1";
        let refs = |refs: Value| serde_json::from_value::<ResolveBody>(json!({ "refs": refs }));
        let got = refs(json!([
            { "bucket_id": b, "name": "a.png" },
            { "bucket": "avatars", "name": "u1/me.png" },
            { "id": b },
        ]))
        .unwrap()
        .refs;
        assert!(
            matches!(&got[0], ObjectRef::BucketId(r) if r.bucket_id.to_string() == b && r.name == "a.png")
        );
        assert!(
            matches!(&got[1], ObjectRef::Bucket(r) if r.bucket == "avatars" && r.name == "u1/me.png")
        );
        assert!(matches!(&got[2], ObjectRef::Id(r) if r.id.to_string() == b));
        for bad in [
            json!([{ "bucket_id": b, "name": "a", "id": b }]),
            json!([{ "bucket": "x", "bucket_id": b, "name": "a" }]),
            json!([{ "name": "a" }]),
            json!([{ "bucket": "x" }]),
            json!([{ "id": "nope" }]),
            json!([{ "id": b, "extra": 1 }]),
        ] {
            assert!(refs(bad.clone()).is_err(), "{bad}");
        }
    }

    /// #102: a readable object in a public bucket resolves to its public URL on the branch,
    /// naming the database; the path's segments are encoded as a signed URL's are.
    #[test]
    fn a_public_object_resolves_to_its_public_url() {
        let branch = Uuid::parse_str("6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10").unwrap();
        assert_eq!(
            public_url(branch, "my db", "pub", "logos/a b.png"),
            "/storage/v1/object/public/6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10/pub/logos/a%20b.png?database=my%20db"
        );
    }

    /// An app user's request names its session (`sid`); a key's doesn't; an unreadable one
    /// mints nothing.
    #[test]
    fn a_session_is_read_from_the_claims() {
        let sid = Uuid::new_v4();
        let mut data = DataIdentity {
            branch_id: Uuid::new_v4(),
            key_id: Uuid::new_v4(),
            kind: chelabase_types::data_key::DataKeyKind::Public,
            role: "authenticated".into(),
            claims: json!({ "sid": sid.to_string() }),
            databases: None,
            project_id: None,
            role_oid: None,
            server_vouched: false,
        };
        assert_eq!(session_of(&data).unwrap(), Some(sid));
        data.claims = json!({});
        assert_eq!(session_of(&data).unwrap(), None);
        data.claims = json!({ "sid": 7 });
        assert_eq!(session_of(&data).unwrap_err().0.status.as_u16(), 401);
    }
}
