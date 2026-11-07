//! `/storage/v1/object`: upload, download, head, delete, copy, move and list. Every route runs
//! its metadata statement in `storage.objects` as the caller (row level security decides)
//! before it touches the object store; an object the caller's policies hide is 404, as a
//! missing one. Every route first looks the bucket up as the caller: callers need a SELECT
//! policy on `storage.buckets`. A bucket they can't see answers 403 `not_allowed`, exactly as an
//! RLS refusal of the operation does, whether it exists or not; only then is its id (a managed
//! column) and limit read as storage itself.
//!
//! An upload (m3's tickets; no Postgres transaction is held across the body):
//! 1. pre-check as the caller: the existing row's version (if any) and, in a savepoint rolled
//!    back, the INSERT (or upsert) with the declared values: 42501 is 403, `CB003` (the policy
//!    passed, only the ticket is missing) continues;
//!    The same transaction reads the database's OID: the blob's key is
//!    `{database_id}/{datoid}/{version_id}` with that OID, never a cached one;
//! 2. the body streams straight to that key (random, unreferenced until the row exists)
//!    through a byte counter (the bucket's limit: 413);
//! 3. storage clears the version's dead tickets (expired, or another timeline's) and writes
//!    one for the real size and etag, the caller's role and `sub`;
//! 4. one statement as the caller writes the row, after re-reading the OID in its own
//!    transaction (a database dropped and made again meanwhile: 503); the trigger consumes the
//!    ticket. An upsert updates only the version the pre-check saw, so a lost race is 409 and
//!    the winner's row is unchanged.
//!
//! A refused or failed write deletes the blob and withdraws the ticket
//! ([`crate::names::keep_if_written`]); a write whose outcome is unknown (no answer from
//! Postgres) keeps the blob for m8's sweeper, which deletes only unreferenced blobs.
//!
//! A write-once bucket (m7) is the database's to enforce (`freeze_objects`, [`WRITE_ONCE`]):
//! an upsert's overwrite, a move, a rename or a delete there fails in the statement, as the
//! pre-check's rolled-back write does, and is answered 409 `write_once`. Nothing here reads
//! the flag, so no route can forget it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chelabase_auth::internal::DataIdentity;
use chelabase_pgsession::service::Type;
use chelabase_pgsession::{ConnSource, NamedStatement, named_statement};
use chelabase_query::QueryFailure;
use chelabase_storage::ByteRange;
use serde::Deserialize;
use serde_json::{Value, json};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::app::{AppState, identity};
use crate::buckets::body;
use crate::engine::{DATOID_SQL, Located, datoid_of};
use crate::error::Failure;
use crate::events::ObjectChange;
use chelabase_nats::storage_objects::{ObjectEventType, ObjectFrom};
use crate::limits::{Limits, SNIFF_BYTES, peek, until_idle};
use crate::names::{self, Staged, check_object_name};
use crate::openapi::StorageErrorEnvelope;

/// A bucket's id and limits, by name (storage's own read: `bucket_id` is managed).
const BUCKET: NamedStatement = named_statement!(
    "bucket",
    "SELECT id, file_size_limit, allowed_mime_types FROM storage.buckets WHERE name OPERATOR(pg_catalog.=) $1",
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

/// Clears a version's dead tickets before one is minted for it: expired ones, and those a
/// branch copied from its parent (another timeline's, never consumable here). Otherwise a
/// leftover (an abandoned move, a failed withdrawal) blocks the version's tickets forever.
const PURGE_TICKETS: NamedStatement = named_statement!(
    "purge tickets",
    "DELETE FROM storage.upload_tickets WHERE version_id OPERATOR(pg_catalog.=) $1 AND (expires_at OPERATOR(pg_catalog.<=) pg_catalog.clock_timestamp() OR timeline_id IS DISTINCT FROM pg_catalog.current_setting('neon.timeline_id', true))",
    &[Type::UUID]
);

/// Withdraws a ticket the row it was minted for didn't take (a refused or failed write).
const UNTICKET: NamedStatement = named_statement!(
    "unticket",
    "DELETE FROM storage.upload_tickets WHERE version_id OPERATOR(pg_catalog.=) $1 AND bucket_id OPERATOR(pg_catalog.=) $2 AND name OPERATOR(pg_catalog.=) $3",
    &[Type::UUID, Type::UUID, Type::TEXT]
);

/// Whether a failed write may have committed: Postgres gave no answer (the connection lost,
/// the COMMIT unconfirmed), so it has no SQLSTATE and is a 503 `unavailable` or a 500.
pub fn outcome_unknown(f: &Failure) -> bool {
    f.0.sqlstate.is_none() && matches!(f.0.code, "unavailable" | "internal")
}

/// The trigger's code for a row its ticket doesn't vouch for.
const TICKET_MISSING: &str = "CB003";

/// The schema's code for a write-once refusal (m7): an object in a write-once bucket
/// overwritten (an upsert), moved, renamed or deleted; `write_once` turned off; a write-once
/// bucket holding objects renamed. The database raises it for every role, so the service
/// never checks the flag itself: 409 `write_once`, with the database's message.
pub const WRITE_ONCE: &str = "CB005";

/// A database error as this module reads it: the trigger's [`TICKET_MISSING`] keeps its
/// SQLSTATE (a 409 until a step reads it), [`WRITE_ONCE`] is 409 `write_once`, everything
/// else maps as the data API does.
pub fn db_failure(code: &str, message: &str, request_id: &str) -> QueryFailure {
    struct Db<'a>(&'a str, &'a str);
    impl chelabase_query::error::DbFields for Db<'_> {
        fn code(&self) -> &str {
            self.0
        }
        fn message(&self) -> &str {
            self.1
        }
        fn detail(&self) -> Option<&str> {
            None
        }
        fn hint(&self) -> Option<&str> {
            None
        }
        fn constraint(&self) -> Option<&str> {
            None
        }
    }
    if code == TICKET_MISSING || code == WRITE_ONCE {
        let kind = if code == WRITE_ONCE { "write_once" } else { "conflict" };
        let mut f = QueryFailure::new(StatusCode::CONFLICT, kind, message);
        f.sqlstate = Some(code.to_owned());
        return f;
    }
    QueryFailure::from_db(&Db(code, message), request_id)
}

/// A caller statement's failure, keeping the schema's own codes ([`db_failure`]): `CB003`
/// and `CB005`. The bucket routes use it too.
pub(crate) fn pg(request_id: &str) -> impl Fn(tokio_postgres::Error) -> QueryFailure + '_ {
    move |e| match e.as_db_error() {
        Some(db) if matches!(db.code().code(), TICKET_MISSING | WRITE_ONCE) => {
            db_failure(db.code().code(), db.message(), request_id)
        }
        _ => QueryFailure::from_pg(&e, request_id),
    }
}

/// A row delete's failure, from the error's SQLSTATE (`None`: not a Postgres error), whether
/// Postgres raised it inside a function or a nested statement (`nested`: it has a context
/// line), and the data API's reading of it (`f`).
///
/// **Invariant (#102, m11 audit B1): no storage error body ever carries Postgres message,
/// detail or hint text from a user's objects.** On a delete, a user's objects come in through
/// the tables that reference `storage.objects(id)`: their keys' checks, their referential
/// actions (which run nested), and the triggers and functions those set off (nested too). The
/// owner can't put triggers on `storage.objects` and nothing in the storage schema references
/// an object, so:
/// - a foreign key violation (23503) is 409 `referenced` ([`Failure::object_referenced`]);
/// - every other integrity violation (class 23) is 409 `delete_refused`;
/// - the schema's write-once refusal ([`WRITE_ONCE`]) is its fixed 409 `write_once`;
/// - any other nested error is 409 `delete_refused` (a user's trigger picks its own SQLSTATE
///   and text; an RLS policy function's error lands here too);
/// - what's left was raised by the `DELETE` on `storage.objects` itself (RLS, privileges,
///   timeouts, conflicts), whose text names only storage's own objects, and is left as it was.
///
/// `delete_rows` runs the keys immediately before and after the `DELETE`, so nothing a user
/// table defers is left for COMMIT, which this mapping doesn't see.
pub fn deleted_failure(code: Option<&str>, nested: bool, f: QueryFailure) -> Failure {
    match code {
        None => Failure(f),
        Some(FOREIGN_KEY_VIOLATION) => Failure::object_referenced(),
        Some(c) if c.starts_with(INTEGRITY_CLASS) => Failure::delete_refused(),
        Some(WRITE_ONCE) => Failure::write_once_delete(),
        Some(_) if nested => Failure::delete_refused(),
        Some(_) => Failure(f),
    }
}

/// A row delete's statement failure, through [`deleted_failure`].
fn delete_error(request_id: &str) -> impl Fn(tokio_postgres::Error) -> QueryFailure + '_ {
    move |e| {
        let db = e.as_db_error();
        let (code, nested) = (db.map(|d| d.code().code()), db.is_some_and(|d| d.where_().is_some()));
        deleted_failure(code, nested, QueryFailure::from_pg(&e, request_id)).0
    }
}

/// Postgres's foreign key violation.
const FOREIGN_KEY_VIOLATION: &str = "23503";

/// Postgres's integrity constraint violations, SQLSTATE class 23.
const INTEGRITY_CLASS: &str = "23";

/// Every deferrable key and constraint trigger checked now, in the delete's transaction: one
/// a user's table defers would otherwise fail at COMMIT, past [`deleted_failure`].
const KEYS_NOW: &str = "SET CONSTRAINTS ALL IMMEDIATE";

/// The row an upload or copy writes.
pub(crate) struct Row {
    pub bucket: Uuid,
    pub name: String,
    pub owner: Option<Uuid>,
    pub size: Option<i64>,
    pub content_type: String,
    pub etag: Option<String>,
    pub version: Uuid,
    /// The database's OID, read in the pre-check's transaction (0 until then).
    pub datoid: u32,
}

const INSERT: &str = "INSERT INTO storage.objects (bucket_id, name, owner, size, content_type, etag, version_id) VALUES ($1, $2, $3, $4, $5, $6, $7)";
const UPSERT: &str = "INSERT INTO storage.objects (bucket_id, name, owner, size, content_type, etag, version_id) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (bucket_id, name) DO UPDATE SET owner = EXCLUDED.owner, size = EXCLUDED.size, content_type = EXCLUDED.content_type, etag = EXCLUDED.etag, version_id = EXCLUDED.version_id WHERE storage.objects.version_id IS NOT DISTINCT FROM $8";

/// An upload's event: an upsert that replaced the version the pre-check saw is an update;
/// anything else wrote a new object (`final_write`'s INSERT).
pub(crate) fn written_kind(upsert: bool, expected: Option<Uuid>) -> ObjectEventType {
    if upsert && expected.is_some() { ObjectEventType::Updated } else { ObjectEventType::Created }
}

impl Row {
    /// The change this written row made, in `bucket` (by name).
    pub(crate) fn change(&self, kind: ObjectEventType, bucket: &str) -> ObjectChange {
        ObjectChange {
            kind,
            bucket: bucket.to_owned(),
            name: self.name.clone(),
            version_id: Some(self.version),
            size: self.size,
            content_type: Some(self.content_type.clone()),
            from: None,
        }
    }
}

/// The caller's `sub` claim as a uuid (an app user), else no owner.
pub(crate) fn owner_of(data: &DataIdentity) -> Option<Uuid> {
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
    pub(crate) async fn bucket(
        &self,
        data: &DataIdentity,
        at: &Located,
        bucket: &str,
        rid: &str,
    ) -> Result<(Uuid, Option<i64>), Failure> {
        let limits = self.limits(data, at, bucket, rid).await?;
        Ok((limits.id, limits.file_size_limit))
    }

    /// `bucket`'s id and limits, as [`Self::bucket`] finds it.
    pub(crate) async fn limits(
        &self,
        data: &DataIdentity,
        at: &Located,
        bucket: &str,
        rid: &str,
    ) -> Result<Limits, Failure> {
        // As the caller first: a bucket it can't see answers as a refusal, whether it exists.
        let (seen, _) = self
            .caller(data, at, rid, async |c, _| {
                c.query_opt("SELECT 1 FROM storage.buckets WHERE name = $1", &[&bucket])
                    .await
                    .map_err(pg(rid))
            })
            .await?;
        if seen.is_none() {
            return Err(Failure::not_allowed());
        }
        let rows = self
            .engine
            .managed(data.branch_id, &at.database, &BUCKET, &[&bucket], rid)
            .await?;
        rows.first()
            .map(|r| Limits {
                id: r.get(0),
                file_size_limit: r.get(1),
                allowed_mime_types: r.get(2),
            })
            .ok_or_else(Failure::not_allowed)
    }

    /// The destination's limits for the stored object at `key` (copy, move): 413 over its
    /// size, 415 when its type or its first bytes aren't on the list. Nothing is written yet.
    async fn check_existing(
        &self,
        limits: &Limits,
        key: &str,
        src: &Found,
        rid: &str,
    ) -> Result<(), Failure> {
        limits.check_size(src.size.max(0) as u64)?;
        limits.check_declared(&src.content_type)?;
        if limits.sniffs() {
            let head = ByteRange::From {
                start: 0,
                end: Some(SNIFF_BYTES as u64 - 1),
            };
            let got = self
                .store
                .get(key, names::range_for(src.size.max(0) as u64, Some(head)))
                .await
                .map_err(|e| Failure::from_store(&e, rid))?;
            let head = chelabase_storage::collect(got.body)
                .await
                .map_err(|e| Failure::from_store(&e, rid))?;
            limits.check_sniffed(&head)?;
        }
        Ok(())
    }

    /// [`crate::engine::Engine::as_caller`] in the located database, a refusal as
    /// [`Failure::not_allowed`].
    pub(crate) async fn caller<T>(
        &self,
        data: &DataIdentity,
        at: &Located,
        rid: &str,
        f: impl AsyncFnOnce(&tokio_postgres::Client, &str) -> Result<T, chelabase_query::QueryFailure>,
    ) -> Result<(T, String), Failure> {
        self.engine
            .as_caller(data, Some(&at.database), rid, f)
            .await
            .map_err(Failure::refusal)
    }

    /// Pre-check (step 1): the version of the row the caller sees at the name (`None`: none),
    /// after the write with `row`'s declared values passed the policies in a rolled-back
    /// savepoint. An existing row without `upsert` is 409.
    pub(crate) async fn precheck(
        &self,
        data: &DataIdentity,
        at: &Located,
        row: &Row,
        upsert: bool,
        rid: &str,
    ) -> Result<(Option<Uuid>, u32), Failure> {
        let (seen, _) = self
            .caller(data, at, rid, async |c, _| {
                let oid: i64 = c.query_one(DATOID_SQL, &[]).await.map_err(pg(rid))?.get(0);
                let Some(datoid) = datoid_of(Some(oid)) else {
                    return Ok(Err(Failure::storage_unavailable()));
                };
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
                    Err(f) if f.sqlstate.as_deref() == Some(TICKET_MISSING) => {
                        Ok((seen.flatten(), datoid))
                    }
                    Err(f) if f.sqlstate.as_deref() == Some("23505") => {
                        Err(Failure::object_conflict())
                    }
                    Err(f) => Err(Failure(f).refusal()),
                    // A ticket happened to match: the policy passed all the same.
                    Ok(_) => Ok((seen.flatten(), datoid)),
                })
            })
            .await?;
        seen
    }

    /// Steps 3 and 4 (copy: its blob is already at the version key): the ticket, then the
    /// caller's own write of `row`. `expected`: the version
    /// the pre-check saw at the name (an upsert replaces only it).
    pub(crate) async fn write(
        &self,
        data: &DataIdentity,
        at: &Located,
        row: &Row,
        expected: Option<Uuid>,
        upsert: bool,
        rid: &str,
    ) -> Result<(), Failure> {
        self.ticket(data, at, row, rid).await?;
        let out = self.final_write(data, at, row, expected, upsert, rid).await;
        if out.is_err() {
            self.unticket(data, at, row, rid).await;
        }
        out
    }

    /// Withdraws `row`'s ticket after its write failed. Best effort: a ticket left behind (this
    /// failing, the request dropped) is cleared by the next mint for its version
    /// ([`PURGE_TICKETS`]) once it has expired.
    async fn unticket(&self, data: &DataIdentity, at: &Located, row: &Row, rid: &str) {
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 3] =
            [&row.version, &row.bucket, &row.name];
        if let Err(f) = self
            .engine
            .managed(data.branch_id, &at.database, &UNTICKET, &params, rid)
            .await
        {
            tracing::warn!(request_id = rid, "withdrawing a ticket: {}", f.0.message);
        }
    }

    /// Step 3: the ticket for `row` (its version, real size and etag), the caller's role and
    /// owner, written as storage itself.
    async fn ticket(
        &self,
        data: &DataIdentity,
        at: &Located,
        row: &Row,
        rid: &str,
    ) -> Result<(), Failure> {
        self.engine
            .managed(
                data.branch_id,
                &at.database,
                &PURGE_TICKETS,
                &[&row.version],
                rid,
            )
            .await?;
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
        Ok(())
    }

    /// Step 4: the caller's own write of `row`; `expected`: the version the pre-check saw at
    /// the name (an upsert replaces only it).
    async fn final_write(
        &self,
        data: &DataIdentity,
        at: &Located,
        row: &Row,
        expected: Option<Uuid>,
        upsert: bool,
        rid: &str,
    ) -> Result<(), Failure> {
        let (written, _) = self
            .caller(data, at, rid, async |c, _| {
                // The database dropped and made again since the pre-check: the blob's key names
                // the old one, so nothing is written.
                let oid: i64 = c.query_one(DATOID_SQL, &[]).await.map_err(pg(rid))?.get(0);
                if datoid_of(Some(oid)) != Some(row.datoid) {
                    return Ok(Err(Failure::storage_unavailable()));
                }
                let base: [&(dyn tokio_postgres::types::ToSql + Sync); 7] = [
                    &row.bucket,
                    &row.name,
                    &row.owner,
                    &row.size,
                    &row.content_type,
                    &row.etag,
                    &row.version,
                ];
                let written = if upsert && expected.is_some() {
                    let mut params = base.to_vec();
                    params.push(&expected);
                    c.execute(UPSERT, &params).await.map_err(pg(rid))?
                } else {
                    c.execute(INSERT, &base).await.map_err(pg(rid))?
                };
                Ok(Ok(written))
            })
            .await
            .map_err(final_failure)?;
        let written = written?;
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
    path: (String, String),
    q: ObjectParams,
    request: Body,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    upload_as(&state, &data, &rid, &headers, path, q, request).await
}

/// An upload as `data` (a data key's request, or an upload token's minting key).
pub(crate) async fn upload_as<S: ConnSource, M: ConnSource>(
    state: &AppState<S, M>,
    data: &DataIdentity,
    rid: &str,
    headers: &HeaderMap,
    (bucket, name): (String, String),
    q: ObjectParams,
    request: Body,
) -> Result<Response, Failure> {
    let (data, rid, headers) = (data.clone(), rid.to_owned(), headers.clone());
    check_object_name(&name)?;
    let upsert = q.upsert.unwrap_or(false) || header(&headers, "x-upsert") == Some("true");
    let at = state
        .engine
        .locate(&data, q.database.as_deref(), &rid)
        .await?;
    let limits = state.limits(&data, &at, &bucket, &rid).await?;
    let (bucket_id, limit) = (limits.id, limits.file_size_limit);
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
        datoid: 0,
    };
    names::check_content_type(&row.content_type)?;
    limits.check_declared(&row.content_type)?;
    let (expected, datoid) = state.precheck(&data, &at, &row, upsert, &rid).await?;
    row.datoid = datoid;
    // The body's buffer (at most one stream part) comes out of the process's cap as its bytes
    // arrive: 503 `busy` past it. A body that stalls fails the request (408), which frees it.
    let stream_part = chelabase_storage::STREAM_PART_SIZE as u64;
    let (body, stalled) = until_idle(request.into_data_stream(), state.inflight.body_idle());
    let (mut body, held) = state.inflight.metered(body, stream_part);
    if limits.sniffs() {
        let (head, again) = peek(body).await.map_err(|e| e.failure())?;
        limits.check_sniffed(&head)?;
        body = again;
    }
    // Straight to the version's own key: random, and unreferenced until the row exists.
    let key = names::version_key(&names::object_prefix(at.database_id, datoid), row.version);
    let (stream, counter) = names::counted(body, limit.map(|l| l.max(0) as u64));
    let meta = names::stage(
        state.store.as_ref(),
        &key,
        stream,
        &counter,
        Some(row.content_type.clone()),
    )
    .await
    .map_err(|e| match e {
        Staged::TooLarge => Failure::too_large(limit.unwrap_or_default()),
        Staged::Store(_) if stalled.load(Ordering::SeqCst) => {
            Failure::body_stalled(state.inflight.body_idle())
        }
        Staged::Store(_) if held.refused() => Failure::busy(),
        Staged::Store(e) => Failure::from_store(&e, &rid),
    })?;
    row.size = Some(meta.size as i64);
    row.etag = Some(meta.etag.trim_matches('"').to_owned());
    // After the body: the ticket (its real size and etag), then the caller's row. A refused or
    // failed write deletes the blob (unless its outcome is unknown) and withdraws the ticket.
    names::keep_if_written(state.store.as_ref(), &key, async || {
        state.write(&data, &at, &row, expected, upsert, &rid).await
    })
    .await?;
    state.emit(&data, &at, row.change(written_kind(upsert, expected), &bucket));
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
    params(("bucket" = String, Path), ("name" = String, Path, description = "The object's name: at most 1024 bytes of UTF-8, no control characters, no leading `/`, no empty segment and none made only of dots (`.`, `..`, full-width or other NFKC dot forms)"), ObjectParams),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Stored: `bucket`, `name`, `version_id`, `size`, `etag`", body = Value),
        (status = 400, description = "`invalid_request`", body = StorageErrorEnvelope),
        (status = 403, description = "`not_allowed`: no policy allows it, or the caller can't see the bucket (every object route needs a SELECT policy on `storage.buckets`); `database_not_allowed`, `role_not_allowed`", body = StorageErrorEnvelope),
        (status = 409, description = "`conflict`: the name is taken (no upsert), or another upload changed it first; `write_once`: an upsert over an object of a write-once bucket", body = StorageErrorEnvelope),
        (status = 408, description = "`request_timeout`: the body sent nothing for `CHELABASE_STORAGE_BODY_IDLE_SECS` (60 s by default)", body = StorageErrorEnvelope),
        (status = 413, description = "`too_large`: over the bucket's file size limit", body = StorageErrorEnvelope),
        (status = 415, description = "`mime_not_allowed`: the declared type, or the type the first bytes are recognised as, isn't on the bucket's `allowed_mime_types`", body = StorageErrorEnvelope),
        (status = 502, description = "`storage_error`: the object store failed", body = StorageErrorEnvelope),
        (status = 503, description = "`storage_unavailable`, `unavailable`, `busy` (too many uploads in flight)", body = StorageErrorEnvelope),
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
pub(crate) struct Found {
    /// The database's OID, read with the row.
    pub datoid: u32,
    pub version: Uuid,
    pub size: i64,
    pub etag: String,
    pub content_type: String,
    pub owner: Option<Uuid>,
}

impl<S: ConnSource, M: ConnSource> AppState<S, M> {
    /// The object `name` of `bucket` as the caller sees it; 404 when missing or hidden.
    pub(crate) async fn find(
        &self,
        data: &DataIdentity,
        at: &Located,
        bucket: &str,
        name: &str,
        rid: &str,
    ) -> Result<(Uuid, Found), Failure> {
        let (bucket_id, _) = self.bucket(data, at, bucket, rid).await?;
        // The OID in the same transaction as the row: it decides the blob's key. A row without
        // a version or a size (never one the service writes) isn't served.
        let (found, _) = self
            .caller(data, at, rid, async |c, _| {
                let row = c
                    .query_opt(
                        "SELECT version_id, size, etag, content_type, owner FROM storage.objects WHERE bucket_id = $1 AND name = $2 AND version_id IS NOT NULL AND size IS NOT NULL",
                        &[&bucket_id, &name],
                    )
                    .await
                    .map_err(pg(rid))?;
                let oid = c.query_one(DATOID_SQL, &[]).await.map_err(pg(rid))?;
                Ok(row.map(|row| (row, oid.get::<_, i64>(0))))
            })
            .await?;
        let (row, oid) = found.ok_or_else(Failure::object_not_found)?;
        let datoid = datoid_of(Some(oid)).ok_or_else(Failure::storage_unavailable)?;
        Ok((
            bucket_id,
            Found {
                datoid,
                version: row.get(0),
                size: row.get::<_, i64>(1),
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
    // Only the parsed essence (a browser takes a list's last type), never the stored text.
    let content_type = names::served_content_type(&found.content_type);
    set(&mut h, header::CONTENT_TYPE, content_type.clone());
    // Inline only for a safe type without `?download`; anything else is an attachment.
    let disposition = match download {
        None if names::inline_safe(&content_type) => None,
        other => Some(other.unwrap_or("")),
    };
    set(
        &mut h,
        header::CONTENT_DISPOSITION,
        names::content_disposition(name, disposition),
    );
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // Never sniffed into another type, and never a document that runs script on the API's
    // origin, even opened directly.
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox"),
    );
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
    serve(
        &state,
        &headers,
        at.database_id,
        &found,
        &name,
        q.download.as_deref(),
        &rid,
    )
    .await
}

/// Answers a GET of `found` (its blob under `database_id`'s prefix): 304 for a matching
/// `If-None-Match`, 206 for a `Range`, else 200, with [`object_headers`].
pub(crate) async fn serve<S, M>(
    state: &AppState<S, M>,
    headers: &HeaderMap,
    database_id: Uuid,
    found: &Found,
    name: &str,
    download: Option<&str>,
    rid: &str,
) -> Result<Response, Failure> {
    let mut out = object_headers(found, name, download);
    if header(headers, header::IF_NONE_MATCH).is_some_and(|v| names::none_match(v, &found.etag)) {
        return Ok((StatusCode::NOT_MODIFIED, out).into_response());
    }
    let range = match header(headers, header::RANGE) {
        Some(r) => Some(ByteRange::parse(r).map_err(|_| Failure::range_not_satisfiable())?),
        None => None,
    };
    // A zero-byte object answers 200 with its empty body whatever the range.
    let range = names::range_for(found.size.max(0) as u64, range);
    let span = match range {
        Some(r) => Some(
            r.resolve(found.size.max(0) as u64)
                .map_err(|_| Failure::range_not_satisfiable())?,
        ),
        None => None,
    };
    let key = names::version_key(
        &names::object_prefix(database_id, found.datoid),
        found.version,
    );
    let object = state
        .store
        .get(&key, range)
        .await
        .map_err(|e| Failure::from_store(&e, rid))?;
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
    /// for the sweeper (m8), which deletes only blobs no row references: a delete a foreign key
    /// refuses (409 `referenced`) deletes no row, so it keeps every blob.
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
            .caller(data, &at, rid, async |c, _| {
                // Before: a deferred key fails in the statement. After: a trigger the delete
                // set off may have deferred the keys again; this checks what it put off.
                c.batch_execute(KEYS_NOW).await.map_err(delete_error(rid))?;
                let rows = c
                    .query("DELETE FROM storage.objects WHERE bucket_id = $1 AND name = ANY($2) RETURNING name, version_id, size, content_type", &[&bucket_id, &names])
                    .await
                    .map_err(delete_error(rid))?;
                c.batch_execute(KEYS_NOW).await.map_err(delete_error(rid))?;
                Ok(rows
                    .iter()
                    .map(|r| (r.get::<_, String>(0), r.get(1), r.get(2), r.get(3)))
                    .collect::<Vec<(String, Option<Uuid>, Option<i64>, Option<String>)>>())
            })
            .await?;
        let mut names = Vec::with_capacity(deleted.len());
        for (name, version_id, size, content_type) in deleted {
            self.emit(
                data,
                &at,
                ObjectChange {
                    kind: ObjectEventType::Deleted,
                    bucket: bucket.to_owned(),
                    name: name.clone(),
                    version_id,
                    size,
                    content_type,
                    from: None,
                },
            );
            names.push(name);
        }
        Ok(names)
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
    responses((status = 204, description = "Deleted"), (status = 404, description = "`object_not_found`: missing, or hidden", body = StorageErrorEnvelope), (status = 409, description = "`write_once`: the bucket is write-once; `referenced`: a foreign key to storage.objects(id) still references it; `delete_refused`: a constraint, trigger or function on a table that references it refused the delete (both with a fixed message; nothing is deleted)", body = StorageErrorEnvelope)),
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
    responses((status = 200, description = "`deleted`: the names deleted (those missing or hidden are left out)", body = Value), (status = 409, description = "`write_once`: the bucket is write-once; `referenced`: a foreign key to storage.objects(id) still references one of them; `delete_refused`: a constraint, trigger or function on a table that references one of them refused the delete (both with a fixed message). Either way nothing is deleted", body = StorageErrorEnvelope)),
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
    responses((status = 200, description = "`version_id` of the copy", body = Value), (status = 404, description = "The source is missing or hidden", body = StorageErrorEnvelope), (status = 409, description = "The destination exists", body = StorageErrorEnvelope), (status = 413, description = "`too_large`: over the destination bucket's limit; nothing is written", body = StorageErrorEnvelope), (status = 415, description = "`mime_not_allowed`: a type the destination bucket doesn't allow", body = StorageErrorEnvelope)),
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
    let dest = state.limits(&data, &at, to_bucket, &rid).await?;
    let source = names::version_key(
        &names::object_prefix(at.database_id, src.datoid),
        src.version,
    );
    // The destination's limits before anything is written (413, 415).
    state.check_existing(&dest, &source, &src, &rid).await?;
    let bucket_id = dest.id;
    let row = Row {
        bucket: bucket_id,
        name: r.to.clone(),
        owner: owner_of(&data),
        size: Some(src.size),
        content_type: src.content_type.clone(),
        etag: Some(src.etag.clone()),
        version: Uuid::new_v4(),
        datoid: 0,
    };
    let (expected, datoid) = state.precheck(&data, &at, &row, false, &rid).await?;
    let row = Row { datoid, ..row };
    // Only after the caller may read the source and write the destination: the blob is copied
    // to the new version's key, deleted again if the row isn't written. `CopyObject`: on
    // AWS S3 that copies at most 5 GB; the S3 store copies a larger one in `UploadPartCopy`
    // parts (m7).
    let key = names::version_key(&names::object_prefix(at.database_id, datoid), row.version);
    let meta = state
        .store
        .copy(&source, &key)
        .await
        .map_err(|e| Failure::from_store(&e, &rid))?;
    let row = Row {
        etag: Some(meta.etag.trim_matches('"').to_owned()),
        size: Some(meta.size as i64),
        ..row
    };
    names::keep_if_written(state.store.as_ref(), &key, async || {
        state.write(&data, &at, &row, expected, false, &rid).await
    })
    .await?;
    state.emit(&data, &at, row.change(ObjectEventType::Created, to_bucket));
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
    responses((status = 204, description = "Moved"), (status = 404, description = "The source is missing or hidden", body = StorageErrorEnvelope), (status = 409, description = "`conflict`: the destination exists; `write_once`: the source's bucket is write-once", body = StorageErrorEnvelope), (status = 413, description = "`too_large`: over the destination bucket's limit; nothing changes", body = StorageErrorEnvelope), (status = 415, description = "`mime_not_allowed`: a type the destination bucket doesn't allow", body = StorageErrorEnvelope)),
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
    let to_id = match r.to_bucket.as_deref() {
        Some(b) if b != r.bucket => {
            // Into another bucket: its limits hold before anything changes (413, 415).
            let dest = state.limits(&data, &at, b, &rid).await?;
            let key = names::version_key(
                &names::object_prefix(at.database_id, src.datoid),
                src.version,
            );
            state.check_existing(&dest, &key, &src, &rid).await?;
            dest.id
        }
        _ => from_id,
    };
    const MOVE: &str = "UPDATE storage.objects SET bucket_id = $3, name = $4 WHERE bucket_id = $1 AND name = $2 AND version_id = $5";
    // A cross-bucket move changes a managed column, so it needs a ticket: minted only once the
    // caller's policies passed the same UPDATE in a rolled-back savepoint.
    let ticketed = (to_id != from_id).then(|| Row {
        bucket: to_id,
        name: r.to.clone(),
        owner: src.owner,
        size: Some(src.size),
        content_type: src.content_type.clone(),
        etag: Some(src.etag.clone()),
        version: src.version,
        datoid: src.datoid,
    });
    if let Some(row) = &ticketed {
        let (passed, _) = state
            .caller(&data, &at, &rid, async |c, _| {
                c.batch_execute("SAVEPOINT premove")
                    .await
                    .map_err(pg(&rid))?;
                let tried = c
                    .execute(MOVE, &[&from_id, &r.from, &to_id, &r.to, &src.version])
                    .await
                    .map_err(pg(&rid));
                c.batch_execute("ROLLBACK TO SAVEPOINT premove")
                    .await
                    .map_err(pg(&rid))?;
                Ok(match tried {
                    Err(f) if f.sqlstate.as_deref() == Some(TICKET_MISSING) => Ok(()),
                    Err(f) => Err(final_failure(Failure(f).refusal())),
                    Ok(0) => Err(Failure::object_not_found()),
                    // A ticket happened to match: the policy passed all the same.
                    Ok(_) => Ok(()),
                })
            })
            .await?;
        passed?;
        state.ticket(&data, &at, row, &rid).await?;
    }
    let moved = state
        .caller(&data, &at, &rid, async |c, _| {
            c.execute(MOVE, &[&from_id, &r.from, &to_id, &r.to, &src.version])
                .await
                .map_err(pg(&rid))
        })
        .await
        .map_err(final_failure)
        .and_then(|(moved, _)| {
            if moved == 0 {
                Err(Failure::object_not_found())
            } else {
                Ok(())
            }
        });
    if let (Err(_), Some(row)) = (&moved, &ticketed) {
        state.unticket(&data, &at, row, &rid).await;
    }
    moved?;
    state.emit(
        &data,
        &at,
        ObjectChange {
            kind: ObjectEventType::Moved,
            bucket: r.to_bucket.clone().unwrap_or_else(|| r.bucket.clone()),
            name: r.to.clone(),
            version_id: Some(src.version),
            size: Some(src.size),
            content_type: Some(src.content_type.clone()),
            from: Some(ObjectFrom { bucket: r.bucket.clone(), name: r.from.clone() }),
        },
    );
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
        .caller(&data, &at, &rid, async |c, _| {
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

    fn failure(sqlstate: &str) -> Failure {
        let mut f = QueryFailure::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "x");
        f.sqlstate = Some(sqlstate.into());
        Failure(f)
    }

    /// #102: a delete a user's foreign key to `storage.objects(id)` refuses (23503) is 409
    /// `referenced`, with a fixed message (never Postgres's, which names the user's table and
    /// constraint); every other failure is left as it was.
    #[test]
    fn a_delete_refused_by_a_foreign_key_is_referenced() {
        let fk = db_failure(
            "23503",
            "update or delete on table \"objects\" violates foreign key constraint \"secret_photo_fkey\" on table \"secret\"",
            "r",
        );
        assert!(
            fk.message.contains("secret"),
            "the data API's mapping keeps the message: {}",
            fk.message
        );
        let got = deleted_failure(Some("23503"), false, fk);
        assert_eq!(
            (got.0.status, got.0.code, got.0.sqlstate.as_deref()),
            (StatusCode::CONFLICT, "referenced", Some("23503"))
        );
        assert!(!got.0.message.contains("secret"), "{}", got.0.message);
        assert_eq!(got.0.detail, None);
        // The DELETE's own failures (not nested, not class 23) are left as they were.
        for other in ["42501", "40001", "57014"] {
            assert_eq!(deleted_failure(Some(other), false, failure(other).0), failure(other), "{other}");
        }
        assert_eq!(deleted_failure(None, false, failure("x").0), failure("x"));
    }

    /// The invariant on [`deleted_failure`] (m11 audit B1, the second leak in the delete
    /// path): no storage error body ever carries Postgres message, detail or hint text from a
    /// user's objects. Exhaustive over every class 23 SQLSTATE, the schema's own codes, and a
    /// spread of codes a user's trigger or function might raise, each nested and not: every
    /// one that can come from a user's object gets a fixed 409 whose text and SQLSTATE are
    /// the same whatever Postgres said.
    #[test]
    fn a_refused_delete_never_carries_the_users_text() {
        const SECRET: &str = "secret_table";
        let class_23 = ["23000", "23001", "23502", "23503", "23505", "23514", "23P01"];
        let raised = [WRITE_ONCE, TICKET_MISSING, "P0001", "42501", "40001", "40P01", "55P03", "57014", "22023", "XX000", "ZZ999"];
        let from_user = |code: &str| {
            let mut f = QueryFailure::new(StatusCode::BAD_REQUEST, "raised", format!("{SECRET} row 1 ({code})"));
            f.detail = Some(format!("Failing row contains ({SECRET})"));
            f.hint = Some(format!("see {SECRET}"));
            f.sqlstate = Some(code.to_owned());
            f
        };
        let mut checked = 0;
        for code in class_23.iter().chain(&raised) {
            for nested in [false, true] {
                let user_reachable = code.starts_with("23") || nested || *code == WRITE_ONCE;
                if !user_reachable {
                    continue;
                }
                let got = deleted_failure(Some(code), nested, from_user(code)).0;
                let body = format!("{} {:?} {:?} {:?}", got.message, got.detail, got.hint, got.sqlstate);
                assert!(!body.contains(SECRET), "{code} nested={nested}: {body}");
                assert_eq!((&got.detail, &got.hint), (&None, &None), "{code} nested={nested}");
                assert_eq!(got.status, StatusCode::CONFLICT, "{code} nested={nested}");
                let want = match *code {
                    "23503" => Failure::object_referenced(),
                    WRITE_ONCE => Failure::write_once_delete(),
                    _ => Failure::delete_refused(),
                };
                assert_eq!(Failure(got), want, "{code} nested={nested}");
                checked += 1;
            }
        }
        assert_eq!(checked, class_23.len() * 2 + 2 + (raised.len() - 1), "every reachable case");
    }

    /// The trigger's `CB003` keeps its SQLSTATE (the data API's mapping has no row for it and
    /// would make it a 500 without one): the pre-check reads it as "the policy passed", the
    /// final write as a lost race.
    #[test]
    fn a_missing_ticket_keeps_its_sqlstate() {
        let f = db_failure(
            TICKET_MISSING,
            "storage.objects: the row matches no unused upload ticket",
            "r",
        );
        assert_eq!(
            (f.status, f.sqlstate.as_deref()),
            (StatusCode::CONFLICT, Some(TICKET_MISSING))
        );
        assert_eq!(final_failure(Failure(f)).0.code, "conflict");
        let rls = db_failure("42501", "new row violates row-level security policy", "r");
        assert_eq!(
            (rls.status, rls.sqlstate.as_deref()),
            (StatusCode::FORBIDDEN, Some("42501"))
        );
    }

    /// m7 (write-once): the triggers' `CB005` (an object in a write-once bucket updated, moved or
    /// deleted; `write_once` turned off; a write-once bucket holding objects renamed) is 409
    /// `write_once` with the database's own message, through every mapping: the pre-check's
    /// refusal, the final write's and a plain statement's.
    #[test]
    fn a_write_once_refusal_is_409_write_once() {
        let message = "storage.objects: the bucket is write-once";
        let f = db_failure("CB005", message, "r");
        assert_eq!(
            (f.status, f.code, f.message.as_str(), f.sqlstate.as_deref()),
            (StatusCode::CONFLICT, "write_once", message, Some("CB005"))
        );
        assert_eq!(final_failure(Failure(f.clone())).0.code, "write_once");
        assert_eq!(Failure(f.clone()).refusal().0.code, "write_once");
        assert!(!outcome_unknown(&Failure(f)));
    }

    /// SF2: a final write that failed without an answer from Postgres (the connection lost, the
    /// COMMIT unconfirmed) may have committed: its blob stays for m8's sweeper. Any answer
    /// (a refusal, a conflict, our own refusals) means nothing was written.
    #[test]
    fn only_an_unanswered_write_keeps_its_blob() {
        let lost = Failure(QueryFailure::unreachable("r", "connection closed"));
        assert!(outcome_unknown(&lost));
        let internal = Failure(QueryFailure::internal("r", "a value that didn't decode"));
        assert!(outcome_unknown(&internal));
        for answered in [
            Failure(db_failure("42501", "rls", "r")),
            Failure(db_failure("23505", "dup", "r")),
            Failure::object_conflict(),
            Failure::not_allowed(),
            Failure::storage_unavailable(),
        ] {
            assert!(!outcome_unknown(&answered), "{answered:?}");
        }
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

    /// m6: no stored object runs script on the API origin: every answer is sandboxed and
    /// never sniffed, and anything but a safe inline type is an attachment.
    #[test]
    fn unsafe_types_are_sandboxed_attachments() {
        let mut found = Found {
            datoid: 1,
            version: Uuid::nil(),
            size: 1,
            etag: "e".into(),
            content_type: "text/html".into(),
            owner: None,
        };
        let h = object_headers(&found, "a.html", None);
        assert_eq!(h[header::CONTENT_SECURITY_POLICY], "sandbox");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(h[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment;"));
        found.content_type = "image/png".into();
        let h = object_headers(&found, "a.png", None);
        assert_eq!(h[header::CONTENT_DISPOSITION], "inline");
        assert_eq!(h[header::CONTENT_SECURITY_POLICY], "sandbox");
        let h = object_headers(&found, "a.png", Some(""));
        assert!(h[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment;"));
    }

    /// m6 audit B3: a stored type a browser would read as HTML (a comma list) is served as
    /// an octet-stream attachment; a safe type is served as its essence only.
    #[test]
    fn a_comma_content_type_is_never_inline() {
        for stored in [
            "text/plain;a=b,text/html",
            "image/png;x=1, text/html",
            "text/plain;charset=utf-8,application/xhtml+xml",
        ] {
            let found = Found {
                datoid: 1,
                version: Uuid::nil(),
                size: 1,
                etag: "e".into(),
                content_type: stored.into(),
                owner: None,
            };
            let h = object_headers(&found, "a.txt", None);
            assert_eq!(h[header::CONTENT_TYPE], "application/octet-stream", "{stored}");
            assert!(h[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment;"), "{stored}");
        }
        let found = Found {
            datoid: 1,
            version: Uuid::nil(),
            size: 1,
            etag: "e".into(),
            content_type: "text/plain; charset=utf-8; x=<b>".into(),
            owner: None,
        };
        let h = object_headers(&found, "a.txt", None);
        assert_eq!(h[header::CONTENT_TYPE], "application/octet-stream");
    }

    /// Every download and HEAD answer says `nosniff`.
    #[test]
    fn downloads_are_never_sniffed() {
        let found = Found {
            datoid: 1,
            version: Uuid::nil(),
            size: 1,
            etag: "e".into(),
            content_type: "text/html".into(),
            owner: None,
        };
        let h = object_headers(&found, "a.html", None);
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::CONTENT_TYPE], "text/html");
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
