//! The managed pool: a service's own login (storage's `chelabase_storage`), for the few
//! statements the service runs as itself rather than as a caller (storage's write and delete
//! tickets). A [`ServicePool`] runs only [`NamedStatement`]s, each a string literal in the
//! service's code ([`named_statement!`](crate::named_statement)), never one built from a
//! request. It has no role session and no way to hand out its connections. Every statement
//! runs in its own transaction on a reset session (`[ROLLBACK;] BEGIN; RESET_SQL` …
//! `COMMIT; RESET_SQL`); a failed reset or a cancelled call closes the connection.
//!
//! The boundary is Postgres's: each connection checks, before it's pooled, that it logged in
//! as the expected service role and that the role is no superuser, can't create roles, has
//! no members and is a member of nothing (`pg_auth_members`, `pg_has_role`), so `SET ROLE`
//! or `set_config('role', …)` to any other role fails in the database whatever the text. A
//! connection that fails the check is never pooled: the pool refuses to open. Connections
//! start with `search_path = pg_catalog, pg_temp`.
//!
//! The text check ([`changes_session`]) is a secondary one: it refuses, at compile time,
//! anything that looks like it could change the session.
//!
//! Its connections count against the same service's per-branch budget as its role
//! sessions ([`ServicePool::beside`]). With a budget of 1, never call it while holding a role
//! session: it waits for the one connection and fails with `Busy` after the acquire timeout.
//!
//! A statement is a literal; a request's text can't become one:
//!
//! ```compile_fail
//! fn from_request(sql: String) {
//!     let _ = chelabase_pgsession::named_statement!("x", &sql, &[]);
//! }
//! ```
//!
//! ```compile_fail
//! let request = String::from("DELETE FROM storage.objects");
//! let _ = chelabase_pgsession::named_statement!("from request", request.leak(), &[]);
//! ```
//!
//! ```compile_fail,E0599
//! let request = String::from("DELETE FROM storage.objects");
//! let _ = chelabase_pgsession::service::NamedStatement::new("from request", request.leak(), &[]);
//! ```
//!
//! A statement the text check refuses doesn't compile, the audit's bypasses included:
//!
//! ```compile_fail,E0080
//! let _ = chelabase_pgsession::named_statement!("escalate", "SET ROLE postgres", &[]);
//! ```
//!
//! ```compile_fail,E0080
//! let _ = chelabase_pgsession::named_statement!("escalate", "/**/SET/**/ROLE postgres", &[]);
//! ```
//!
//! ```compile_fail,E0080
//! let _ = chelabase_pgsession::named_statement!(
//!     "path",
//!     "-- managed write\nSET search_path TO evil",
//!     &[]
//! );
//! ```
//!
//! ```compile_fail,E0080
//! let _ = chelabase_pgsession::named_statement!("path", "/* x */ SET search_path TO evil", &[]);
//! ```
//!
//! ```compile_fail,E0080
//! let _ = chelabase_pgsession::named_statement!(
//!     "escalate",
//!     "SELECT U&\"set\\005fconfig\"('role', 'postgres', false)",
//!     &[]
//! );
//! ```
//!
//! ```compile_fail,E0080
//! let _ = chelabase_pgsession::named_statement!(
//!     "escalate",
//!     "DO $$BEGIN EXECUTE format('%s ROLE postgres', 'SE' || 'T'); END$$",
//!     &[]
//! );
//! ```
//!
//! nor can one be built around the check:
//!
//! ```compile_fail,E0451
//! let _ = chelabase_pgsession::service::NamedStatement {
//!     name: "escalate",
//!     sql: "SET ROLE postgres",
//!     types: &[],
//! };
//! ```
//!
//! The pool hands out no connection, so nothing else runs on it (and no role session can be
//! opened from it):
//!
//! ```compile_fail,E0599
//! async fn f(pool: &chelabase_pgsession::ServicePool<chelabase_pgsession::InternalSource>) {
//!     let conn = pool.acquire(uuid::Uuid::nil(), "main").await.unwrap();
//! }
//! ```
//!
//! ```compile_fail,E0616
//! async fn f(pool: &chelabase_pgsession::ServicePool<chelabase_pgsession::InternalSource>) {
//!     let _ = pool.pools.acquire(uuid::Uuid::nil(), "main").await;
//! }
//! ```
//!
//! What it does run:
//!
//! ```no_run
//! use chelabase_pgsession::named_statement;
//! use chelabase_pgsession::service::NamedStatement;
//! use tokio_postgres::types::Type;
//!
//! const WRITE_TICKET: NamedStatement =
//!     named_statement!("write ticket", "SELECT storage.write_ticket($1)", &[Type::TEXT]);
//!
//! # async fn f(pool: &chelabase_pgsession::ServicePool<chelabase_pgsession::InternalSource>) {
//! let rows = pool
//!     .query(uuid::Uuid::nil(), "main", &WRITE_TICKET, &[&"t1"], "rid")
//!     .await;
//! # }
//! ```

use chelabase_query::{Lease, QueryClient, QueryFailure, with_transaction};
use tokio_postgres::Row;
use tokio_postgres::types::{ToSql, Type};
use uuid::Uuid;

use crate::pools::{AcquireError, ConnSource, Pools, RESET_SQL};

/// A statement a [`ServicePool`] may run: a fixed text (`$1`… its parameters, of `types`)
/// and a name for the logs, made only by [`named_statement!`](crate::named_statement).
#[derive(Debug, Clone, Copy)]
pub struct NamedStatement {
    name: &'static str,
    sql: &'static str,
    types: &'static [Type],
}

/// A [`NamedStatement`] from a string literal, evaluated in a `const` item: a text the
/// check refuses ([`changes_session`]) is a compile error, and anything but a literal doesn't
/// match.
#[macro_export]
macro_rules! named_statement {
    ($name:literal, $sql:literal, $types:expr $(,)?) => {{
        const STATEMENT: $crate::service::NamedStatement =
            $crate::service::NamedStatement::__literal_only_use_named_statement_macro(
                $name, $sql, $types,
            );
        STATEMENT
    }};
}

impl NamedStatement {
    /// [`named_statement!`](crate::named_statement)'s constructor; never call it directly.
    /// Panics when the text check refuses `sql`.
    #[doc(hidden)]
    pub const fn __literal_only_use_named_statement_macro(
        name: &'static str,
        sql: &'static str,
        types: &'static [Type],
    ) -> Self {
        assert!(
            !changes_session(sql),
            "a named statement never changes the session"
        );
        NamedStatement { name, sql, types }
    }

    /// A statement the text check never saw: this crate's live tests show the database
    /// refuses what the check would have.
    #[cfg(test)]
    pub(crate) fn unchecked_for_tests(name: &'static str, sql: &'static str) -> Self {
        NamedStatement {
            name,
            sql,
            types: &[],
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

const fn lower(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

const fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Whether `word` (lower case) is at `i` of `s`, as a whole word, in any case.
const fn word_at(s: &[u8], i: usize, word: &[u8]) -> bool {
    if i + word.len() > s.len() || (i > 0 && is_ident(s[i - 1])) {
        return false;
    }
    let mut k = 0;
    while k < word.len() {
        if lower(s[i + k]) != word[k] {
            return false;
        }
        k += 1;
    }
    i + word.len() == s.len() || !is_ident(s[i + word.len()])
}

/// The secondary text check (the boundary is the database's: see the module docs). It
/// refuses, case-insensitively and anywhere in the text (string literals included): a
/// comment (`--`, `/*`), a `U&` escape, more than one statement (`;`), and the words `DO`,
/// `SET`, `RESET`, `ROLE`, `set_config`, `session_authorization` and `authorization`. So
/// `UPDATE … SET` is refused too: managed statements are `SELECT`s of functions, `INSERT`s
/// and `DELETE`s.
pub const fn changes_session(sql: &str) -> bool {
    const WORDS: [&[u8]; 7] = [
        b"do",
        b"set",
        b"reset",
        b"role",
        b"set_config",
        b"session_authorization",
        b"authorization",
    ];
    let s = sql.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let next = if i + 1 < s.len() { lower(s[i + 1]) } else { 0 };
        match (s[i], next) {
            (b'-', b'-') | (b'/', b'*') | (b';', _) => return true,
            (b'u' | b'U', b'&') => return true,
            _ => {}
        }
        let mut w = 0;
        while w < WORDS.len() {
            if word_at(s, i, WORDS[w]) {
                return true;
            }
            w += 1;
        }
        i += 1;
    }
    false
}

/// Why a managed statement didn't run or failed.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Acquire(#[from] AcquireError),
    #[error("{}", .0.message)]
    Query(QueryFailure),
}

/// A service's own pools for [`NamedStatement`]s (see the module docs).
pub struct ServicePool<S> {
    pools: Pools<S>,
}

impl<S: ConnSource> ServicePool<S> {
    /// Pools connecting with `source`'s credentials (the service's own login), within the
    /// same budget as `role_pools`, the service's role-session pools: the branch slots and
    /// the global cap are shared, so both together stay within the service's budget.
    pub fn beside<R: ConnSource>(role_pools: &Pools<R>, source: S) -> Self {
        ServicePool {
            pools: role_pools.sharing_budget(source),
        }
    }

    /// Runs `statement` with `params` in the branch's `database`, in its own transaction
    /// (see the module docs); the rows it returns.
    pub async fn query(
        &self,
        branch: Uuid,
        database: &str,
        statement: &NamedStatement,
        params: &[&(dyn ToSql + Sync)],
        request_id: &str,
    ) -> Result<Vec<Row>, ServiceError> {
        let conn = self.pools.acquire(branch, database).await?;
        let permits = conn.permits;
        // Declared after `permits`: dropped (pooled or closed) before they're released.
        let mut lease = Lease::new(conn.client);
        let out = with_transaction(&lease, RESET_SQL, request_id, async |c| {
            let pg = |e: tokio_postgres::Error| QueryFailure::from_pg(&e, request_id);
            let prepared = lease
                .prepare_cached(statement.sql, statement.types)
                .await
                .map_err(pg)?;
            c.query(&prepared, params).await.map_err(pg)
        })
        .await;
        if let Err(f) = &out {
            tracing::debug!(request_id, statement = statement.name, "{}", f.message);
        }
        lease.done();
        drop(lease);
        drop(permits);
        out.map_err(ServiceError::Query)
    }

    /// Forgets what's held for `branch` ([`Pools::invalidate`]).
    pub async fn invalidate(&self, branch: Uuid) {
        self.pools.invalidate(branch).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pools::{ConnError, ConnInfo, Service};
    use std::time::Duration;
    use uuid::Uuid;

    /// What a session-changing statement looks like, in any case and spacing (and, now that
    /// the check refuses the word anywhere, `UPDATE … SET`), and what a managed write that
    /// names a word merely containing one of the refused words looks like.
    #[test]
    fn a_named_statement_never_changes_the_session() {
        for sql in [
            "SET ROLE postgres",
            "set  role postgres",
            "SELECT 1; SET\n\tSESSION AUTHORIZATION postgres",
            "SET LOCAL ROLE x",
            "SET SESSION ROLE x",
            "select pg_catalog.set_config('role', 'x', false)",
            "SELECT SET_CONFIG('role', 'x', true)",
            "RESET ROLE",
            "reset all",
            "SET search_path = public",
            "UPDATE storage.objects SET name = $1 WHERE id = $2",
            "SELECT 1 FROM pg_roles WHERE rolname = 'x' AND 'role' = 'role'",
        ] {
            assert!(changes_session(sql), "{sql}");
        }
        for sql in [
            "INSERT INTO storage.tickets (preset, roles) VALUES ($1, $2)",
            "SELECT storage.write_ticket($1, $2)",
            "DELETE FROM storage.objects WHERE offset_reset = $1",
        ] {
            assert!(!changes_session(sql), "{sql}");
        }
    }

    /// The audit's bypasses of the first text check: comments around or before `SET`, a
    /// `U&` identifier, dynamic SQL in `DO`. Each is refused (and so fails to build).
    #[test]
    fn the_audits_bypasses_are_refused() {
        for sql in [
            "/**/SET/**/ROLE postgres",
            "-- managed write\nSET search_path TO evil",
            "/* x */ SET search_path TO evil",
            "SELECT U&\"set\\005fconfig\"('role', 'postgres', false)",
            "DO $$BEGIN EXECUTE format('%s ROLE postgres', 'SE' || 'T'); END$$",
            "\u{b}SET ROLE postgres",
            "SELECT 1; SELECT 2",
        ] {
            assert!(changes_session(sql), "{sql:?}");
        }
    }

    /// Built at run time (a leaked string), a session-changing statement still never exists.
    #[test]
    #[should_panic(expected = "changes the session")]
    fn a_session_changing_statement_panics_at_run_time_too() {
        let sql: &'static str = String::from("SET ROLE postgres").leak();
        let _ = NamedStatement::__literal_only_use_named_statement_macro("escalate", sql, &[]);
    }

    struct Five(u16);

    impl crate::pools::ConnSource for Five {
        async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
            let mut info = crate::pools::test_support::two_databases();
            info.port = self.0;
            info.settings.max_connections = 5;
            Ok(info)
        }
    }

    /// The managed pool counts against the same service's budget as its role sessions: with
    /// storage's one connection held by a role session, the managed pool gets none.
    #[tokio::test]
    async fn the_managed_pool_shares_its_services_budget() {
        let fake = crate::session::tests::recorder(|_, _| false).await;
        let roles = crate::Pools::new(Five(fake.port), 100).for_service(Service::Storage, |_| 1);
        let managed = ServicePool::beside(&roles, Five(fake.port));
        let branch = Uuid::new_v4();
        let held = roles.acquire(branch, "main").await.unwrap();
        let waited = tokio::time::timeout(
            Duration::from_millis(300),
            managed.pools.acquire(branch, "main"),
        )
        .await;
        assert!(
            waited.is_err(),
            "the managed pool opened past storage's budget"
        );
        drop(held);
        // Freed, the slot is the managed pool's: it opens a connection at once (which this
        // fake can't pass the login check with).
        let opened = tokio::time::timeout(
            Duration::from_millis(1500),
            managed.pools.acquire(branch, "main"),
        )
        .await;
        assert!(
            matches!(opened, Ok(Err(AcquireError::Db(_))) | Ok(Ok(_))),
            "{:?}",
            opened.map(|r| r.map(|_| ()))
        );
    }

    /// Nested on a budget of 1 (a role session held, then the managed pool), the managed
    /// call doesn't hang: it fails with `Busy` once the acquire timeout passes.
    #[tokio::test]
    async fn a_nested_managed_call_on_a_budget_of_one_fails() {
        let fake =
            crate::pools::test_support::fake_pg(crate::pools::test_support::Answer::Ready).await;
        let roles = crate::Pools::new(Five(fake.port), 100).for_service(Service::Storage, |_| 1);
        let managed = ServicePool::beside(&roles, Five(fake.port));
        let branch = Uuid::new_v4();
        let _held = roles.acquire(branch, "main").await.unwrap();
        // Only timers are left to wait on: the paused clock jumps to the timeout.
        tokio::time::pause();
        const WRITE: NamedStatement =
            crate::named_statement!("write ticket", "SELECT storage.write_ticket($1)", &[]);
        let out = managed.query(branch, "main", &WRITE, &[], "rid").await;
        assert!(
            matches!(
                out,
                Err(ServiceError::Acquire(
                    AcquireError::Busy | AcquireError::TimedOut
                ))
            ),
            "{:?}",
            out.map(|rows| rows.len())
        );
    }

    /// A connection that can't show it's the bare service login is never used: this fake
    /// refuses the login check, so nothing runs (the live tests show a real check pass).
    #[tokio::test]
    async fn a_connection_failing_the_login_check_runs_nothing() {
        let fake = crate::session::tests::recorder(|_, _| false).await;
        let roles = crate::Pools::new(Five(fake.port), 100).for_service(Service::Storage, |_| 1);
        let managed = ServicePool::beside(&roles, Five(fake.port));
        const WRITE: NamedStatement = crate::named_statement!(
            "write ticket",
            "SELECT storage.write_ticket($1)",
            &[Type::TEXT]
        );
        let out = managed
            .query(Uuid::new_v4(), "main", &WRITE, &[&"t1"], "rid")
            .await;
        assert!(matches!(out, Err(ServiceError::Acquire(_))), "{out:?}");
        assert!(fake.queries.lock().unwrap().is_empty());
    }

    /// A managed statement runs in its own transaction on a reset session (`SET ROLE NONE`
    /// first: the service's own login, never a caller's role), and a failure ends it.
    #[tokio::test]
    async fn a_managed_statement_runs_as_the_service_on_a_reset_session() {
        let fake =
            crate::pools::test_support::fake_pg(crate::pools::test_support::Answer::Ready).await;
        let roles = crate::Pools::new(Five(fake.port), 100).for_service(Service::Storage, |_| 1);
        let managed = ServicePool::beside(&roles, Five(fake.port));
        const WRITE: NamedStatement = crate::named_statement!(
            "write ticket",
            "SELECT storage.write_ticket($1)",
            &[Type::TEXT]
        );
        assert_eq!(WRITE.name(), "write ticket");
        // The fake never answers a statement: the call times out, and its connection is
        // closed rather than pooled.
        let call = managed.query(Uuid::new_v4(), "main", &WRITE, &[&"t1"], "rid");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), call)
                .await
                .is_err()
        );
        for _ in 0..100 {
            if fake.open.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fake.open.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
