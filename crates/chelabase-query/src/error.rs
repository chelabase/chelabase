//! A failure as the data API reports it (code, status, message and Postgres's detail, hint
//! and SQLSTATE), and the mapping from Postgres SQLSTATEs to it (the spec's Errors table).
//! No HTTP framework here: rest wraps [`QueryFailure`] in its own response type.

use http::StatusCode;
use tracing::{error, warn};

use crate::query::QueryError;
use crate::sql::PlanError;

/// A 503's message when the branch's database can't be reached or turns the connection
/// away. What happened goes to the log with the request id.
pub const UNREACHABLE: &str = "the branch's database can't be reached right now; try again";

/// The message when `chelabase.schema_version` can't be read; [`schema_missing_in`] names the
/// database in the text the client sees.
pub const SCHEMA_MISSING: &str = "the data API's schema is missing; it will be reinstalled";

/// [`SCHEMA_MISSING`] for `database`.
pub fn schema_missing_in(database: &str) -> String {
    format!("the data API's schema is missing in the database {database:?}; it will be reinstalled")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFailure {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub sqlstate: Option<String>,
}

impl QueryFailure {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        QueryFailure {
            status,
            code,
            message: message.into(),
            detail: None,
            hint: None,
            sqlstate: None,
        }
    }

    pub fn invalid_query(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_query", message)
    }

    /// 503 (rest answers it with `Retry-After`).
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
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

    /// A database error raised while a statement ran: the spec's table, or a 500. Only a
    /// mapped (non-503) error shows Postgres's message, detail and hint.
    pub fn from_db(err: &dyn DbFields, request_id: &str) -> Self {
        let sqlstate = err.code();
        match classify(sqlstate) {
            Some((503, _)) => {
                Self::unreachable(request_id, format!("{sqlstate}: {}", err.message()))
            }
            Some((status, code)) => QueryFailure {
                status: StatusCode::from_u16(status).expect("a valid status"),
                code,
                message: err.message().to_owned(),
                detail: err.detail().map(str::to_owned),
                hint: err.hint().map(str::to_owned),
                sqlstate: Some(sqlstate.to_owned()),
            },
            None => Self::internal(request_id, format!("{sqlstate}: {}", err.message())),
        }
    }

    /// An error from running a statement (after a connection was acquired): Postgres's error
    /// through [`Self::from_db`]; a closed connection or an I/O failure (the server gone, the
    /// network) a 503 [`UNREACHABLE`]; anything else (a value that doesn't encode or decode,
    /// an unexpected row count: a bug here) a 500.
    pub fn from_pg(err: &tokio_postgres::Error, request_id: &str) -> Self {
        if let Some(db) = err.as_db_error() {
            return Self::from_db(db, request_id);
        }
        if err.is_closed() || io_cause(err) {
            Self::unreachable(request_id, format!("{err}: {err:?}"))
        } else {
            Self::internal(request_id, format!("{err}: {err:?}"))
        }
    }
}

/// Whether an I/O error sits under `err` (reading, writing or connecting: the server or the
/// network failed, not this code).
fn io_cause(err: &tokio_postgres::Error) -> bool {
    let mut source = std::error::Error::source(err);
    while let Some(e) = source {
        if e.is::<std::io::Error>() {
            return true;
        }
        source = e.source();
    }
    false
}

/// What [`QueryFailure::from_db`] reads of a Postgres error: tokio-postgres's
/// [`DbError`](tokio_postgres::error::DbError) has no public constructor, so tests use a
/// fake.
pub trait DbFields {
    /// The SQLSTATE.
    fn code(&self) -> &str;
    fn message(&self) -> &str;
    fn detail(&self) -> Option<&str>;
    fn hint(&self) -> Option<&str>;
    fn constraint(&self) -> Option<&str>;
}

impl DbFields for tokio_postgres::error::DbError {
    fn code(&self) -> &str {
        tokio_postgres::error::DbError::code(self).code()
    }
    fn message(&self) -> &str {
        tokio_postgres::error::DbError::message(self)
    }
    fn detail(&self) -> Option<&str> {
        tokio_postgres::error::DbError::detail(self)
    }
    fn hint(&self) -> Option<&str> {
        tokio_postgres::error::DbError::hint(self)
    }
    fn constraint(&self) -> Option<&str> {
        tokio_postgres::error::DbError::constraint(self)
    }
}

impl From<QueryError> for QueryFailure {
    fn from(e: QueryError) -> Self {
        Self::invalid_query(e.message)
    }
}

impl From<PlanError> for QueryFailure {
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

    /// A database error as Postgres would send it (`DbError` has no public constructor).
    #[derive(Debug, Default)]
    pub(crate) struct FakeDb {
        pub code: &'static str,
        pub message: &'static str,
        pub detail: Option<&'static str>,
        pub hint: Option<&'static str>,
        pub constraint: Option<&'static str>,
    }

    impl FakeDb {
        pub(crate) fn new(code: &'static str, message: &'static str) -> Self {
            FakeDb {
                code,
                message,
                ..Default::default()
            }
        }
    }

    impl DbFields for FakeDb {
        fn code(&self) -> &str {
            self.code
        }
        fn message(&self) -> &str {
            self.message
        }
        fn detail(&self) -> Option<&str> {
            self.detail
        }
        fn hint(&self) -> Option<&str> {
            self.hint
        }
        fn constraint(&self) -> Option<&str> {
            self.constraint
        }
    }

    /// A port nothing listens on (bound, then released).
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    fn config(port: u16) -> tokio_postgres::Config {
        let mut config = tokio_postgres::Config::new();
        config.host("127.0.0.1").port(port).user("x").dbname("x");
        config
    }

    /// A client whose server went away: a listener that answers the startup message with
    /// `AuthenticationOk` and `ReadyForQuery`, then drops the socket. The connection task
    /// has ended when this returns.
    pub(crate) async fn closed_client() -> tokio_postgres::Client {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let len = socket.read_i32().await.unwrap();
            let mut startup = vec![0; usize::try_from(len).unwrap() - 4];
            socket.read_exact(&mut startup).await.unwrap();
            // AuthenticationOk, then ReadyForQuery (idle).
            socket
                .write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'Z', 0, 0, 0, 5, b'I'])
                .await
                .unwrap();
            socket.flush().await.unwrap();
        });
        let (client, connection) = config(port)
            .connect(tokio_postgres::NoTls)
            .await
            .expect("the fake server's handshake");
        server.await.unwrap();
        // The connection ends when it reads the server's end of the stream.
        let _ = connection.await;
        client
    }

    /// A connection closed under a request (the server gone, the pool's connection dropped)
    /// is the database being out of reach for a moment: a 503 the client retries, not a 500.
    #[tokio::test]
    async fn a_closed_connection_is_unavailable() {
        let client = closed_client().await;
        let err = client.batch_execute("SELECT 1").await.unwrap_err();
        assert!(err.is_closed(), "{err:?}");
        let e = QueryFailure::from_pg(&err, "rid-1");
        assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!((e.code, e.message.as_str()), ("unavailable", UNREACHABLE));
    }

    /// A driver error that isn't the database or the network (here a value that doesn't
    /// encode as its declared type) is a bug on this side: a 500, not a retryable 503.
    #[tokio::test]
    async fn a_driver_error_here_is_internal() {
        let client = closed_client().await;
        let err = client
            .query_typed(
                "SELECT $1",
                &[(&"seven", tokio_postgres::types::Type::INT8)],
            )
            .await
            .unwrap_err();
        assert!(!err.is_closed() && err.as_db_error().is_none(), "{err:?}");
        let e = QueryFailure::from_pg(&err, "rid-1");
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR, "{err:?}");
        assert_eq!(e.message, "internal error (request id rid-1)");
    }

    /// A 503's body is a fixed message: no address, no driver or Postgres text.
    #[tokio::test]
    async fn unavailable_bodies_are_fixed() {
        const SECRET: &str = "10.1.2.3:5432 password authentication failed for user x";
        // A refused connection: an I/O error under the driver's.
        let refused = config(free_port().await)
            .connect(tokio_postgres::NoTls)
            .await
            .map(|_| ())
            .unwrap_err();
        let e = QueryFailure::from_pg(&refused, "rid-1");
        assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE, "{refused:?}");
        assert_eq!(e.message, UNREACHABLE);
        for code in ["08006", "28P01", "57P01", "53300"] {
            let db = FakeDb {
                detail: Some(SECRET),
                hint: Some(SECRET),
                ..FakeDb::new(code, SECRET)
            };
            let e = QueryFailure::from_db(&db, "rid-1");
            assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE, "{code}");
            assert_eq!(e.message, UNREACHABLE, "{code}");
            assert_eq!((e.detail, e.hint), (None, None), "{code}");
        }
    }

    /// A mapped error carries Postgres's message, detail, hint and SQLSTATE; an unmapped one
    /// is a 500 that hides them.
    #[test]
    fn a_db_error_keeps_its_detail_and_hint() {
        let db = FakeDb {
            detail: Some("Key (id)=(1) already exists."),
            hint: Some("pick another id"),
            constraint: Some("items_pkey"),
            ..FakeDb::new(
                "23505",
                "duplicate key value violates unique constraint \"items_pkey\"",
            )
        };
        let e = QueryFailure::from_db(&db, "rid-1");
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert_eq!(e.code, "conflict");
        assert_eq!(e.message, db.message);
        assert_eq!(e.detail.as_deref(), db.detail);
        assert_eq!(e.hint.as_deref(), db.hint);
        assert_eq!(e.sqlstate.as_deref(), Some("23505"));

        let e = QueryFailure::from_db(&FakeDb::new("XX000", "secret internals"), "rid-1");
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!e.message.contains("secret"), "{}", e.message);
        assert_eq!((e.detail, e.hint, e.sqlstate), (None, None, None));
    }
}
