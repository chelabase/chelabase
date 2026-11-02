//! The per-request role session: a pooled connection lent to one call as one role, with that
//! role's JWT claims. It is the only way to reach a connection of [`Pools`](crate::Pools):
//! [`Conn`] keeps its client to itself, and a [`RoleSession`] exists only inside
//! [`Conn::with_role_session`], so no caller runs a statement without naming a role, and
//! nothing defaults to the connection's own user (no owner path).
//!
//! The session's SQL is the data API's, for both of [`chelabase_query::run`]'s paths: each
//! transaction opens with `[ROLLBACK;] BEGIN; RESET_SQL`
//! ([`chelabase_query::engine::begin_sql`]), sets the role, claims and timeouts in its
//! preamble ([`RoleSession::run_as`]; sequential `PREAMBLE_SQL`, pipelined
//! `GUARDED_PREAMBLE_SQL`) and ends with `COMMIT` (or `ROLLBACK`) and `RESET_SQL`
//! ([`RoleSession::settings`] carries the reset and the timeouts). A reset that fails marks
//! the lease broken, and a broken or unfinished lease closes its connection instead of
//! pooling it.
//!
//! No role, no run: a connection's client can't be read, and a [`Conn`] is no
//! [`chelabase_query::QueryClient`].
//!
//! ```compile_fail,E0616
//! fn no_role_no_run(conn: chelabase_pgsession::Conn) {
//!     let _client = conn.client;
//! }
//! ```
//!
//! ```compile_fail,E0277
//! fn no_role_no_run(conn: &chelabase_pgsession::Conn) {
//!     fn runs_on<C: chelabase_query::QueryClient>(_: &C) {}
//!     runs_on(conn);
//! }
//! ```
//!
//! A [`RoleSession`] can't be built or named into being outside the crate:
//!
//! ```compile_fail,E0451
//! fn forged(
//!     lease: chelabase_query::Lease,
//!     info: std::sync::Arc<chelabase_pgsession::ConnInfo>,
//!     claims: serde_json::Value,
//! ) -> chelabase_pgsession::RoleSession {
//!     chelabase_pgsession::RoleSession {
//!         lease,
//!         info,
//!         role: "postgres".into(),
//!         claims,
//!         oid: None,
//!     }
//! }
//! ```
//!
//! ```compile_fail,E0599
//! fn forged(lease: chelabase_query::Lease) {
//!     let _ = chelabase_pgsession::RoleSession::new(lease);
//! }
//! ```
//!
//! The one way in names the role and its claims:
//!
//! ```no_run
//! # async fn f(pools: &chelabase_pgsession::Pools<chelabase_pgsession::InternalSource>) {
//! let conn = pools.acquire(uuid::Uuid::nil(), "main").await.unwrap();
//! let claims = serde_json::json!({ "sub": "u1" });
//! conn.with_role_session("authenticated", &claims, None, async |session| {
//!     let _run_as = session.run_as(); // always `RunAs::Role`
//! })
//! .await;
//! # }
//! ```

use std::sync::Arc;

use chelabase_query::{Lease, RunAs, RunSettings};
use serde_json::Value;

use crate::pools::{Conn, ConnInfo, RESET_SQL, ServedDatabase};

/// The request limits from the branch's connection info, for `database`: its rows and
/// timeout, its `default_schema`, and [`RESET_SQL`] as the session's reset.
pub fn run_settings(info: &ConnInfo, database: &ServedDatabase) -> RunSettings {
    RunSettings {
        max_rows: info.settings.max_rows,
        statement_timeout_ms: info.settings.statement_timeout_ms,
        default_schema: database.default_schema.clone(),
        reset_sql: RESET_SQL,
    }
}

/// A connection lent to one call as `role` with `claims` (see the module docs). Statements
/// go through [`chelabase_query`] with [`Self::lease`] and [`Self::run_as`]. It holds its
/// own copy of the role and claims (a borrowed one makes the session's future fail the
/// `Send` check axum's handlers need).
pub struct RoleSession {
    lease: Lease,
    info: Arc<ConnInfo>,
    role: String,
    claims: Value,
    oid: Option<i64>,
}

impl RoleSession {
    /// The leased connection, as `authenticator` until a preamble sets the role: the owner
    /// check and catalog reads a caller makes before its transaction run on it.
    pub fn lease(&self) -> &Lease {
        &self.lease
    }

    /// The branch's connection info the connection was opened with.
    pub fn info(&self) -> &Arc<ConnInfo> {
        &self.info
    }

    /// Who every transaction of the session runs as: always the session's role and claims,
    /// held to `oid` when it has one.
    pub fn run_as(&self) -> RunAs<'_> {
        RunAs::Role {
            role: &self.role,
            claims: &self.claims,
            oid: self.oid,
        }
    }

    /// The limits and reset for a request in `database` ([`run_settings`]).
    pub fn settings(&self, database: &ServedDatabase) -> RunSettings {
        run_settings(&self.info, database)
    }
}

impl Conn {
    /// Runs `f` with this connection as a session of `role` with `claims` (`oid`: the role's
    /// identity a secret key is bound to, `None` to check the name only). When `f` returns
    /// (an error it answers with included), the connection goes back to its pool unless a
    /// reset failed; when `f` is cancelled or panics, the connection is closed. Its slots are
    /// released after.
    pub async fn with_role_session<T>(
        self,
        role: &str,
        claims: &Value,
        oid: Option<i64>,
        f: impl AsyncFnOnce(&RoleSession) -> T,
    ) -> T {
        let Conn {
            client,
            info,
            permits,
        } = self;
        // Declared after `permits`: dropped (pooled or closed) before they're released.
        let mut session = RoleSession {
            lease: Lease::new(client),
            info,
            role: role.to_owned(),
            claims: claims.clone(),
            oid,
        };
        let out = f(&session).await;
        session.lease.done();
        drop(session);
        drop(permits);
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pools::{ConnError, ConnInfo, ConnSource, Pools, RESET_SQL};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_postgres::TransactionStatus;
    use uuid::Uuid;

    /// The session's SQL as it was in `services/rest` before the move, byte for byte.
    const PRE_MOVE_RESET: &str = "SET ROLE NONE; RESET ALL";
    const PRE_MOVE_PREAMBLE: &str = "SELECT pg_catalog.set_config('role', CASE WHEN $4::pg_catalog.int8 IS NULL OR \
     (SELECT r.oid::pg_catalog.int8 FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) $1) OPERATOR(pg_catalog.=) $4::pg_catalog.int8 \
     THEN $1 ELSE '' END, true), \
     pg_catalog.set_config('request.jwt.claims', $2, true), pg_catalog.set_config('statement_timeout', $3, true), \
     pg_catalog.set_config('transaction_timeout', $3, true), \
     (SELECT version::pg_catalog.text FROM chelabase.schema_version)";
    const PRE_MOVE_GUARDED_PREAMBLE: &str = "SELECT pg_catalog.set_config('role', CASE WHEN $4::pg_catalog.int8 IS NULL OR \
     (SELECT r.oid::pg_catalog.int8 FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) $1) OPERATOR(pg_catalog.=) $4::pg_catalog.int8 \
     THEN $1 ELSE '' END, true), \
     pg_catalog.set_config('request.jwt.claims', $2, true), pg_catalog.set_config('statement_timeout', $3, true), \
     pg_catalog.set_config('transaction_timeout', $3, true), \
     chelabase.assert_schema_version($5)";

    /// A Postgres stand-in that records every simple query and answers it `OK`, or with an
    /// error when `fails` says so (given the query and how many resets it has seen,
    /// this one included). It tracks the transaction status the client reads.
    pub(crate) struct Recorder {
        pub(crate) port: u16,
        pub(crate) queries: Arc<Mutex<Vec<String>>>,
        /// Sockets open now.
        pub(crate) open: Arc<AtomicUsize>,
    }

    fn message(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn error_body() -> Vec<u8> {
        let mut e = Vec::new();
        for (f, v) in [(b'S', "ERROR"), (b'C', "XX000"), (b'M', "refused")] {
            e.push(f);
            e.extend_from_slice(v.as_bytes());
            e.push(0);
        }
        e.push(0);
        e
    }

    pub(crate) async fn recorder(fails: fn(&str, usize) -> bool) -> Recorder {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let queries = Arc::new(Mutex::new(Vec::new()));
        let open = Arc::new(AtomicUsize::new(0));
        let resets = Arc::new(AtomicUsize::new(0));
        let (q, o) = (queries.clone(), open.clone());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (queries, open, resets) = (q.clone(), o.clone(), resets.clone());
                tokio::spawn(async move {
                    open.fetch_add(1, Ordering::SeqCst);
                    let Ok(len) = socket.read_i32().await else {
                        open.fetch_sub(1, Ordering::SeqCst);
                        return;
                    };
                    let mut startup = vec![0; usize::try_from(len).unwrap() - 4];
                    let _ = socket.read_exact(&mut startup).await;
                    let mut hello = message(b'R', &0_i32.to_be_bytes());
                    hello.extend(message(b'Z', b"I"));
                    let _ = socket.write_all(&hello).await;
                    let mut status = b'I';
                    while let Ok(kind) = socket.read_u8().await {
                        let Ok(len) = socket.read_i32().await else {
                            break;
                        };
                        let mut body = vec![0; usize::try_from(len).unwrap() - 4];
                        if socket.read_exact(&mut body).await.is_err() || kind == b'X' {
                            break;
                        }
                        // The extended protocol (a prepare) is refused at its `Sync`.
                        if kind == b'S' {
                            let mut out = message(b'E', &error_body());
                            out.extend(message(b'Z', &[status]));
                            if socket.write_all(&out).await.is_err() {
                                break;
                            }
                        }
                        if kind != b'Q' {
                            continue;
                        }
                        let sql = String::from_utf8_lossy(&body[..body.len() - 1]).into_owned();
                        queries.lock().unwrap().push(sql.clone());
                        let nth = if sql.contains("RESET ALL") {
                            resets.fetch_add(1, Ordering::SeqCst) + 1
                        } else {
                            resets.load(Ordering::SeqCst)
                        };
                        let last = ["BEGIN", "COMMIT", "ROLLBACK"]
                            .iter()
                            .filter_map(|k| sql.rfind(k).map(|i| (i, *k)))
                            .max();
                        let out = if fails(&sql, nth) {
                            if status != b'I' || last.is_some_and(|(_, k)| k == "BEGIN") {
                                status = b'E';
                            }
                            message(b'E', &error_body())
                        } else {
                            match last {
                                Some((_, "BEGIN")) => status = b'T',
                                Some(_) => status = b'I',
                                None => {}
                            }
                            message(b'C', b"OK\0")
                        };
                        let mut out = out;
                        out.extend(message(b'Z', &[status]));
                        if socket.write_all(&out).await.is_err() {
                            break;
                        }
                    }
                    open.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Recorder {
            port,
            queries,
            open,
        }
    }

    struct At(u16);

    impl ConnSource for At {
        async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
            let mut info = crate::pools::test_support::two_databases();
            info.port = self.0;
            Ok(info)
        }
    }

    /// Waits (bounded) until `open` sockets are open.
    async fn settle(open: &AtomicUsize, want: usize) -> usize {
        for _ in 0..200 {
            if open.load(Ordering::SeqCst) == want {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        open.load(Ordering::SeqCst)
    }

    /// A session's future can run on a multi-threaded runtime (axum's handlers need it).
    #[allow(dead_code)]
    fn a_session_is_send(conn: Conn, claims: &serde_json::Value) {
        fn send<T: Send>(_: T) {}
        send(conn.with_role_session("r", claims, None, async |s| {
            let _ = s.lease();
            tokio::task::yield_now().await;
        }));
    }

    /// The session SQL every request sends is the strings rest sent before the move, on
    /// both paths: the reset, the transaction's opening and the owner check's clean-up, and
    /// the sequential (`PREAMBLE_SQL`) and pipelined (`GUARDED_PREAMBLE_SQL`) preambles,
    /// which set the role, the claims and the timeouts with the same text.
    #[test]
    fn session_sql_matches_the_pre_move_strings() {
        use chelabase_query::engine::{GUARDED_PREAMBLE_SQL, PREAMBLE_SQL, begin_sql};
        assert_eq!(RESET_SQL, PRE_MOVE_RESET);
        let info = crate::pools::test_support::two_databases();
        let settings = run_settings(&info, &info.databases[1]);
        assert_eq!(settings.reset_sql, PRE_MOVE_RESET);
        assert_eq!(settings.statement_timeout_ms, 8000);
        assert_eq!(settings.max_rows, 1000);
        assert_eq!(settings.default_schema, "reporting");
        assert_eq!(
            begin_sql(settings.reset_sql, TransactionStatus::Idle),
            "BEGIN; SET ROLE NONE; RESET ALL"
        );
        assert_eq!(
            begin_sql(settings.reset_sql, TransactionStatus::InTransaction),
            "ROLLBACK; BEGIN; SET ROLE NONE; RESET ALL"
        );
        assert_eq!(
            crate::owner_level::owner_check_batch(TransactionStatus::Idle),
            "SET ROLE NONE; RESET ALL"
        );
        assert_eq!(PREAMBLE_SQL, PRE_MOVE_PREAMBLE);
        assert_eq!(GUARDED_PREAMBLE_SQL, PRE_MOVE_GUARDED_PREAMBLE);
        let shared = PRE_MOVE_PREAMBLE
            .rfind("pg_catalog.set_config('transaction_timeout', $3, true), ")
            .unwrap();
        assert_eq!(PREAMBLE_SQL[..shared], GUARDED_PREAMBLE_SQL[..shared]);
    }

    /// A role session runs as the role it was opened with, never as the owner, and its
    /// transaction sends the pre-move opening and closing on the wire.
    #[tokio::test]
    async fn a_role_sessions_wire_matches_the_pre_move_sequence() {
        let fake = recorder(|_, _| false).await;
        let pools = Pools::new(At(fake.port), 10);
        let claims = serde_json::json!({ "sub": "u1" });
        let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap();
        let out = conn
            .with_role_session("reader", &claims, Some(16390), async |s| {
                match s.run_as() {
                    chelabase_query::RunAs::Role { role, claims, oid } => {
                        assert_eq!((role, oid), ("reader", Some(16390)));
                        assert_eq!(claims, &serde_json::json!({ "sub": "u1" }));
                    }
                    chelabase_query::RunAs::Owner { .. } => panic!("a role session as the owner"),
                }
                let settings = s.settings(&s.info().databases[0]);
                chelabase_query::with_transaction(s.lease(), settings.reset_sql, "rid", async |c| {
                    c.batch_execute("SELECT 1")
                        .await
                        .map_err(|e| chelabase_query::QueryFailure::from_pg(&e, "rid"))
                })
                .await
            })
            .await;
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(
            *fake.queries.lock().unwrap(),
            vec![
                "BEGIN; SET ROLE NONE; RESET ALL",
                "SELECT 1",
                "COMMIT",
                "SET ROLE NONE; RESET ALL",
            ]
        );
        // The connection went back to its pool.
        assert_eq!(settle(&fake.open, 1).await, 1);
    }

    /// A reset that fails, before the request or after it, discards the connection: it never
    /// goes back to the pool, and the next session opens a new one.
    #[tokio::test]
    async fn a_failed_reset_discards_the_connection() {
        for (name, fails) in [
            (
                "the opening reset",
                (|sql: &str, _| sql.starts_with("BEGIN")) as fn(&str, usize) -> bool,
            ),
            ("the trailing reset", |sql: &str, nth| {
                sql == RESET_SQL && nth == 2
            }),
        ] {
            let fake = recorder(fails).await;
            let pools = Pools::new(At(fake.port), 10);
            let claims = serde_json::json!({});
            let branch = Uuid::new_v4();
            let conn = pools.acquire(branch, "main").await.unwrap();
            let _ = conn
                .with_role_session("reader", &claims, None, async |s| {
                    chelabase_query::with_transaction(s.lease(), RESET_SQL, "rid", async |_| Ok(()))
                        .await
                })
                .await;
            assert_eq!(
                settle(&fake.open, 0).await,
                0,
                "{name}: the socket is closed"
            );
            // The next request opens a new connection.
            let conn = pools.acquire(branch, "main").await.unwrap();
            conn.with_role_session("reader", &claims, None, async |_| ())
                .await;
            assert_eq!(settle(&fake.open, 1).await, 1, "{name}");
        }
    }

    /// The owner check's clean-up failing on a role session's connection discards it too.
    #[tokio::test]
    async fn a_failed_owner_check_cleanup_discards_the_connection() {
        let fake = recorder(|sql, _| sql == RESET_SQL).await;
        let pools = Pools::new(At(fake.port), 10);
        let claims = serde_json::json!({});
        let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap();
        conn.with_role_session("reader", &claims, None, async |s| {
            let check = crate::owner_level::fetch_owner_level(s.lease(), "reader", "rid").await;
            assert!(check.is_err());
            assert!(s.lease().is_broken());
        })
        .await;
        assert_eq!(settle(&fake.open, 0).await, 0);
    }
}
