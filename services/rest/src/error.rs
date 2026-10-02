//! The data API's error body, `{"error":{"code","message","detail"?,"hint"?,"sqlstate"?}}`,
//! and the mapping from Postgres SQLSTATEs to it (the spec's Errors table).

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use sqlx::error::DatabaseError;
use tracing::{error, warn};

use crate::query::QueryError;
use crate::sql::PlanError;

/// Seconds a 503 asks the client to wait.
pub const RETRY_AFTER_SECS: u32 = 2;

/// A 503's message when the branch's database can't be reached or turns the connection
/// away. What happened goes to the log with the request id.
pub const UNREACHABLE: &str = "the branch's database can't be reached right now; try again";

/// The message when `cheladb.schema_version` can't be read.
pub const SCHEMA_MISSING: &str = "the data API's schema is missing; it will be reinstalled";

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

    pub fn too_large() -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "the body is over 1 MiB",
        )
    }

    /// A 503 with [`UNREACHABLE`]: `detail` goes to the log with the request id.
    pub fn unreachable(request_id: &str, detail: impl std::fmt::Display) -> Self {
        warn!(request_id, "the branch's database is unavailable: {detail}");
        Self::unavailable(UNREACHABLE)
    }

    /// A 500: the details go to the log with the request id, never to the client.
    pub fn internal(request_id: &str, detail: impl std::fmt::Display) -> Self {
        error!(request_id, "internal error: {detail}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("internal error (request id {request_id})"),
        )
    }

    /// A database error raised while a statement ran: the spec's table, or a 500.
    pub fn from_database(err: &dyn DatabaseError, request_id: &str) -> Self {
        let pg = err.try_downcast_ref::<sqlx::postgres::PgDatabaseError>();
        let sqlstate = err.code().map(|c| c.into_owned()).unwrap_or_default();
        match classify(&sqlstate) {
            Some((503, _)) => {
                Self::unreachable(request_id, format!("{sqlstate}: {}", err.message()))
            }
            Some((status, code)) => ApiError {
                status: StatusCode::from_u16(status).expect("a valid status"),
                code,
                message: err.message().to_owned(),
                detail: pg.and_then(|e| e.detail()).map(str::to_owned),
                hint: pg.and_then(|e| e.hint()).map(str::to_owned),
                sqlstate: Some(sqlstate),
            },
            None => Self::internal(request_id, format!("{sqlstate}: {}", err.message())),
        }
    }

    /// An error from running a statement (after a connection was acquired).
    pub fn from_sqlx(err: sqlx::Error, request_id: &str) -> Self {
        match &err {
            sqlx::Error::Database(db) => Self::from_database(db.as_ref(), request_id),
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolTimedOut => {
                Self::unreachable(request_id, &err)
            }
            _ => Self::internal(request_id, &err),
        }
    }
}

impl From<QueryError> for ApiError {
    fn from(e: QueryError) -> Self {
        Self::invalid_query(e.message)
    }
}

impl From<PlanError> for ApiError {
    fn from(e: PlanError) -> Self {
        let status = match e {
            PlanError::UnknownTable(_) | PlanError::UnknownFunction(_) => StatusCode::NOT_FOUND,
            PlanError::UnknownColumn { .. } | PlanError::InvalidQuery(_) => StatusCode::BAD_REQUEST,
        };
        Self::new(status, e.code(), e.to_string())
    }
}

/// The status and code for a SQLSTATE, or `None` for a 500 `internal`.
///
/// The spec's table (57014 and 25P04, the transaction timeout, are both `timeout`), plus
/// the cases its "compute unreachable" row covers: connection
/// exceptions (class 08), a shutting-down server (57P01-57P03), refused credentials
/// (class 28) and insufficient resources (class 53) are 503 `unavailable`.
pub fn classify(sqlstate: &str) -> Option<(u16, &'static str)> {
    Some(match sqlstate {
        "42501" => (403, "permission_denied"),
        "23505" | "23503" | "23P01" => (409, "conflict"),
        "23502" | "23514" => (400, "invalid_value"),
        s if s.len() == 5 && s.starts_with("22") => (400, "invalid_value"),
        "P0001" => (400, "raised"),
        "40001" | "40P01" => (409, "retry"),
        "57014" | "25P04" => (504, "timeout"),
        "57P01" | "57P02" | "57P03" => (503, "unavailable"),
        // Connection exceptions, refused credentials and insufficient resources (too many
        // connections, out of memory or disk).
        s if s.len() == 5
            && (s.starts_with("08") || s.starts_with("28") || s.starts_with("53")) =>
        {
            (503, "unavailable")
        }
        _ => return None,
    })
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
pub(crate) mod tests {
    use super::*;

    /// Every row of the spec's Errors table that comes from Postgres.
    #[test]
    fn sqlstate_mapping_table() {
        let rows: &[(&str, u16, &str)] = &[
            ("42501", 403, "permission_denied"),
            ("23505", 409, "conflict"),
            ("23503", 409, "conflict"),
            ("23P01", 409, "conflict"),
            ("23502", 400, "invalid_value"),
            ("23514", 400, "invalid_value"),
            ("22001", 400, "invalid_value"),
            ("22003", 400, "invalid_value"),
            ("22P02", 400, "invalid_value"),
            ("22023", 400, "invalid_value"),
            ("P0001", 400, "raised"),
            ("40001", 409, "retry"),
            ("40P01", 409, "retry"),
            ("57014", 504, "timeout"),
            ("25P04", 504, "timeout"),
            ("08006", 503, "unavailable"),
            ("57P01", 503, "unavailable"),
            ("28P01", 503, "unavailable"),
            ("53300", 503, "unavailable"),
            ("53200", 503, "unavailable"),
            ("53100", 503, "unavailable"),
        ];
        for &(sqlstate, status, code) in rows {
            assert_eq!(classify(sqlstate), Some((status, code)), "{sqlstate}");
        }
        // Anything else is a 500.
        for sqlstate in ["42P01", "42703", "23000", "XX000", "0A000", "P0002", ""] {
            assert_eq!(classify(sqlstate), None, "{sqlstate}");
        }
    }

    /// A database error with a code and a message, as Postgres would send it.
    #[derive(Debug)]
    pub(crate) struct FakeDb {
        pub code: &'static str,
        pub message: &'static str,
    }

    impl std::fmt::Display for FakeDb {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for FakeDb {}

    impl DatabaseError for FakeDb {
        fn message(&self) -> &str {
            self.message
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.code.into())
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    /// A 503's body is a fixed message: no address, no sqlx or Postgres text.
    #[test]
    fn unavailable_bodies_are_fixed() {
        const SECRET: &str = "10.1.2.3:5432 password authentication failed for user x";
        let io = sqlx::Error::Io(std::io::Error::other(SECRET));
        let e = ApiError::from_sqlx(io, "rid-1");
        assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.message, UNREACHABLE);
        for code in ["08006", "28P01", "57P01", "53300"] {
            let db = FakeDb {
                code,
                message: SECRET,
            };
            let e = ApiError::from_database(&db, "rid-1");
            assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE, "{code}");
            assert_eq!(e.message, UNREACHABLE, "{code}");
            assert_eq!((e.detail, e.hint), (None, None), "{code}");
        }
    }

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
