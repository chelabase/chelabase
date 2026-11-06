//! `/storage/v1/bucket`: list, create, update and delete (empty only), each one statement as
//! the caller ([`crate::engine::Engine::as_caller`]), so row level security on
//! `storage.buckets` decides. A bucket the caller's policies hide is 404, as a missing one.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chelabase_pgsession::ConnSource;
use chelabase_query::QueryFailure;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::app::{AppState, identity};
use crate::error::Failure;
use crate::openapi::StorageErrorEnvelope;

/// `^[a-z0-9][a-z0-9._-]{0,62}$`: a bucket's name, safe in a path and a key.
pub fn valid_bucket_name(name: &str) -> bool {
    let b = name.as_bytes();
    let ok = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    matches!(b.first(), Some(c) if ok(c))
        && b.len() <= 63
        && b.iter().all(|c| ok(c) || matches!(c, b'.' | b'_' | b'-'))
}

fn check_name(name: &str) -> Result<(), Failure> {
    if valid_bucket_name(name) {
        Ok(())
    } else {
        Err(Failure::invalid_request(
            "a bucket name is 1 to 63 lower-case letters, digits, `.`, `_` or `-`, starting with a letter or digit",
        ))
    }
}

/// Literal segments the storage routes use where a bucket name goes (`/object/list/…`,
/// `/object/copy`, `/upload/session/…`, …, and the ones later milestones add): never a bucket's name.
pub const RESERVED_BUCKET_NAMES: [&str; 8] = [
    "list", "copy", "move", "sign", "public", "upload", "resolve", "session",
];

/// A bucket's new name (create, rename): valid and not reserved.
fn check_new_name(name: &str) -> Result<(), Failure> {
    check_name(name)?;
    if RESERVED_BUCKET_NAMES.contains(&name) {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "reserved_bucket_name",
            format!("`{name}` is reserved by the storage routes"),
        ));
    }
    Ok(())
}

/// `?database=`: the Postgres database (the key's main one when left out).
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DatabaseParam {
    /// Served, and on the key's database list when it has one.
    pub database: Option<String>,
}

/// A new bucket.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateBucket {
    pub name: String,
    /// Default false.
    pub public: Option<bool>,
    /// Bytes; above 0.
    pub file_size_limit: Option<i64>,
    pub allowed_mime_types: Option<Vec<String>>,
    /// Default false. Write-once: an object is uploaded once and never overwritten, moved,
    /// renamed or deleted, by anyone (409 `write_once`), so its public URL is permanent. Once
    /// true, never false again; holding objects, the bucket is neither deleted nor renamed.
    pub write_once: Option<bool>,
}

/// A bucket's changes; a field left out stays as it is.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateBucket {
    /// A new name (not of a write-once bucket holding objects).
    pub name: Option<String>,
    pub public: Option<bool>,
    pub file_size_limit: Option<i64>,
    pub allowed_mime_types: Option<Vec<String>>,
    /// Turned on at any time, never off again (409 `write_once`).
    pub write_once: Option<bool>,
}

/// A bucket as created.
#[derive(Serialize, ToSchema)]
pub struct Bucket {
    pub id: Uuid,
    pub name: String,
    pub public: bool,
    pub file_size_limit: Option<i64>,
    pub allowed_mime_types: Option<Vec<String>>,
    pub write_once: bool,
}

/// [`crate::objects::pg`]: a statement's failure, the schema's own codes kept (`CB005`, a
/// write-once refusal, is 409 `write_once`).
pub(crate) use crate::objects::pg;

pub(crate) fn body<T>(json: Result<Json<T>, JsonRejection>) -> Result<T, Failure> {
    json.map(|Json(t)| t).map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Failure::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "the body is over 16 MiB",
            )
        } else {
            Failure::invalid_request(format!("the body: {}", e.body_text()))
        }
    })
}

#[utoipa::path(
    get,
    path = "/storage/v1/bucket",
    operation_id = "listBuckets",
    tag = "storage",
    summary = "List the buckets the caller may see",
    params(DatabaseParam),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The buckets the caller's policies show, by name", body = Vec<Value>),
        (status = 401, description = "`unauthorized`", body = StorageErrorEnvelope),
        (status = 403, description = "`database_not_allowed`, `role_not_allowed`", body = StorageErrorEnvelope),
        (status = 503, description = "`storage_unavailable`, `unavailable`", body = StorageErrorEnvelope),
    ),
)]
pub async fn list<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Query(q): Query<DatabaseParam>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let (text, _) = state
        .engine
        .as_caller(&data, q.database.as_deref(), &rid, async |c, _| {
            let row = c
                .query_one(
                    "SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.to_jsonb(b) ORDER BY b.name), '[]')::pg_catalog.text FROM storage.buckets b",
                    &[],
                )
                .await
                .map_err(pg(&rid))?;
            Ok(row.get::<_, String>(0))
        })
        .await?;
    let list: Value =
        serde_json::from_str(&text).map_err(|e| Failure(QueryFailure::internal(&rid, e)))?;
    Ok(Json(list).into_response())
}

#[utoipa::path(
    post,
    path = "/storage/v1/bucket",
    operation_id = "createBucket",
    tag = "storage",
    summary = "Create a bucket",
    description = "Runs as the key's role: an insert policy on `storage.buckets` decides.",
    params(DatabaseParam),
    request_body = CreateBucket,
    security(("bearer" = [])),
    responses(
        (status = 201, description = "Created", body = Bucket),
        (status = 400, description = "`invalid_request`, `invalid_value`, `reserved_bucket_name`", body = StorageErrorEnvelope),
        (status = 403, description = "`permission_denied` (no policy allows it), `database_not_allowed`, `role_not_allowed`", body = StorageErrorEnvelope),
        (status = 409, description = "`conflict`: a bucket of that name exists", body = StorageErrorEnvelope),
        (status = 503, description = "`storage_unavailable`, `unavailable`", body = StorageErrorEnvelope),
    ),
)]
pub async fn create<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Query(q): Query<DatabaseParam>,
    json: Result<Json<CreateBucket>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let new = body(json)?;
    check_new_name(&new.name)?;
    // No RETURNING: it would need a SELECT policy too.
    let bucket = Bucket {
        id: Uuid::new_v4(),
        name: new.name,
        public: new.public.unwrap_or(false),
        file_size_limit: new.file_size_limit,
        allowed_mime_types: new.allowed_mime_types,
        write_once: new.write_once.unwrap_or(false),
    };
    state
        .engine
        .as_caller(&data, q.database.as_deref(), &rid, async |c, _| {
            c.execute(
                "INSERT INTO storage.buckets (id, name, public, file_size_limit, allowed_mime_types, write_once) VALUES ($1, $2, $3, $4, $5, $6)",
                &[&bucket.id, &bucket.name, &bucket.public, &bucket.file_size_limit, &bucket.allowed_mime_types, &bucket.write_once],
            )
            .await
            .map_err(pg(&rid))
        })
        .await?;
    Ok((StatusCode::CREATED, Json(bucket)).into_response())
}

#[utoipa::path(
    patch,
    path = "/storage/v1/bucket/{name}",
    operation_id = "updateBucket",
    tag = "storage",
    summary = "Change a bucket",
    description = "Runs as the key's role: the update policy on `storage.buckets` decides.",
    params(("name" = String, Path, description = "The bucket"), DatabaseParam),
    request_body = UpdateBucket,
    security(("bearer" = [])),
    responses(
        (status = 204, description = "Changed"),
        (status = 400, description = "`invalid_request`, `invalid_value`, `reserved_bucket_name`", body = StorageErrorEnvelope),
        (status = 403, description = "`permission_denied`, `database_not_allowed`, `role_not_allowed`", body = StorageErrorEnvelope),
        (status = 404, description = "`bucket_not_found`: missing, or hidden from the caller", body = StorageErrorEnvelope),
        (status = 409, description = "`conflict`: the new name is taken; `write_once`: turned off, or a write-once bucket holding objects renamed", body = StorageErrorEnvelope),
        (status = 503, description = "`storage_unavailable`, `unavailable`", body = StorageErrorEnvelope),
    ),
)]
pub async fn update<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<DatabaseParam>,
    json: Result<Json<UpdateBucket>, JsonRejection>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    let change = body(json)?;
    check_name(&name)?;
    if let Some(new) = &change.name {
        check_new_name(new)?;
    }
    let (changed, _) = state
        .engine
        .as_caller(&data, q.database.as_deref(), &rid, async |c, _| {
            c.execute(
                "UPDATE storage.buckets SET name = coalesce($2, name), public = coalesce($3, public), file_size_limit = coalesce($4, file_size_limit), allowed_mime_types = coalesce($5, allowed_mime_types), write_once = coalesce($6, write_once) WHERE name = $1",
                &[&name, &change.name, &change.public, &change.file_size_limit, &change.allowed_mime_types, &change.write_once],
            )
            .await
            .map_err(pg(&rid))
        })
        .await?;
    if changed == 0 {
        return Err(Failure::bucket_not_found(&name));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[utoipa::path(
    delete,
    path = "/storage/v1/bucket/{name}",
    operation_id = "deleteBucket",
    tag = "storage",
    summary = "Delete an empty bucket",
    description = "Runs as the key's role: the delete policy on `storage.buckets` decides. A bucket that still holds objects is refused (a write-once one holding objects, for good: its objects can't be deleted).",
    params(("name" = String, Path, description = "The bucket"), DatabaseParam),
    security(("bearer" = [])),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "`database_not_allowed`, `role_not_allowed`", body = StorageErrorEnvelope),
        (status = 404, description = "`bucket_not_found`: missing, or hidden from the caller", body = StorageErrorEnvelope),
        (status = 409, description = "`bucket_not_empty`: it holds objects", body = StorageErrorEnvelope),
        (status = 503, description = "`storage_unavailable`, `unavailable`", body = StorageErrorEnvelope),
    ),
)]
pub async fn delete<S: ConnSource, M: ConnSource>(
    State(state): State<Arc<AppState<S, M>>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<DatabaseParam>,
) -> Result<Response, Failure> {
    let (data, rid) = identity(&state, &headers)?;
    check_name(&name)?;
    let database = state
        .engine
        .served_database(&data, q.database.as_deref())
        .await?
        .name;
    let deleted = state
        .engine
        .as_caller(&data, Some(&database), &rid, async |c, _| {
            c.execute("DELETE FROM storage.buckets WHERE name = $1", &[&name])
                .await
                .map_err(pg(&rid))
        })
        .await;
    match deleted {
        Ok((0, _)) => Err(Failure::bucket_not_found(&name)),
        Ok(_) => Ok(StatusCode::NO_CONTENT.into_response()),
        // The objects' foreign key: the caller's delete passed its policy, but objects remain
        // (no count: it would include objects the caller can't see).
        Err(f) if is_not_empty(&f) => Err(Failure::bucket_not_empty(&name)),
        Err(f) => Err(f),
    }
}

/// A delete refused by `storage.objects`' foreign key on its bucket.
pub fn is_not_empty(f: &Failure) -> bool {
    f.0.sqlstate.as_deref() == Some("23503")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new bucket name may not be a literal segment the storage routes use where a bucket
    /// name goes (today's and the ones later milestones add): 400 `reserved_bucket_name`.
    #[test]
    fn route_segments_are_reserved_bucket_names() {
        for name in [
            "list", "copy", "move", "sign", "public", "upload", "resolve", "session",
        ] {
            let err = check_new_name(name).unwrap_err();
            assert_eq!(
                (err.0.status.as_u16(), err.0.code),
                (400, "reserved_bucket_name"),
                "{name}"
            );
        }
        assert!(check_new_name("avatars").is_ok());
        assert_eq!(check_new_name("Bad").unwrap_err().0.code, "invalid_request");
        for ep in chelabase_routes::ENDPOINTS
            .iter()
            .filter(|e| e.service == chelabase_routes::Service::Storage)
        {
            for base in ["/storage/v1/object/", "/storage/v1/bucket/", "/storage/v1/upload/"] {
                if let Some(rest) = ep.path.strip_prefix(base) {
                    let first = rest.split('/').next().unwrap();
                    if !first.starts_with('{') {
                        assert!(
                            RESERVED_BUCKET_NAMES.contains(&first),
                            "{} uses `{first}`",
                            ep.name
                        );
                    }
                }
            }
        }
    }

    /// AC: a bucket's name is checked before any statement.
    #[test]
    fn bucket_names_are_checked() {
        for ok in ["avatars", "a", "user-files.v2", "0_x", &"a".repeat(63)] {
            assert!(valid_bucket_name(ok), "{ok}");
        }
        for bad in [
            "",
            "Avatars",
            "-a",
            ".a",
            "a/b",
            "../x",
            "a b",
            "a\u{0}",
            &"a".repeat(64),
            "é",
        ] {
            assert!(!valid_bucket_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_the_foreign_key_means_not_empty() {
        let mut f = QueryFailure::new(StatusCode::CONFLICT, "conflict", "x");
        f.sqlstate = Some("23503".into());
        assert!(is_not_empty(&Failure(f.clone())));
        f.sqlstate = Some("23505".into());
        assert!(!is_not_empty(&Failure(f)));
    }
}
