//! The managed pool: a service's own login (storage's `chelabase_storage`), for the few
//! statements the service runs as itself rather than as a caller (storage's write and delete
//! tickets). A [`ServicePool`] runs only [`NamedStatement`]s, each a fixed text declared in
//! the service's code, never one built from a request, and none that changes the session: it
//! has no role session, no `SET ROLE`, and no way to hand out its connections. Every
//! statement runs in its own transaction on a reset session (`[ROLLBACK;] BEGIN; RESET_SQL`
//! … `COMMIT; RESET_SQL`); a failed reset or a cancelled call closes the connection.
//!
//! Its connections count against the same service's per-branch budget as its role
//! sessions ([`ServicePool::beside`]).
//!
//! A statement's text is `'static`: a request's text can't become one.
//!
//! ```compile_fail,E0597
//! fn from_request(sql: String) {
//!     let _ = chelabase_pgsession::service::NamedStatement::new("x", &sql, &[]);
//! }
//! ```
//!
//! A statement that sets a role or changes the session doesn't compile:
//!
//! ```compile_fail,E0080
//! const ESCALATE: chelabase_pgsession::service::NamedStatement =
//!     chelabase_pgsession::service::NamedStatement::new("escalate", "SET ROLE postgres", &[]);
//! ```
//!
//! nor can one be built around [`NamedStatement::new`]'s check:
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
//! use chelabase_pgsession::service::NamedStatement;
//! use tokio_postgres::types::Type;
//!
//! const WRITE_TICKET: NamedStatement =
//!     NamedStatement::new("write ticket", "SELECT storage.write_ticket($1)", &[Type::TEXT]);
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
/// and a name for the logs. [`Self::new`] refuses one that changes the session (at compile
/// time in a `const`).
#[derive(Debug, Clone, Copy)]
pub struct NamedStatement {
    name: &'static str,
    sql: &'static str,
    types: &'static [Type],
}

impl NamedStatement {
    /// Panics (a compile error in a `const`) when `sql` changes the session
    /// ([`changes_session`]).
    pub const fn new(name: &'static str, sql: &'static str, types: &'static [Type]) -> Self {
        assert!(
            !changes_session(sql),
            "a named statement never changes the session"
        );
        NamedStatement { name, sql, types }
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

/// The first index from `i` that isn't whitespace.
const fn skip_space(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && s[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Whether `i` starts a statement: only whitespace before it, back to the text's start or a
/// `;`.
const fn starts_statement(s: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 && s[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    j == 0 || s[j - 1] == b';'
}

/// Whether `sql` may change the session (over-approximated: string literals and comments
/// count too): a `SET` or `RESET` statement, `SET ROLE`, `SET SESSION`, `SET LOCAL`,
/// `SESSION AUTHORIZATION` or `set_config` anywhere. `UPDATE … SET col = …` is none of them.
pub const fn changes_session(sql: &str) -> bool {
    let s = sql.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if word_at(s, i, b"set_config")
            || word_at(s, i, b"reset")
            || word_at(s, i, b"authorization")
        {
            return true;
        }
        if word_at(s, i, b"set") {
            let j = skip_space(s, i + 3);
            if starts_statement(s, i)
                || word_at(s, j, b"role")
                || word_at(s, j, b"session")
                || word_at(s, j, b"local")
            {
                return true;
            }
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

    /// What a session-changing statement looks like, in any case and spacing, and what a
    /// managed write that merely names `SET` or a word ending in `reset` looks like.
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
        ] {
            assert!(changes_session(sql), "{sql}");
        }
        for sql in [
            "UPDATE storage.objects SET name = $1 WHERE id = $2",
            "INSERT INTO storage.tickets (preset, roles) VALUES ($1, $2)",
            "SELECT storage.write_ticket($1, $2)",
            "DELETE FROM storage.objects WHERE offset_reset = $1",
        ] {
            assert!(!changes_session(sql), "{sql}");
        }
    }

    /// Built at run time (a leaked string), a session-changing statement still never exists.
    #[test]
    #[should_panic(expected = "changes the session")]
    fn a_session_changing_statement_panics_at_run_time_too() {
        let sql: &'static str = String::from("SET ROLE postgres").leak();
        let _ = NamedStatement::new("escalate", sql, &[]);
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
        let fake =
            crate::pools::test_support::fake_pg(crate::pools::test_support::Answer::Ready).await;
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
        assert!(managed.pools.acquire(branch, "main").await.is_ok());
    }

    /// A managed statement runs in its own transaction on a reset session, and nothing in it
    /// sets a role: on the wire, the opening, the end and the trailing reset only (the
    /// statement itself goes through the extended protocol, which this fake refuses).
    #[tokio::test]
    async fn a_managed_statements_session_sql_is_the_reset_only() {
        let fake = crate::session::tests::recorder(|_, _| false).await;
        let roles = crate::Pools::new(Five(fake.port), 100).for_service(Service::Storage, |_| 1);
        let managed = ServicePool::beside(&roles, Five(fake.port));
        const WRITE: NamedStatement = NamedStatement::new(
            "write ticket",
            "SELECT storage.write_ticket($1)",
            &[Type::TEXT],
        );
        let out = managed
            .query(Uuid::new_v4(), "main", &WRITE, &[&"t1"], "rid")
            .await;
        assert!(matches!(out, Err(ServiceError::Query(_))), "{out:?}");
        assert_eq!(
            *fake.queries.lock().unwrap(),
            vec![
                "BEGIN; SET ROLE NONE; RESET ALL",
                "ROLLBACK",
                "SET ROLE NONE; RESET ALL",
            ]
        );
    }

    /// A managed statement runs in its own transaction on a reset session (`SET ROLE NONE`
    /// first: the service's own login, never a caller's role), and a failure ends it.
    #[tokio::test]
    async fn a_managed_statement_runs_as_the_service_on_a_reset_session() {
        let fake =
            crate::pools::test_support::fake_pg(crate::pools::test_support::Answer::Ready).await;
        let roles = crate::Pools::new(Five(fake.port), 100).for_service(Service::Storage, |_| 1);
        let managed = ServicePool::beside(&roles, Five(fake.port));
        const WRITE: NamedStatement = NamedStatement::new(
            "write ticket",
            "SELECT storage.write_ticket($1)",
            &[Type::TEXT],
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
