//! The storage service's error body: the data API's envelope,
//! `{"error":{"code","message","detail"?,"hint"?,"sqlstate"?}}`, built on
//! [`QueryFailure`] (whose SQLSTATE mapping a statement's failure goes through).

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chelabase_pgsession::{AcquireError, ConnError};
use chelabase_query::QueryFailure;
use serde_json::{Map, Value, json};

/// Seconds a 503 asks the client to wait.
pub const RETRY_AFTER_SECS: u32 = 2;

/// A failed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure(pub QueryFailure);

impl Failure {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Failure(QueryFailure::new(status, code, message))
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    /// 503: the database's `storage` schema isn't Chelabase's current one (a user's own, a
    /// missing or unknown stamp), or storage's own login can't reach it.
    pub fn storage_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "object storage isn't available in this database right now",
        )
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    /// 403: the database isn't served or isn't on the key's list (one text for both).
    pub fn database_not_allowed() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "database_not_allowed",
            "the database doesn't exist or this key may not use it",
        )
    }

    /// 403: a secret key's role is owner-level (as the data API refuses it).
    pub fn role_not_allowed(role: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "role_not_allowed",
            format!("the role `{role}` of this key is owner-level or has system privileges"),
        )
    }

    /// 404: no bucket of that name that the caller may see (missing or hidden alike).
    pub fn bucket_not_found(name: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "bucket_not_found",
            format!("no bucket `{name}`"),
        )
    }

    /// 409: the bucket still holds `objects` objects.
    pub fn bucket_not_empty(name: &str) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "bucket_not_empty",
            format!("bucket `{name}` still holds objects; delete them first"),
        )
    }

    /// 400: an object name that isn't allowed (checked before any statement).
    pub fn invalid_name() -> Self {
        Self::invalid_request(
            "an object name is 1 to 1024 bytes of UTF-8 with no control characters, no leading `/`, and no empty or `..` segment",
        )
    }

    /// 404: no object of that name the caller may see (missing or hidden alike).
    pub fn object_not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "object_not_found", "no such object")
    }

    /// 409: someone else changed the object (or took the name) first.
    pub fn object_conflict() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "conflict",
            "the object changed or was created meanwhile; try again",
        )
    }

    /// 413: the body is over the bucket's file size limit.
    pub fn too_large(limit: i64) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("the object is over the bucket's limit of {limit} bytes"),
        )
    }

    pub fn range_not_satisfiable() -> Self {
        Self::new(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "range_not_satisfiable",
            "the range is outside the object",
        )
    }

    /// 502: the object store failed (the caller logs why).
    pub fn store_failed() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "storage_error",
            "the object store failed; try again",
        )
    }

    /// 403 on the object routes: row level security refused the operation, or the caller
    /// can't see the bucket. One answer for both, whether the bucket exists or not.
    pub fn not_allowed() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "not_allowed",
            "the caller may not do this in this bucket",
        )
    }

    /// An object route's statement failure: RLS's 42501 becomes [`Self::not_allowed`], so a
    /// refusal can't be told apart from a bucket the caller can't see.
    pub fn refusal(self) -> Self {
        if self.0.sqlstate.as_deref() == Some("42501") {
            Self::not_allowed()
        } else {
            self
        }
    }

    /// The object store failed (logged with the request id).
    pub fn from_store(e: &chelabase_storage::StorageError, request_id: &str) -> Self {
        match e {
            chelabase_storage::StorageError::NotFound => Self::object_not_found(),
            other => {
                tracing::warn!(request_id, "the object store: {other}");
                Self::new(
                    StatusCode::BAD_GATEWAY,
                    "storage_error",
                    "the object store failed; try again",
                )
            }
        }
    }

    /// No connection to the branch's database.
    pub fn from_acquire(e: &AcquireError, request_id: &str) -> Self {
        match e {
            AcquireError::NotServed => Self::database_not_allowed(),
            AcquireError::Conn(e) => Self::from_conn(e),
            AcquireError::Db(e) => {
                tracing::warn!(request_id, "connecting to the branch: {e}");
                Self::unavailable(chelabase_query::error::UNREACHABLE)
            }
            AcquireError::Busy | AcquireError::TimedOut => {
                Self::unavailable("too many connections in use; try again")
            }
        }
    }

    pub fn from_conn(e: &ConnError) -> Self {
        match e {
            ConnError::UnknownBranch => Self::unauthorized("the key's branch is gone"),
            ConnError::NotReady(status) => Self::unavailable(format!("the branch is {status}")),
            ConnError::Unreachable(_) => {
                Self::unavailable("storage can't look up the branch right now; try again")
            }
        }
    }
}

impl From<QueryFailure> for Failure {
    fn from(f: QueryFailure) -> Self {
        Failure(f)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let f = self.0;
        let mut body = Map::new();
        body.insert("code".into(), json!(f.code));
        body.insert("message".into(), json!(f.message));
        for (key, value) in [
            ("detail", f.detail),
            ("hint", f.hint),
            ("sqlstate", f.sqlstate),
        ] {
            if let Some(value) = value {
                body.insert(key.into(), Value::String(value));
            }
        }
        let mut response = (f.status, Json(json!({ "error": body }))).into_response();
        if f.status == StatusCode::SERVICE_UNAVAILABLE {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(RETRY_AFTER_SECS));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body(f: Failure) -> (StatusCode, Option<HeaderValue>, Value) {
        let response = f.into_response();
        let status = response.status();
        let retry = response.headers().get(header::RETRY_AFTER).cloned();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        (status, retry, serde_json::from_slice(&bytes).unwrap())
    }

    /// AC 6: every route answers a 503 `storage_unavailable` (with `Retry-After`) for a schema
    /// that isn't Chelabase's; the envelope is the data API's.
    #[tokio::test]
    async fn storage_unavailable_is_a_503_in_the_data_api_envelope() {
        let (status, retry, json) = body(Failure::storage_unavailable()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(retry, Some(HeaderValue::from(RETRY_AFTER_SECS)));
        assert_eq!(json["error"]["code"], "storage_unavailable");
        assert!(json["error"].get("sqlstate").is_none());
    }

    /// RLS refusing a write is Postgres's 42501: a 403 carrying the SQLSTATE.
    #[tokio::test]
    async fn a_policy_refusal_is_a_403() {
        let mut f = QueryFailure::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "new row violates row-level security policy",
        );
        f.sqlstate = Some("42501".into());
        let (status, retry, json) = body(f.into()).await;
        assert_eq!((status, retry), (StatusCode::FORBIDDEN, None));
        assert_eq!(json["error"]["sqlstate"], "42501");
        // N5: no count (it would include objects the caller can't see).
        let (status, _, json) = body(Failure::bucket_not_empty("avatars")).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let message = json["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("still holds objects") && !message.chars().any(|c| c.is_ascii_digit()),
            "{message}"
        );
        let (status, _, _) = body(Failure::from_acquire(&AcquireError::NotServed, "r")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    /// A bucket the caller can't see (missing or hidden alike) and an RLS refusal of the object
    /// operation answer the same bytes.
    #[tokio::test]
    async fn a_hidden_bucket_and_a_refusal_answer_alike() {
        let mut rls = QueryFailure::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "new row violates row-level security policy for table \"objects\"",
        );
        rls.sqlstate = Some("42501".into());
        let refused = Failure(rls).refusal().into_response();
        let hidden = Failure::not_allowed().into_response();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let bytes =
            |r: Response| async { axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap() };
        assert_eq!(bytes(refused).await, bytes(hidden).await);
        // Anything else keeps its own answer.
        let other = Failure::object_conflict().refusal();
        assert_eq!(other.0.code, "conflict");
    }
}
