//! Resumable uploads (`/storage/v1/upload`): a session in `storage.upload_sessions` holds one S3
//! multipart upload straight into the object's version key.
//!
//! - **Create** (`POST /upload/{bucket}/{*name}`): the declared type against the bucket's list
//!   (415), m5's pre-check as the caller (the INSERT, or upsert, in a rolled-back savepoint:
//!   403), then the S3 upload and the session row, written as storage itself and bound to the
//!   caller's role, `sub` and data key, the database's OID and (by `guard_sessions`) the timeline.
//! - **Part** (`PUT /upload/session/{id}/{n}`): 5 MiB to 100 MiB (the last may be smaller),
//!   counted as it arrives and held to the bucket's limit less the other parts' bytes (413);
//!   part 1's first bytes are sniffed (415). The part's buffer is reserved from the process's
//!   [`crate::limits::InFlight`] cap first (503 `busy`). A resent part replaces the earlier one.
//! - **Complete** (`POST /upload/session/{id}/complete`): parts 1..n with no gap, the total
//!   against the bucket's current limit (413, the S3 upload aborted), the pre-check again
//!   (403, aborted), then the session is claimed (its row deleted: a losing concurrent
//!   completion is 409), the S3 upload completed and m5's ticket flow writes the row as the
//!   caller (RLS decides once more; a refused row deletes the blob).
//! - **Abort** (`DELETE /upload/session/{id}`) and **list** (`GET`).
//!
//! A session is the caller's only: another role, `sub` or data key, another timeline (a branch copies
//! the row) or another database answers 404 `upload_not_found`, as a missing one. After 24 hours
//! it answers 410 and its S3 upload is aborted; m8's sweeper deletes the expired rows and any
//! multipart upload no live session names.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use chelabase_auth::internal::DataIdentity;
use chelabase_pgsession::service::Type;
use chelabase_pgsession::{ConnSource, NamedStatement, named_statement};
use chelabase_storage::{CompletedPart, StorageError};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::IntoParams;
use uuid::Uuid;

use crate::app::{AppState, identity};
use crate::engine::{Located, datoid_of};
use crate::error::Failure;
use crate::limits::{Limits, SNIFF_BYTES};
use crate::names::{self, check_object_name};
use crate::objects::{ObjectParams, Row, owner_of};
use crate::openapi::StorageErrorEnvelope;
use crate::signed::now;

/// A part's smallest size (S3's), but for the last.
pub const MIN_PART: u64 = 5 << 20;
/// A part's largest size: one part is buffered whole.
pub const MAX_PART: u64 = 100 << 20;
/// S3's part numbers.
pub const MAX_PARTS: u32 = 10_000;
/// A session's lifetime.
pub const SESSION_TTL_SECS: u64 = 24 * 3600;

/// A part as the session row records it (`parts` jsonb: `{"<n>": {"size", "etag"}}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub size: u64,
    pub etag: String,
}

/// What a session row holds that the routes decide on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub parts: BTreeMap<u32, Part>,
    /// Seconds since the epoch.
    pub expires_at: u64,
}

impl Session {
    /// 410 once `now` reaches its expiry.
    pub fn admit(&self, now: u64) -> Result<(), Failure> {
        if now >= self.expires_at {
            Err(Failure::upload_expired())
        } else {
            Ok(())
        }
    }

    /// The most bytes part `number` may have: [`MAX_PART`], and the bucket's `limit` less the
    /// other parts' bytes (the part it replaces doesn't count). 400 for a number outside
    /// 1..=[`MAX_PARTS`].
    pub fn budget(&self, number: u32, limit: Option<u64>) -> Result<u64, Failure> {
        if !(1..=MAX_PARTS).contains(&number) {
            return Err(Failure::invalid_parts(format!(
                "a part number is 1 to {MAX_PARTS}"
            )));
        }
        let others: u64 = self
            .parts
            .iter()
            .filter(|(n, _)| **n != number)
            .map(|(_, p)| p.size)
            .sum();
        Ok(limit.map_or(MAX_PART, |l| l.saturating_sub(others).min(MAX_PART)))
    }

    /// Records part `number` of `size` counted bytes, as the part route does once the store
    /// has it: refused (and not recorded) when expired or over [`Session::budget`].
    pub fn accept(
        &mut self,
        number: u32,
        part: Part,
        limit: Option<u64>,
        now: u64,
    ) -> Result<(), Failure> {
        self.admit(now)?;
        let budget = self.budget(number, limit)?;
        if part.size > budget {
            return Err(over(budget, limit));
        }
        self.parts.insert(number, part);
        Ok(())
    }

    /// The completion: the parts in order and the total, when they make an object within
    /// `limit`: parts 1..n with no gap, each but the last at least [`MIN_PART`]. 410 once
    /// expired, 400 `invalid_parts`, 413 over the limit.
    pub fn plan(&self, limit: Option<u64>, now: u64) -> Result<(Vec<CompletedPart>, u64), Failure> {
        self.admit(now)?;
        let count = self.parts.len() as u32;
        if count == 0 || self.parts.keys().copied().ne(1..=count) {
            return Err(Failure::invalid_parts(
                "the parts are numbered 1 to n with none missing",
            ));
        }
        if self
            .parts
            .iter()
            .rev()
            .skip(1)
            .any(|(_, p)| p.size < MIN_PART)
        {
            return Err(Failure::invalid_parts(
                "every part but the last is at least 5 MiB",
            ));
        }
        let total: u64 = self.parts.values().map(|p| p.size).sum();
        if let Some(l) = limit
            && total > l
        {
            return Err(Failure::too_large(l as i64));
        }
        let parts = self
            .parts
            .iter()
            .map(|(n, p)| CompletedPart {
                part_number: *n,
                etag: p.etag.clone(),
            })
            .collect();
        Ok((parts, total))
    }
}

/// A part over its budget: the bucket's limit when that is what's left, else [`MAX_PART`].
fn over(budget: u64, limit: Option<u64>) -> Failure {
    match limit {
        Some(l) if budget < MAX_PART => Failure::too_large(l as i64),
        _ => Failure::too_large(MAX_PART as i64),
    }
}

/// A session row, written as storage itself; `timeline_id` is stamped by `guard_sessions`.
const OPEN: NamedStatement = named_statement!(
    "open upload",
    "INSERT INTO storage.upload_sessions (id, bucket_id, name, version_id, upload_id, content_type, upsert, datoid, caller_role, owner, key_id, parts, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::pg_catalog.name, $10, $11, '{}'::pg_catalog.jsonb, pg_catalog.clock_timestamp() OPERATOR(pg_catalog.+) interval '24 hours') RETURNING pg_catalog.date_part('epoch', expires_at)::pg_catalog.int8",
    &[
        Type::UUID,
        Type::UUID,
        Type::TEXT,
        Type::UUID,
        Type::TEXT,
        Type::TEXT,
        Type::BOOL,
        Type::INT8,
        Type::TEXT,
        Type::UUID,
        Type::UUID
    ]
);

/// The caller's session (its id, role, owner and key, this timeline) with its bucket's limits.
const SESSION: NamedStatement = named_statement!(
    "upload session",
    "SELECT s.bucket_id, s.name, s.version_id, s.upload_id, s.content_type, s.upsert, s.datoid, s.parts::pg_catalog.text, pg_catalog.date_part('epoch', s.expires_at)::pg_catalog.int8, b.name, b.file_size_limit, b.allowed_mime_types FROM storage.upload_sessions s JOIN storage.buckets b ON b.id OPERATOR(pg_catalog.=) s.bucket_id WHERE s.id OPERATOR(pg_catalog.=) $1 AND s.caller_role OPERATOR(pg_catalog.=) $2::pg_catalog.name AND s.owner IS NOT DISTINCT FROM $3 AND s.key_id OPERATOR(pg_catalog.=) $4 AND s.timeline_id OPERATOR(pg_catalog.=) pg_catalog.current_setting('neon.timeline_id', true)",
    &[Type::UUID, Type::TEXT, Type::UUID, Type::UUID]
);

/// Records a part (a resent one replaces the earlier) through `storage.record_part`: true,
/// or NULL once the session expired.
const RECORD: NamedStatement = named_statement!(
    "record part",
    "SELECT storage.record_part($1, $2, $3, $4)",
    &[Type::UUID, Type::INT4, Type::INT8, Type::TEXT]
);

/// A completion's claim: the session's row, deleted by exactly one completion.
const CLAIM: NamedStatement = named_statement!(
    "claim upload",
    "DELETE FROM storage.upload_sessions WHERE id OPERATOR(pg_catalog.=) $1 RETURNING id",
    &[Type::UUID]
);

const CLOSE: NamedStatement = named_statement!(
    "close upload",
    "DELETE FROM storage.upload_sessions WHERE id OPERATOR(pg_catalog.=) $1",
    &[Type::UUID]
);

/// Who a session belongs to: the caller's role, `sub` and data key. A lookup matches all
/// three ([`SESSION`]), so anyone else's id is 404.
#[derive(Debug, PartialEq, Eq)]
struct Binding<'a> {
    role: &'a str,
    owner: Option<Uuid>,
    key_id: Uuid,
}

fn binding(data: &DataIdentity) -> Binding<'_> {
    Binding {
        role: &data.role,
        owner: owner_of(data),
        key_id: data.key_id,
    }
}

/// The completion's claim on its session ([`CLAIM`]): only the one completion that deleted
/// the row goes on; a losing concurrent one is 409, never a second S3 completion (which S3
/// may accept, and whose refused row would delete the winner's blob).
fn claimed(rows: usize) -> Result<(), Failure> {
    if rows == 0 {
        Err(Failure::object_conflict())
    } else {
        Ok(())
    }
}

/// The S3 completion's failure, after the claim: the upload gone means another completion
/// (or an abort) took it: 409.
fn completion_failure(e: &StorageError, rid: &str) -> Failure {
    match e {
        StorageError::NotFound => Failure::object_conflict(),
        other => Failure::from_store(other, rid),
    }
}

/// A session as the routes use it.
struct Loaded {
    id: Uuid,
    session: Session,
    bucket: String,
    limits: Limits,
    name: String,
    version: Uuid,
    upload_id: String,
    content_type: String,
    upsert: bool,
    datoid: u32,
    /// The version's blob key, the S3 upload's.
    key: String,
}

/// The store's failure on a session's upload: the upload gone (completed or aborted
/// meanwhile) is the session's 404.
fn store_failure(e: &StorageError, rid: &str) -> Failure {
    match e {
        StorageError::NotFound => Failure::upload_not_found(),
        other => Failure::from_store(other, rid),
    }
}

impl<S: ConnSource, M: ConnSource> AppState<S, M> {
    /// The caller's session `id` in the located database; 404 for anyone else's.
    async fn session(
        &self,
        data: &DataIdentity,
        at: &Located,
        id: &str,
        rid: &str,
    ) -> Result<Loaded, Failure> {
        let id: Uuid = id.parse().map_err(|_| Failure::upload_not_found())?;
        let who = binding(data);
        let rows = self
            .engine
            .managed(
                data.branch_id,
                &at.database,
                &SESSION,
                &[&id, &who.role, &who.owner, &who.key_id],
                rid,
            )
            .await?;
        let r = rows.first().ok_or_else(Failure::upload_not_found)?;
        let parts = serde_json::from_str(r.get::<_, &str>(7)).map_err(|e| {
            tracing::warn!(request_id = rid, "an upload session's parts: {e}");
            Failure::storage_unavailable()
        })?;
        let datoid = datoid_of(Some(r.get(6))).ok_or_else(Failure::storage_unavailable)?;
        let version: Uuid = r.get(2);
        Ok(Loaded {
            id,
            session: Session {
                parts,
                expires_at: r.get::<_, i64>(8).max(0) as u64,
            },
            bucket: r.get(9),
            limits: Limits {
                id: r.get(0),
                file_size_limit: r.get(10),
                allowed_mime_types: r.get(11),
            },
            name: r.get(1),
            version,
            upload_id: r.get(3),
            content_type: r.get(4),
            upsert: r.get(5),
            datoid,
            key: names::version_key(&names::object_prefix(at.database_id, datoid), version),
        })
    }

    /// `s` admitted now; an expired one's S3 upload is aborted (its row stays, answering 410,
    /// until m8's sweeper deletes it).
    async fn admit(&self, s: &Loaded, rid: &str) -> Result<(), Failure> {
        let out = s.session.admit(now());
        if out.is_err() {
            self.abort_upload(s, rid).await;
        }
        out
    }

    /// Aborts `s`'s S3 upload (best effort: one already gone is fine; a leftover is the
    /// sweeper's).
    async fn abort_upload(&self, s: &Loaded, rid: &str) {
        match self.store.multipart_abort(&s.key, &s.upload_id).await {
            Ok(()) | Err(StorageError::NotFound) => {}
            Err(e) => tracing::warn!(request_id = rid, "aborting an upload: {e}"),
        }
    }

    /// Ends `s`: its S3 upload aborted and its row deleted.
    async fn close(&self, data: &DataIdentity, at: &Located, s: &Loaded, rid: &str) {
        self.abort_upload(s, rid).await;
        if let Err(f) = self
            .engine
            .managed(data.branch_id, &at.database, &CLOSE, &[&s.id], rid)
            .await
        {
            tracing::warn!(
                request_id = rid,
                "closing an upload session: {}",
                f.0.message
            );
        }
    }
}

/// `?database=`.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SessionParams {
    /// The Postgres database (the key's main one when left out).
    pub database: Option<String>,
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

#[utoipa::path(
    post,
    path = "/storage/v1/upload/{bucket}/{*name}",
    operation_id = "createUpload",
    tag = "storage",
    summary = "Start a resumable upload",
    description = "Checks the declared `Content-Type` against the bucket's `allowed_mime_types` and the caller's INSERT (or, with `upsert`, its UPDATE) policies on `storage.objects`, then opens a session for 24 hours. Upload parts of 5 MiB to 100 MiB (the last may be smaller), then complete it. The body is empty.",
    params(("bucket" = String, Path), ("name" = String, Path, description = "The object's name, as for an upload"), ObjectParams),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "`id` (the session), `bucket`, `name`, `expires_at` (Unix seconds), `min_part_size`, `max_part_size`", body = Value),
        (status = 403, description = "`not_allowed`: no policy allows the write, or the caller can't see the bucket", body = StorageErrorEnvelope),
        (status = 409, description = "`conflict`: the name is taken (no upsert); `write_once`: an upsert over an object of a write-once bucket", body = StorageErrorEnvelope),
        (status = 415, description = "`mime_not_allowed`", body = StorageErrorEnvelope),
        (status = 503, description = "`storage_unavailable`, `unavailable`", body = StorageErrorEnvelope),
    ),
)]
pub async fn create<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((bucket, name)): Path<(String, String)>,
    Query(q): Query<ObjectParams>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    check_object_name(&name)?;
    let upsert = q.upsert.unwrap_or(false)
        || headers.get("x-upsert").and_then(|v| v.to_str().ok()) == Some("true");
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let limits = state.limits(&data, &at, &bucket, &rid).await?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    limits.check_declared(&content_type)?;
    let row = Row {
        bucket: limits.id,
        name,
        owner: owner_of(&data),
        size: None,
        content_type,
        etag: None,
        version: Uuid::new_v4(),
        datoid: 0,
    };
    let (_, datoid) = state.precheck(&data, &at, &row, upsert, &rid).await?;
    let key = names::version_key(&names::object_prefix(at.database_id, datoid), row.version);
    let upload_id = state
        .store
        .multipart_create(&key, Some(&row.content_type))
        .await
        .map_err(|e| Failure::from_store(&e, &rid))?;
    let id = Uuid::new_v4();
    let opened = state
        .engine
        .managed(
            data.branch_id,
            &at.database,
            &OPEN,
            &[
                &id,
                &row.bucket,
                &row.name,
                &row.version,
                &upload_id,
                &row.content_type,
                &upsert,
                &i64::from(datoid),
                &data.role,
                &row.owner,
                &data.key_id,
            ],
            &rid,
        )
        .await;
    let rows = match opened {
        Ok(rows) => rows,
        Err(f) => {
            let _ = state.store.multipart_abort(&key, &upload_id).await;
            return Err(f);
        }
    };
    let expires_at: i64 = rows.first().map_or(0, |r| r.get(0));
    Ok(Json(json!({
        "id": id, "bucket": bucket, "name": row.name, "expires_at": expires_at,
        "min_part_size": MIN_PART, "max_part_size": MAX_PART,
    }))
    .into_response())
}

#[utoipa::path(
    put,
    path = "/storage/v1/upload/session/{session}/{part}",
    operation_id = "uploadPart",
    tag = "storage",
    summary = "Upload one part",
    description = "Part numbers are 1 to 10000; a part is 5 MiB to 100 MiB, but the last may be smaller. Sending a part again replaces it. Its bytes are counted as they arrive and held to the bucket's `file_size_limit` less the other parts'. Part 1's first bytes must be a type the bucket allows.",
    params(("session" = String, Path), ("part" = u32, Path), SessionParams),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "`part_number`, `size`, `etag`", body = Value),
        (status = 404, description = "`upload_not_found`: missing, finished, or another caller's", body = StorageErrorEnvelope),
        (status = 410, description = "`upload_expired`", body = StorageErrorEnvelope),
        (status = 413, description = "`too_large`: over 100 MiB or the bucket's limit", body = StorageErrorEnvelope),
        (status = 415, description = "`mime_not_allowed`", body = StorageErrorEnvelope),
        (status = 503, description = "`busy`: too many uploads in flight; `storage_unavailable`", body = StorageErrorEnvelope),
    ),
)]
pub async fn upload_part<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path((id, part)): Path<(String, String)>,
    Query(q): Query<SessionParams>,
    request: Body,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let part: u32 = part
        .parse()
        .map_err(|_| Failure::invalid_parts(format!("a part number is 1 to {MAX_PARTS}")))?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let s = state.session(&data, &at, &id, &rid).await?;
    state.admit(&s, &rid).await?;
    let limit = s.limits.max_bytes();
    let budget = s.session.budget(part, limit)?;
    let declared = content_length(&headers);
    if declared.is_some_and(|d| d > budget) {
        return Err(over(budget, limit));
    }
    // The whole part is held: its bytes come out of the process's cap first (503 `busy`).
    let _buffer = state
        .inflight
        .reserve(declared.unwrap_or(budget).min(budget))?;
    let mut buf = BytesMut::new();
    let mut body = request.into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk =
            chunk.map_err(|e| Failure::invalid_request(format!("the request body: {e}")))?;
        if (buf.len() + chunk.len()) as u64 > budget {
            return Err(over(budget, limit));
        }
        buf.extend_from_slice(&chunk);
    }
    if part == 1 {
        s.limits.check_sniffed(&buf[..buf.len().min(SNIFF_BYTES)])?;
    }
    let size = buf.len() as u64;
    let etag = state
        .store
        .multipart_part(&s.key, &s.upload_id, part, buf.freeze())
        .await
        .map_err(|e| store_failure(&e, &rid))?;
    let mut session = s.session.clone();
    session.accept(
        part,
        Part {
            size,
            etag: etag.clone(),
        },
        limit,
        now(),
    )?;
    let recorded = state
        .engine
        .managed(
            data.branch_id,
            &at.database,
            &RECORD,
            &[&s.id, &(part as i32), &(size as i64), &etag],
            &rid,
        )
        .await?;
    if recorded.first().and_then(|r| r.get::<_, Option<bool>>(0)) != Some(true) {
        return Err(Failure::upload_expired());
    }
    Ok(Json(json!({ "part_number": part, "size": size, "etag": etag })).into_response())
}

#[utoipa::path(
    get,
    path = "/storage/v1/upload/session/{session}",
    operation_id = "listParts",
    tag = "storage",
    summary = "The parts received",
    params(("session" = String, Path), SessionParams),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "`bucket`, `name`, `expires_at`, `parts` (`part_number`, `size`, `etag`)", body = Value),
        (status = 404, description = "`upload_not_found`", body = StorageErrorEnvelope),
        (status = 410, description = "`upload_expired`", body = StorageErrorEnvelope),
    ),
)]
pub async fn list_parts<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<SessionParams>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let s = state.session(&data, &at, &id, &rid).await?;
    state.admit(&s, &rid).await?;
    let parts: Vec<Value> = s
        .session
        .parts
        .iter()
        .map(|(n, p)| json!({ "part_number": n, "size": p.size, "etag": p.etag }))
        .collect();
    Ok(Json(json!({
        "bucket": s.bucket, "name": s.name, "expires_at": s.session.expires_at, "parts": parts,
    }))
    .into_response())
}

#[utoipa::path(
    delete,
    path = "/storage/v1/upload/session/{session}",
    operation_id = "abortUpload",
    tag = "storage",
    summary = "Abort a resumable upload",
    params(("session" = String, Path), SessionParams),
    security(("bearer" = [])),
    responses((status = 204, description = "Aborted: its parts are gone"), (status = 404, description = "`upload_not_found`", body = StorageErrorEnvelope)),
)]
pub async fn abort<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<SessionParams>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let s = state.session(&data, &at, &id, &rid).await?;
    state.close(&data, &at, &s, &rid).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[utoipa::path(
    post,
    path = "/storage/v1/upload/session/{session}/complete",
    operation_id = "completeUpload",
    tag = "storage",
    summary = "Complete a resumable upload",
    description = "Parts 1 to n with none missing make the object, atomically: the total is held to the bucket's current `file_size_limit`, and the caller's policies on `storage.objects` decide again before and as the row is written. A refusal (403, 413, 415) aborts the upload.",
    params(("session" = String, Path), SessionParams),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Stored: `bucket`, `name`, `version_id`, `size`, `etag`", body = Value),
        (status = 400, description = "`invalid_parts`: a gap, or a part under 5 MiB that isn't the last", body = StorageErrorEnvelope),
        (status = 403, description = "`not_allowed`: the policies no longer allow it", body = StorageErrorEnvelope),
        (status = 404, description = "`upload_not_found`", body = StorageErrorEnvelope),
        (status = 409, description = "`conflict`: the name was taken, the object changed meanwhile, or another completion of this upload won; `write_once`: an upsert over an object of a write-once bucket", body = StorageErrorEnvelope),
        (status = 410, description = "`upload_expired`", body = StorageErrorEnvelope),
        (status = 413, description = "`too_large`", body = StorageErrorEnvelope),
        (status = 415, description = "`mime_not_allowed`", body = StorageErrorEnvelope),
    ),
)]
pub async fn complete<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<SessionParams>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let s = state.session(&data, &at, &id, &rid).await?;
    state.admit(&s, &rid).await?;
    // A refusal that no retry of this session can change ends it.
    let terminal = |f: &Failure| matches!(f.0.status.as_u16(), 403 | 413 | 415);
    let checked = s
        .limits
        .check_declared(&s.content_type)
        .and_then(|()| s.session.plan(s.limits.max_bytes(), now()));
    let (parts, total) = match checked {
        Ok(plan) => plan,
        Err(f) => {
            if terminal(&f) {
                state.close(&data, &at, &s, &rid).await;
            }
            return Err(f);
        }
    };
    let mut row = Row {
        bucket: s.limits.id,
        name: s.name.clone(),
        owner: owner_of(&data),
        size: Some(total as i64),
        content_type: s.content_type.clone(),
        etag: None,
        version: s.version,
        datoid: s.datoid,
    };
    // The caller's policies again, before the parts become an object.
    let expected = match state.precheck(&data, &at, &row, s.upsert, &rid).await {
        Ok((_, datoid)) if datoid != s.datoid => Err(Failure::storage_unavailable()),
        other => other.map(|(expected, _)| expected),
    };
    let expected = match expected {
        Ok(e) => e,
        Err(f) => {
            if terminal(&f) {
                state.close(&data, &at, &s, &rid).await;
            }
            return Err(f);
        }
    };
    // Exactly one completion claims the session (its row deleted); a losing concurrent one is
    // 409 and never reaches S3, so it can't delete the winner's blob. From here a failure
    // can't be retried: the upload is aborted.
    let claim = state
        .engine
        .managed(data.branch_id, &at.database, &CLAIM, &[&s.id], &rid)
        .await?;
    claimed(claim.len())?;
    let meta = match state
        .store
        .multipart_complete(&s.key, &s.upload_id, parts)
        .await
    {
        Ok(meta) => meta,
        Err(e) => {
            state.abort_upload(&s, &rid).await;
            return Err(completion_failure(&e, &rid));
        }
    };
    // The S3 upload is gone either way now; the row, a refused or failed one, as m5's.
    let written = names::keep_if_written(state.store.as_ref(), &s.key, async || {
        if meta.size != total {
            tracing::warn!(
                request_id = rid,
                "a completed upload is {} bytes, its parts {total}",
                meta.size
            );
            return Err(Failure::store_failed());
        }
        row.size = Some(meta.size as i64);
        row.etag = Some(meta.etag.trim_matches('"').to_owned());
        state
            .write(&data, &at, &row, expected, s.upsert, &rid)
            .await
    })
    .await;
    written?;
    Ok(Json(json!({
        "bucket": s.bucket, "name": row.name, "version_id": row.version, "size": row.size, "etag": row.etag,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn session() -> Session {
        Session {
            parts: BTreeMap::new(),
            expires_at: SESSION_TTL_SECS,
        }
    }

    fn part(size: u64, tag: &str) -> Part {
        Part {
            size,
            etag: tag.into(),
        }
    }

    /// AC 1: 30 MiB in six 5 MiB parts, part 3 resent: the plan is parts 1..6 in order with
    /// the resent part's etag, 30 MiB in all.
    #[test]
    fn a_resent_part_replaces_the_first() {
        let mut s = session();
        for n in 1..=6 {
            s.accept(n, part(MIN_PART, &format!("e{n}")), None, 0)
                .unwrap();
        }
        s.accept(3, part(MIN_PART, "e3-again"), None, 1).unwrap();
        let (parts, total) = s.plan(None, 2).unwrap();
        assert_eq!(total, 30 << 20);
        let tags: Vec<&str> = parts.iter().map(|p| p.etag.as_str()).collect();
        assert_eq!(tags, ["e1", "e2", "e3-again", "e4", "e5", "e6"]);
        assert_eq!(
            parts.iter().map(|p| p.part_number).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6]
        );
    }

    /// AC 2: parts within the limit one by one, more than it together: the part that goes
    /// over is refused (413), and a plan over a limit lowered meanwhile is 413 too.
    #[test]
    fn the_limit_holds_per_part_and_in_total() {
        let mut s = session();
        let limit = Some(12 << 20);
        s.accept(1, part(MIN_PART, "a"), limit, 0).unwrap();
        s.accept(2, part(MIN_PART, "b"), limit, 0).unwrap();
        assert_eq!(s.budget(3, limit).unwrap(), 2 << 20);
        // Replacing part 2 frees its bytes.
        assert_eq!(s.budget(2, limit).unwrap(), 7 << 20);
        let over = s.accept(3, part(3 << 20, "c"), limit, 0).unwrap_err();
        assert_eq!(over.0.status.as_u16(), 413);
        assert!(!s.parts.contains_key(&3));
        assert_eq!(s.plan(limit, 0).unwrap().1, 10 << 20);
        assert_eq!(s.plan(Some(9 << 20), 0).unwrap_err().0.status.as_u16(), 413);
        assert_eq!(s.budget(1, None).unwrap(), MAX_PART);
        assert_eq!(
            s.accept(4, part(MAX_PART + 1, "d"), None, 0)
                .unwrap_err()
                .0
                .status
                .as_u16(),
            413
        );
    }

    /// Gaps, an empty session, a small part that isn't the last and bad numbers are 400.
    #[test]
    fn the_parts_must_make_an_object() {
        let mut s = session();
        assert_eq!(s.plan(None, 0).unwrap_err().0.code, "invalid_parts");
        s.accept(1, part(MIN_PART, "a"), None, 0).unwrap();
        s.accept(3, part(1, "c"), None, 0).unwrap();
        assert_eq!(s.plan(None, 0).unwrap_err().0.code, "invalid_parts");
        s.accept(2, part(MIN_PART - 1, "b"), None, 0).unwrap();
        assert_eq!(s.plan(None, 0).unwrap_err().0.code, "invalid_parts");
        s.accept(2, part(MIN_PART, "b"), None, 0).unwrap();
        assert_eq!(s.plan(None, 0).unwrap().1, 2 * MIN_PART + 1);
        for bad in [0, MAX_PARTS + 1] {
            assert_eq!(
                s.budget(bad, None).unwrap_err().0.status.as_u16(),
                400,
                "{bad}"
            );
        }
    }

    /// AC 4 (time travel): a session is live until its 24 hours are up, then every part and
    /// the completion are 410 and nothing more is recorded.
    #[test]
    fn an_expired_session_is_gone() {
        let mut s = session();
        s.accept(1, part(1, "a"), None, SESSION_TTL_SECS - 1)
            .unwrap();
        assert!(s.admit(SESSION_TTL_SECS - 1).is_ok());
        for now in [SESSION_TTL_SECS, SESSION_TTL_SECS + 3600] {
            assert_eq!(s.admit(now).unwrap_err().0.status.as_u16(), 410);
            assert_eq!(
                s.accept(2, part(1, "b"), None, now).unwrap_err().0.code,
                "upload_expired"
            );
            assert_eq!(s.plan(None, now).unwrap_err().0.status.as_u16(), 410);
        }
        assert_eq!(s.parts.len(), 1);
    }

    fn identity(role: &str, sub: Option<Uuid>, key_id: Uuid) -> DataIdentity {
        serde_json::from_value(json!({
            "branch_id": Uuid::nil(), "key_id": key_id, "kind": "secret", "role": role,
            "claims": sub.map_or(json!({}), |s| json!({ "sub": s.to_string() })),
        }))
        .unwrap()
    }

    /// AC 9 (review): a session is bound to the caller's role, `sub` and data key: a second
    /// key on the same role, another user or another role each binds differently, and the
    /// lookup matches all three.
    #[test]
    fn a_session_is_bound_to_role_owner_and_key() {
        let (k1, k2, user) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mine = identity("authenticated", Some(user), k1);
        let b = binding(&mine);
        assert_eq!(
            b,
            Binding {
                role: "authenticated",
                owner: Some(user),
                key_id: k1
            }
        );
        for other in [
            identity("authenticated", Some(user), k2),
            identity("authenticated", None, k1),
            identity("authenticated", Some(Uuid::new_v4()), k1),
            identity("anon", Some(user), k1),
        ] {
            assert_ne!(binding(&other), b, "{other:?}");
        }
        let sql = SESSION.sql();
        for part in [
            "s.caller_role OPERATOR(pg_catalog.=) $2::pg_catalog.name",
            "s.owner IS NOT DISTINCT FROM $3",
            "s.key_id OPERATOR(pg_catalog.=) $4",
            "s.timeline_id OPERATOR(pg_catalog.=) pg_catalog.current_setting('neon.timeline_id', true)",
        ] {
            assert!(sql.contains(part), "{part}: {sql}");
        }
        assert!(
            OPEN.sql().contains("caller_role, owner, key_id, parts"),
            "{}",
            OPEN.sql()
        );
    }

    /// Review: of two concurrent completions only the one that claimed the session goes on;
    /// the other is 409 `conflict`, and an upload gone at S3 after the claim is 409 too.
    #[test]
    fn a_losing_concurrent_completion_is_409() {
        assert!(claimed(1).is_ok());
        let lost = claimed(0).unwrap_err();
        assert_eq!((lost.0.status.as_u16(), lost.0.code), (409, "conflict"));
        let gone = completion_failure(&StorageError::NotFound, "r");
        assert_eq!((gone.0.status.as_u16(), gone.0.code), (409, "conflict"));
        let other = completion_failure(&StorageError::Transient("x".into()), "r");
        assert_eq!(other.0.status.as_u16(), 502);
        assert!(
            CLAIM
                .sql()
                .starts_with("DELETE FROM storage.upload_sessions")
                && CLAIM.sql().ends_with("RETURNING id")
        );
    }

    #[derive(Debug, Clone)]
    enum Op {
        Put(u32, u64),
        Wait(u64),
    }

    fn op() -> impl Strategy<Value = Op> {
        let size = prop_oneof![
            Just(1u64),
            Just(MIN_PART - 1),
            Just(MIN_PART),
            Just(MIN_PART + 1),
            Just(MAX_PART),
            Just(MAX_PART + 1),
        ];
        prop_oneof![
            8 => (1u32..7, size).prop_map(|(n, s)| Op::Put(n, s)),
            1 => (0u64..SESSION_TTL_SECS / 2).prop_map(Op::Wait),
        ]
    }

    proptest! {
        /// The session's transitions against a model written from the rules alone: last write
        /// wins per part number, a part fits [`MAX_PART`] and the limit less the others, nothing
        /// is recorded once expired, and a completion is exactly the contiguous parts 1..n
        /// (each but the last at least [`MIN_PART`]) within the limit, in order, with their
        /// sizes summed.
        #[test]
        fn sessions_follow_the_model(
            ops in prop::collection::vec(op(), 0..24),
            limit in prop_oneof![Just(None), (1u64..400 << 20).prop_map(Some)],
        ) {
            let mut real = session();
            let mut model: BTreeMap<u32, (u64, String)> = BTreeMap::new();
            let mut now = 0u64;
            for (i, op) in ops.iter().enumerate() {
                match *op {
                    Op::Wait(secs) => now += secs,
                    Op::Put(n, size) => {
                        let tag = format!("t{i}");
                        let others: u64 = model.iter().filter(|(k, _)| **k != n).map(|(_, v)| v.0).sum();
                        let fits = size <= MAX_PART && limit.is_none_or(|l| others + size <= l);
                        let live = now < SESSION_TTL_SECS;
                        let got = real.accept(n, part(size, &tag), limit, now);
                        prop_assert_eq!(got.is_ok(), live && fits, "{:?} at {}", op, now);
                        if live && fits {
                            model.insert(n, (size, tag));
                        }
                    }
                }
                let keys: Vec<u32> = model.keys().copied().collect();
                let contiguous = !keys.is_empty() && keys.iter().copied().eq(1..=keys.len() as u32);
                let small_inside = model.iter().rev().skip(1).any(|(_, v)| v.0 < MIN_PART);
                let total: u64 = model.values().map(|v| v.0).sum();
                let within = limit.is_none_or(|l| total <= l);
                match real.plan(limit, now) {
                    Ok((parts, sum)) => {
                        prop_assert!(now < SESSION_TTL_SECS && contiguous && !small_inside && within);
                        prop_assert_eq!(sum, total);
                        let want: Vec<CompletedPart> = model.iter()
                            .map(|(k, v)| CompletedPart { part_number: *k, etag: v.1.clone() })
                            .collect();
                        prop_assert_eq!(parts, want);
                    }
                    Err(f) => {
                        let code = f.0.code;
                        let want = if now >= SESSION_TTL_SECS {
                            "upload_expired"
                        } else if !contiguous || small_inside {
                            "invalid_parts"
                        } else {
                            prop_assert!(!within);
                            "too_large"
                        };
                        prop_assert_eq!(code, want);
                    }
                }
            }
        }
    }
}
