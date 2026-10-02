//! The data API's error body, `{"error":{"code","message","detail"?,"hint"?,"sqlstate"?}}`.
//! The mapping from Postgres SQLSTATEs to it lives in [`cheladb_query::error`]; this is its
//! HTTP edge.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use sqlx::error::DatabaseError;

pub use cheladb_query::error::{SCHEMA_MISSING, UNREACHABLE, classify};
use cheladb_query::error::QueryFailure;
use cheladb_query::query::QueryError;
use cheladb_query::sql::PlanError;

/// [`ApiError::unknown_database`]'s text.
pub const UNKNOWN_DATABASE: &str = "the database doesn't exist or isn't served by this key";

/// Seconds a 503 asks the client to wait.
pub const RETRY_AFTER_SECS: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub sqlstate: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
            detail: None,
            hint: None,
            sqlstate: None,
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn invalid_query(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_query", message)
    }

    /// 503, answered with `Retry-After`.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    /// 403: a secret key whose role can bypass row-level security.
    pub fn role_not_allowed(role: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "role_not_allowed",
            format!(
                "the role `{role}` of this key can bypass row-level security; make it plain or use another role"
            ),
        )
    }

    /// 404: a database that doesn't exist, isn't served or isn't on the key's list (one
    /// text for all three, so a key can't discover database names).
    pub fn unknown_database() -> Self {
        Self::new(StatusCode::NOT_FOUND, "unknown_database", UNKNOWN_DATABASE)
    }

    /// 404 on the console route: the role it names doesn't exist.
    pub fn unknown_role(role: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_role",
            format!("the role `{role}` doesn't exist"),
        )
    }

    pub fn too_large() -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "the body is over 1 MiB",
        )
    }

    /// A 503 with [`UNREACHABLE`]: `detail` goes to the log with the request id.
    pub fn unreachable(request_id: &str, detail: impl std::fmt::Display) -> Self {
        QueryFailure::unreachable(request_id, detail).into()
    }

    /// A 500: the details go to the log with the request id, never to the client.
    pub fn internal(request_id: &str, detail: impl std::fmt::Display) -> Self {
        QueryFailure::internal(request_id, detail).into()
    }

    /// A database error raised while a statement ran: the spec's table, or a 500.
    pub fn from_database(err: &dyn DatabaseError, request_id: &str) -> Self {
        QueryFailure::from_database(err, request_id).into()
    }

    /// An error from running a statement (after a connection was acquired).
    pub fn from_sqlx(err: sqlx::Error, request_id: &str) -> Self {
        QueryFailure::from_sqlx(err, request_id).into()
    }
}

impl From<QueryFailure> for ApiError {
    fn from(e: QueryFailure) -> Self {
        ApiError {
            status: e.status,
            code: e.code,
            message: e.message,
            detail: e.detail,
            hint: e.hint,
            sqlstate: e.sqlstate,
        }
    }
}

impl From<QueryError> for ApiError {
    fn from(e: QueryError) -> Self {
        QueryFailure::from(e).into()
    }
}

impl From<PlanError> for ApiError {
    fn from(e: PlanError) -> Self {
        QueryFailure::from(e).into()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = Map::new();
        error.insert("code".into(), json!(self.code));
        error.insert("message".into(), json!(self.message));
        for (key, value) in [
            ("detail", self.detail),
            ("hint", self.hint),
            ("sqlstate", self.sqlstate),
        ] {
            if let Some(value) = value {
                error.insert(key.into(), Value::String(value));
            }
        }
        let mut response = (self.status, Json(json!({ "error": error }))).into_response();
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
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

    #[test]
    fn body_has_only_the_fields_set_and_503_asks_to_retry() {
        let response = ApiError::unavailable("down").into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "2");
        let response = ApiError::invalid_query("bad").into_response();
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
    }
    #[test]
    fn role_not_allowed_body() {
        let e = ApiError::role_not_allowed("svc");
        assert_eq!(e.status, StatusCode::FORBIDDEN);
        assert_eq!(e.code, "role_not_allowed");
        assert_eq!(
            e.message,
            "the role `svc` of this key can bypass row-level security; make it plain or use another role"
        );
    }

}
