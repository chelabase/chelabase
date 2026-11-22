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
//! ([`RoleSession::settings`] carries the reset and the timeouts). Before the `COMMIT`,
//! deferred constraints are made immediate and the session check runs
//! (`SESSION_CHECK_SQL`): a transaction that no longer runs as the session's role, or a
//! session holding a SQL-level prepared statement, commits nothing (a 500). A reset or a
//! session check that fails marks the lease broken, and a broken or unfinished lease closes
//! its connection instead of pooling it.
//!
//! No role, no run: a connection's client can't be read, a [`Conn`] is no
//! [`chelabase_query::QueryClient`], and a [`RoleSession`] runs statements only through its
//! own methods, which apply the preamble (apart from its fixed pre-transaction read).
//! A role session refuses `none` and the empty name (Postgres reads `none` as no role: the
//! login itself). The one session as the login is explicit: [`Conn::with_login_session`].
//! `tests/compile_fail.rs` holds what doesn't compile, with each expected error.
//!
//! Which login a role's session runs on is decided before the connection is taken
//! ([`crate::Pools::acquire_for_role`]; the invariant is in [`chelabase_query::roles`]), and
//! the connection is bound to that role: [`Conn::with_role_session`] refuses any other
//! ([`NoRole`]), so a role never opens on a connection that wasn't routed for it.
//!
//! Statements sent outside a transaction ([`RoleSession::table_json`], the role facts read)
//! are followed by the session's full reset before the connection is pooled, and it is
//! pooled only when that reset passed.
//!
//! The one way in names the role and its claims:
//!
//! ```no_run
//! # async fn f(pools: &chelabase_pgsession::Pools<chelabase_pgsession::InternalSource>) {
//! # let cache = chelabase_pgsession::owner_level::owner_level_cache();
//! # let guard = chelabase_pgsession::owner_level::Guard::None;
//! let conn = pools
//!     .acquire_for_role(&cache, uuid::Uuid::nil(), "main", "authenticated", guard, "rid")
//!     .await
//!     .unwrap();
//! let claims = serde_json::json!({ "sub": "u1" });
//! let settings = chelabase_query::RunSettings {
//!     max_rows: 10,
//!     statement_timeout_ms: 1000,
//!     default_schema: "public".into(),
//!     reset_sql: chelabase_pgsession::RESET_SQL,
//! };
//! let _ = conn
//!     .with_role_session("authenticated", &claims, None, async |session| {
//!         session
//!             .transaction("main", &settings, "rid", async |client, _version| {
//!                 // After the preamble: as `authenticated`, with the claims.
//!                 let _ = client;
//!                 Ok(())
//!             })
//!             .await
//!     })
//!     .await;
//! # }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chelabase_query::roles::{Login, RoleFacts};
use chelabase_query::engine::{
    QueryCache, console_session, console_session_mapped, with_checked_transaction,
};
use chelabase_query::{Lease, Op, QueryClient, QueryFailure, RunAs, RunSettings, Schema};
use serde_json::Value;
use tokio_postgres::{Client, SimpleQueryMessage};
use uuid::Uuid;

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

/// A connection lent to one call as `role` with `claims` (see the module docs). Its
/// statements run only through its own methods, each of which applies the role, claims and
/// timeouts first ([`Self::run`], [`Self::transaction`], [`Self::schema`]), apart from the
/// fixed read a request makes before its transaction ([`Self::table_json`]). It holds its own copy of the role and claims (a borrowed one makes the session's future fail the
/// `Send` check axum's handlers need).
pub struct RoleSession {
    lease: Lease,
    info: Arc<ConnInfo>,
    role: String,
    claims: Value,
    oid: Option<i64>,
    /// A statement was sent outside a transaction and no transaction's trailing reset has
    /// passed since: the session owes the full reset before its connection is pooled.
    outside: AtomicBool,
    /// A transaction's preamble found the role unusable on this connection's login.
    unusable: AtomicBool,
}

impl RoleSession {
    /// Records what a transaction of the session ended with: one that answered ran its
    /// trailing reset (a failed reset marks the lease broken), so nothing is owed any more;
    /// a preamble that refused the role is remembered for the session's end.
    pub(crate) fn seen<T>(&self, out: Result<T, QueryFailure>) -> Result<T, QueryFailure> {
        match &out {
            Ok(_) => self.outside.store(false, Ordering::Relaxed),
            Err(f) if *f == chelabase_query::engine::role_unusable(&self.role) => {
                self.unusable.store(true, Ordering::Relaxed);
            }
            Err(_) => {}
        }
        out
    }

    /// The table `schema.table`'s model as the catalog's JSON (empty when there's no such
    /// table), read as the login before the session's transaction (the catalog is readable
    /// by every role), under `statement_timeout_ms`: the console cell read's first step.
    pub async fn table_json(
        &self,
        schema: &str,
        table: &str,
        statement_timeout_ms: u64,
    ) -> Result<String, tokio_postgres::Error> {
        let client = self.lease.client();
        self.outside.store(true, Ordering::Relaxed);
        client
            .simple_query(&format!("SET statement_timeout = {statement_timeout_ms}"))
            .await?;
        Ok(client
            .simple_query(&chelabase_schema::catalog::table_sql(schema, table))
            .await?
            .iter()
            .find_map(|m| match m {
                SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
                _ => None,
            })
            .unwrap_or_default())
    }

    /// [`chelabase_query::run`] as the session's role: `op` in `key = (branch, database)`,
    /// pipelined or sequential, in its own transaction.
    pub async fn run(
        &self,
        cache: &QueryCache,
        key: (Uuid, &str),
        settings: &RunSettings,
        budget: usize,
        op: &Op,
        request_id: &str,
    ) -> Result<String, QueryFailure> {
        let out = chelabase_query::run(
            &self.lease,
            cache,
            key,
            self.run_as(),
            settings,
            budget,
            op,
            request_id,
        )
        .await;
        self.seen(out)
    }

    /// A transaction as the session's role ([`console_session`]): `f` runs after the
    /// preamble, with the connection and the schema version; a failed preamble (a missing
    /// role) never runs it.
    pub async fn transaction<T>(
        &self,
        database: &str,
        settings: &RunSettings,
        request_id: &str,
        f: impl AsyncFnOnce(&Client, &str) -> Result<T, QueryFailure>,
    ) -> Result<T, QueryFailure> {
        let out = console_session(
            &self.lease,
            database,
            self.run_as(),
            settings,
            request_id,
            f,
        )
        .await;
        self.seen(out)
    }

    /// [`Self::transaction`], with a failed `COMMIT` mapped by `on_commit`
    /// ([`chelabase_query::with_transaction_mapped`]).
    pub async fn transaction_mapped<T>(
        &self,
        database: &str,
        settings: &RunSettings,
        request_id: &str,
        on_commit: impl FnOnce(&tokio_postgres::Error) -> QueryFailure,
        f: impl AsyncFnOnce(&Client, &str) -> Result<T, QueryFailure>,
    ) -> Result<T, QueryFailure> {
        let out = console_session_mapped(
            &self.lease,
            database,
            self.run_as(),
            settings,
            request_id,
            on_commit,
            f,
        )
        .await;
        self.seen(out)
    }

    /// In its own transaction as the session's role: the schema of `key = (branch,
    /// database)` ([`chelabase_query::load_schema`], which runs the preamble), then `f` with
    /// the connection and that schema. Like every transaction of the session it ends with
    /// the session check before its `COMMIT`
    /// ([`chelabase_query::engine::with_checked_transaction`]).
    pub async fn schema<T>(
        &self,
        cache: &QueryCache,
        key: (Uuid, &str),
        statement_timeout_ms: i64,
        request_id: &str,
        f: impl AsyncFnOnce(&Client, &Arc<Schema>) -> Result<T, QueryFailure>,
    ) -> Result<(Arc<Schema>, T), QueryFailure> {
        let on_commit = |e: &tokio_postgres::Error| QueryFailure::from_pg(e, request_id);
        let role = self.role.as_str();
        let out = with_checked_transaction(&self.lease, RESET_SQL, role, request_id, on_commit, async |client| {
            let (schema, _, _) = chelabase_query::load_schema(
                client,
                cache,
                key,
                self.run_as(),
                statement_timeout_ms,
                request_id,
            )
            .await?;
            let out = f(client, &schema).await?;
            Ok((schema, out))
        })
        .await;
        self.seen(out)
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

/// A role session asked for no role: `none` (any case) or the empty name, or for a role
/// other than the one its connection was routed for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} names no role")]
pub struct NoRole(pub String);

/// Whether `role` names no role in a preamble: Postgres reads `none` as the login itself, and
/// the empty name fails (any case of `none` is refused too).
fn names_no_role(role: &str) -> bool {
    role.is_empty() || role.eq_ignore_ascii_case("none")
}

impl Conn {
    /// Runs `f` with this connection as a session of `role` with `claims` (`oid`: the role's
    /// identity a secret key is bound to, `None` to check the name only). When `f` returns
    /// (an error it answers with included), the connection goes back to its pool unless a
    /// reset failed; when `f` is cancelled or panics, the connection is closed. Its slots are
    /// released after. A `role` that names no role, or isn't the role the connection was
    /// acquired for ([`crate::Pools::acquire_for_role`]), runs nothing ([`NoRole`]).
    pub async fn with_role_session<T>(
        self,
        role: &str,
        claims: &Value,
        oid: Option<i64>,
        f: impl AsyncFnOnce(&RoleSession) -> T,
    ) -> Result<T, NoRole> {
        if names_no_role(role) || self.role.as_deref() != Some(role) {
            return Err(NoRole(role.to_owned()));
        }
        Ok(self.session(role, claims, oid, f).await)
    }

    /// Runs `f` with this connection as a session of its own login (rest's `authenticator`),
    /// with no claims: each transaction's preamble sets the role `none`. Only for reads
    /// every role may make and the login is meant to make itself (rest's internal schema
    /// route, which describes other roles' views); everything else is a role session.
    ///
    /// Only `authenticator`'s connection has one: on any other login `f` doesn't run
    /// (`None`). The elevated login runs nothing but the role it was routed for.
    pub async fn with_login_session<T>(self, f: impl AsyncFnOnce(&RoleSession) -> T) -> Option<T> {
        if self.login != Login::Authenticator {
            return None;
        }
        Some(
            self.session("none", &Value::Object(Default::default()), None, f)
                .await,
        )
    }

    /// `role`'s facts ([`crate::owner_level::fetch_role_facts`]), read as the login on this
    /// `authenticator` connection, which then goes back to its pool.
    pub(crate) async fn read_role_facts(
        self,
        role: &str,
        request_id: &str,
    ) -> Result<Option<RoleFacts>, tokio_postgres::Error> {
        debug_assert_eq!(self.login, Login::Authenticator);
        self.session("none", &Value::Object(Default::default()), None, async |s| {
            s.outside.store(true, Ordering::Relaxed);
            crate::owner_level::fetch_role_facts(&s.lease, role, request_id).await
        })
        .await
    }

    /// `role`'s facts read now on this connection, before its role session: what a session
    /// on the elevated login is held to ([`crate::Pools::acquire_for_role`]). The connection
    /// then owes its session's full reset, and is closed at its end when the read's
    /// clean-up or the read itself failed.
    pub(crate) async fn confirm_role_facts(
        &mut self,
        role: &str,
        request_id: &str,
    ) -> Result<Option<RoleFacts>, tokio_postgres::Error> {
        /// The connection before its session's [`Lease`] exists.
        struct Unleased<'a> {
            client: &'a Client,
            broken: AtomicBool,
        }
        impl QueryClient for Unleased<'_> {
            fn client(&self) -> &Client {
                self.client
            }
            async fn prepare_cached(
                &self,
                sql: &str,
                types: &[tokio_postgres::types::Type],
            ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
                self.client.prepare_typed(sql, types).await
            }
            fn mark_broken(&self) {
                self.broken.store(true, Ordering::Relaxed);
            }
        }
        self.outside = true;
        let probe = Unleased {
            client: &self.client,
            broken: AtomicBool::new(false),
        };
        let out = crate::owner_level::fetch_role_facts(&probe, role, request_id).await;
        self.broken |= probe.broken.load(Ordering::Relaxed) || out.is_err();
        out
    }

    /// Ends a connection no session will run on: after the full reset it owes it goes back
    /// to its pool, or is closed when that failed.
    pub(crate) async fn release(self) {
        self.session("none", &Value::Object(Default::default()), None, async |_| ())
            .await
    }

    async fn session<T>(
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
            login: _,
            role: _,
            facts,
            outside,
            broken,
        } = self;
        let lease = Lease::new(client);
        if broken {
            lease.mark_broken();
        }
        // Declared after `permits`: dropped (pooled or closed) before they're released.
        let mut session = RoleSession {
            lease,
            info,
            role: role.to_owned(),
            claims: claims.clone(),
            oid,
            outside: AtomicBool::new(outside),
            unusable: AtomicBool::new(false),
        };
        let out = f(&session).await;
        // A statement went out outside a transaction: the full reset is owed, and the
        // connection is pooled only when it passed ([`Lease`]'s rule).
        let clean = !session.outside.load(Ordering::Relaxed)
            || match session.lease.client().batch_execute(RESET_SQL).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("the session's reset failed; the connection is discarded: {e}");
                    false
                }
            };
        if clean {
            session.lease.done();
        }
        // The login chosen from cached facts doesn't hold the role (any more): the facts
        // are dropped, and the next request reads them anew.
        if session.unusable.load(Ordering::Relaxed)
            && let Some((cache, key)) = &facts
        {
            cache.invalidate(key).await;
        }
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

    /// The session's reset, byte for byte: rest's from before the move, and what it has
    /// cleared and refused since (advisory locks, temporary tables, cursors, `LISTEN`s,
    /// sequence state, SQL-level prepared statements). The preambles below are the pre-move
    /// text unchanged.
    const PRE_MOVE_RESET: &str = "SET ROLE NONE; RESET ALL; \
         SELECT pg_catalog.pg_advisory_unlock_all(); DISCARD TEMP; CLOSE ALL; UNLISTEN *; \
         DISCARD SEQUENCES; \
         SELECT pg_catalog.set_config('role', CASE WHEN EXISTS \
         (SELECT 1 FROM pg_catalog.pg_prepared_statement() p WHERE p.from_sql) THEN '' ELSE 'none' END, false)";
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
        /// The user each connection logged in as, in order.
        pub(crate) users: Arc<Mutex<Vec<String>>>,
        /// Sockets open now.
        pub(crate) open: Arc<AtomicUsize>,
        /// What a role facts read is answered with (owner-level, elevated, held by the
        /// elevated login). `None`, the default: the extended protocol is refused.
        pub(crate) facts: Arc<Mutex<Option<[bool; 3]>>>,
    }

    /// The extended protocol's answer to one typed query: a row of three booleans.
    fn facts_row(facts: [bool; 3]) -> Vec<u8> {
        let mut out = message(b'1', &[]);
        out.extend(message(b'2', &[]));
        let mut desc = 3_i16.to_be_bytes().to_vec();
        let mut row = 3_i16.to_be_bytes().to_vec();
        for f in facts {
            desc.extend_from_slice(b"b\0");
            desc.extend_from_slice(&0_i32.to_be_bytes());
            desc.extend_from_slice(&0_i16.to_be_bytes());
            desc.extend_from_slice(&16_i32.to_be_bytes());
            desc.extend_from_slice(&1_i16.to_be_bytes());
            desc.extend_from_slice(&(-1_i32).to_be_bytes());
            desc.extend_from_slice(&1_i16.to_be_bytes());
            row.extend_from_slice(&1_i32.to_be_bytes());
            row.push(u8::from(f));
        }
        out.extend(message(b'T', &desc));
        out.extend(message(b'D', &row));
        out.extend(message(b'C', b"SELECT 1\0"));
        out
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
        let users = Arc::new(Mutex::new(Vec::new()));
        let open = Arc::new(AtomicUsize::new(0));
        let resets = Arc::new(AtomicUsize::new(0));
        let facts: Arc<Mutex<Option<[bool; 3]>>> = Arc::default();
        let (q, o, u, fa) = (queries.clone(), open.clone(), users.clone(), facts.clone());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (queries, open, resets, users) = (q.clone(), o.clone(), resets.clone(), u.clone());
                let facts = fa.clone();
                tokio::spawn(async move {
                    open.fetch_add(1, Ordering::SeqCst);
                    let Ok(len) = socket.read_i32().await else {
                        open.fetch_sub(1, Ordering::SeqCst);
                        return;
                    };
                    let mut startup = vec![0; usize::try_from(len).unwrap() - 4];
                    let _ = socket.read_exact(&mut startup).await;
                    let fields: Vec<&[u8]> = startup.get(4..).unwrap_or_default().split(|b| *b == 0).collect();
                    if let Some(i) = fields.iter().position(|f| *f == b"user")
                        && let Some(user) = fields.get(i + 1)
                    {
                        users.lock().unwrap().push(String::from_utf8_lossy(user).into_owned());
                    }
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
                        // The extended protocol (a prepare) is refused at its `Sync`, unless
                        // the stand-in was given role facts to answer with.
                        if kind == b'S' {
                            let answer = *facts.lock().unwrap();
                            let mut out = match answer {
                                Some(f) => facts_row(f),
                                None => message(b'E', &error_body()),
                            };
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
            users,
            open,
            facts,
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
    pub(crate) async fn settle(open: &AtomicUsize, want: usize) -> usize {
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
            let _ = &s.lease;
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
        assert!(PRE_MOVE_RESET.starts_with("SET ROLE NONE; RESET ALL; "));
        assert_eq!(
            begin_sql(settings.reset_sql, TransactionStatus::Idle),
            format!("BEGIN; {PRE_MOVE_RESET}")
        );
        assert_eq!(
            begin_sql(settings.reset_sql, TransactionStatus::InTransaction),
            format!("ROLLBACK; BEGIN; {PRE_MOVE_RESET}")
        );
        assert_eq!(
            crate::owner_level::owner_check_batch(TransactionStatus::Idle),
            PRE_MOVE_RESET
        );
        assert_eq!(PREAMBLE_SQL, PRE_MOVE_PREAMBLE);
        assert_eq!(GUARDED_PREAMBLE_SQL, PRE_MOVE_GUARDED_PREAMBLE);
        let shared = PRE_MOVE_PREAMBLE
            .rfind("pg_catalog.set_config('transaction_timeout', $3, true), ")
            .unwrap();
        assert_eq!(PREAMBLE_SQL[..shared], GUARDED_PREAMBLE_SQL[..shared]);
    }

    /// A role session's `run_as` is its own role, never the owner, and a transaction on its
    /// connection opens and closes with the pre-move text on the wire. (This fake answers no
    /// preamble, so the transaction here is `with_transaction`'s framing alone, from inside
    /// the crate; the live `a_session_for_a_missing_role_runs_nothing_live` shows the role.)
    #[tokio::test]
    async fn a_role_sessions_wire_matches_the_pre_move_sequence() {
        let fake = recorder(|_, _| false).await;
        let pools = Pools::new(At(fake.port), 10);
        let claims = serde_json::json!({ "sub": "u1" });
        let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap().bound_to("reader", None);
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
                chelabase_query::with_transaction(&s.lease, settings.reset_sql, "rid", async |c| {
                    c.batch_execute("SELECT 1")
                        .await
                        .map_err(|e| chelabase_query::QueryFailure::from_pg(&e, "rid"))
                })
                .await
            })
            .await
            .unwrap();
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(
            *fake.queries.lock().unwrap(),
            vec![
                // A pooled connection that is idle and unbroken opens without the
                // prepared-statement check; the reset after the COMMIT is the full text.
                format!(
                    "BEGIN; {}",
                    chelabase_query::engine::opening_reset_sql(PRE_MOVE_RESET)
                ),
                "SELECT 1".to_owned(),
                "COMMIT".to_owned(),
                PRE_MOVE_RESET.to_owned(),
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
            let conn = pools.acquire(branch, "main").await.unwrap().bound_to("reader", None);
            let _ = conn
                .with_role_session("reader", &claims, None, async |s| {
                    chelabase_query::with_transaction(&s.lease, RESET_SQL, "rid", async |_| Ok(()))
                        .await
                })
                .await;
            assert_eq!(
                settle(&fake.open, 0).await,
                0,
                "{name}: the socket is closed"
            );
            // The next request opens a new connection.
            let conn = pools.acquire(branch, "main").await.unwrap().bound_to("reader", None);
            conn.with_role_session("reader", &claims, None, async |_| ())
                .await
                .unwrap();
            assert_eq!(settle(&fake.open, 1).await, 1, "{name}");
        }
    }

    /// `none` (Postgres's word for no role, the login itself) and the empty name are no
    /// role: such a session runs nothing, and its connection goes back unused.
    #[tokio::test]
    async fn no_role_no_run() {
        let fake = recorder(|_, _| false).await;
        let pools = Pools::new(At(fake.port), 10);
        let claims = serde_json::json!({});
        for role in ["none", "NONE", "None", ""] {
            let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap();
            let ran = std::cell::Cell::new(false);
            let out = conn
                .with_role_session(role, &claims, None, async |_| ran.set(true))
                .await;
            assert_eq!(out.unwrap_err(), NoRole(role.to_owned()));
            assert!(!ran.get(), "{role:?} ran");
        }
        assert!(fake.queries.lock().unwrap().is_empty());
        // The login's own session is explicit, and its role is `none`.
        let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap();
        let role = conn
            .with_login_session(async |s| match s.run_as() {
                chelabase_query::RunAs::Role { role, .. } => role.to_owned(),
                chelabase_query::RunAs::Owner { .. } => panic!("the owner"),
            })
            .await
            .expect("authenticator's connection has a login session");
        assert_eq!(role, "none");
        // A connection opens a session only as the role it was routed for.
        let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap();
        let unbound = conn.with_role_session("reader", &claims, None, async |_| ()).await;
        assert_eq!(unbound.unwrap_err(), NoRole("reader".to_owned()));
        let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap().bound_to("reader", None);
        let other = conn.with_role_session("writer", &claims, None, async |_| ()).await;
        assert_eq!(other.unwrap_err(), NoRole("writer".to_owned()));
    }

    /// A statement sent outside a transaction ([`RoleSession::table_json`]) owes the
    /// session's full reset: it is the session's last statement, and the connection is
    /// pooled only when it passed.
    #[tokio::test]
    async fn a_statement_outside_a_transaction_owes_the_full_reset() {
        for (fails, pooled) in [
            ((|_, _| false) as fn(&str, usize) -> bool, 1),
            (|sql, _| sql == RESET_SQL, 0),
        ] {
            let fake = recorder(fails).await;
            let pools = Pools::new(At(fake.port), 10);
            let claims = serde_json::json!({});
            let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap().bound_to("reader", None);
            conn.with_role_session("reader", &claims, None, async |s| {
                s.table_json("public", "notes", 1000).await.unwrap();
            })
            .await
            .unwrap();
            let queries = fake.queries.lock().unwrap().clone();
            assert_eq!(queries.last().map(String::as_str), Some(RESET_SQL), "{queries:?}");
            assert_eq!(settle(&fake.open, pooled).await, pooled, "pooled only after a reset that passed");
        }
        // A session that sent nothing outside a transaction sends no reset of its own.
        let fake = recorder(|_, _| false).await;
        let pools = Pools::new(At(fake.port), 10);
        let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap().bound_to("reader", None);
        conn.with_role_session("reader", &serde_json::json!({}), None, async |_| ())
            .await
            .unwrap();
        assert!(fake.queries.lock().unwrap().is_empty());
        assert_eq!(settle(&fake.open, 1).await, 1);
    }

    /// The owner check's clean-up failing on a role session's connection discards it too.
    #[tokio::test]
    async fn a_failed_owner_check_cleanup_discards_the_connection() {
        let fake = recorder(|sql, _| sql == RESET_SQL).await;
        let pools = Pools::new(At(fake.port), 10);
        let claims = serde_json::json!({});
        let conn = pools.acquire(Uuid::new_v4(), "main").await.unwrap().bound_to("reader", None);
        conn.with_role_session("reader", &claims, None, async |s| {
            let check = crate::owner_level::fetch_role_facts(&s.lease, "reader", "rid").await;
            assert!(check.is_err());
            assert!(s.lease.is_broken());
        })
        .await
        .unwrap();
        assert_eq!(settle(&fake.open, 0).await, 0);
    }
}
