//! The managed pool: a service's own login (storage's `chelabase_storage`), for the few
//! statements the service runs as itself rather than as a caller (storage's write and delete
//! tickets). A [`ServicePool`] runs only [`NamedStatement`]s, each a string literal in the
//! service's code ([`named_statement!`](crate::named_statement)), never one built from a
//! request: a statement's text is a [`StatementText`] associated const, which run-time text
//! can't fill. The pool has no role session and no way to hand out its connections. Every
//! statement runs in its own transaction on a reset session (`[ROLLBACK;] BEGIN; RESET_SQL`
//! … `COMMIT; RESET_SQL`); a failed reset or a cancelled call closes the connection.
//!
//! The boundary is Postgres's. Each connection checks, when it opens and again on every
//! checkout, that it is logged in as the expected service role, and that the role is no
//! superuser, can't create roles or databases, can't replicate, has no members and is a
//! member of nothing (`pg_auth_members`, `pg_has_role`): so `SET ROLE` or
//! `set_config('role', …)` to any other role fails in the database whatever the text, and a
//! grant made after a connection was pooled is caught before its next statement. A
//! connection that fails the check is never used: the pool refuses to open. `BYPASSRLS` is
//! allowed: storage's login has it by design, to write the rows its managed statements
//! own; with no memberships it can't lend it to any other role. Connections start with
//! `search_path = pg_catalog, pg_temp`.
//!
//! The text check ([`changes_session`]) is a secondary one: it refuses, at compile time,
//! anything that looks like it could change the session or leave state `RESET ALL` doesn't
//! clear (advisory locks, listeners, prepared statements, temporary tables, locks).
//!
//! Its connections count against the same service's per-branch budget as its role
//! sessions ([`ServicePool::beside`]). A managed call made while holding a role session
//! needs a free connection of the same budget: with N role sessions held at once, each
//! making a managed call, the budget must be above N, or the managed calls wait for the
//! acquire timeout and fail with `Busy` (with a budget of 1, any nested call does). Size
//! storage's budget above its peak concurrent role sessions.
//!
//! `tests/compile_fail.rs` holds what doesn't compile, with each expected error: run-time
//! text as a statement, a refused text, a statement built around the check, a connection
//! taken from the pool.
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

use chelabase_query::{Lease, QueryClient, QueryFailure, with_transaction, with_transaction_mapped};
use tokio_postgres::Row;
use tokio_postgres::types::ToSql;
/// The parameter types a [`StatementText`] lists (re-exported for [`named_statement!`](crate::named_statement)).
pub use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::pools::{AcquireError, ConnSource, Pools, RESET_SQL};

/// A statement a [`ServicePool`] may run: a fixed text (`$1`… its parameters, of `types`)
/// and a name for the logs, made only from a [`StatementText`] ([`NamedStatement::of`],
/// [`named_statement!`](crate::named_statement)).
#[derive(Debug, Clone, Copy)]
pub struct NamedStatement {
    name: &'static str,
    sql: &'static str,
    types: &'static [Type],
}

/// A statement's text as associated consts: only constant expressions (literals) fill them.
pub trait StatementText {
    const NAME: &'static str;
    const SQL: &'static str;
    const TYPES: &'static [Type];
}

/// A [`NamedStatement`] from string literals: it declares a hidden type, implements
/// [`StatementText`] for it and calls [`NamedStatement::of`]. Anything but a literal doesn't
/// match, and a text the check refuses ([`changes_session`]) is a compile error.
#[macro_export]
macro_rules! named_statement {
    ($name:literal, $sql:literal, $types:expr $(,)?) => {{
        struct Text;
        impl $crate::service::StatementText for Text {
            const NAME: &'static str = $name;
            const SQL: &'static str = $sql;
            const TYPES: &'static [$crate::service::Type] = $types;
        }
        $crate::service::NamedStatement::of::<Text>()
    }};
}

impl NamedStatement {
    /// The statement `T` describes; a text the check refuses ([`changes_session`]) fails to
    /// compile.
    pub const fn of<T: StatementText>() -> Self {
        const {
            assert!(
                !changes_session(T::SQL),
                "a named statement never changes the session"
            )
        };
        NamedStatement {
            name: T::NAME,
            sql: T::SQL,
            types: T::TYPES,
        }
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

    /// The statement's text (callers' tests check what it matches on).
    pub fn sql(&self) -> &'static str {
        self.sql
    }
}

const fn lower(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

const fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Whether `word` (lower case) is at `i` of `s`, in any case: as a whole word, or (`prefix`)
/// at the start of one.
const fn word_at(s: &[u8], i: usize, word: &[u8], prefix: bool) -> bool {
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
    prefix || i + word.len() == s.len() || !is_ident(s[i + word.len()])
}

/// The secondary text check (the boundary is the database's: see the module docs). It
/// refuses, case-insensitively and anywhere in the text (string literals included): a
/// comment (`--`, `/*`), a `U&` escape, more than one statement (`;`), the words `DO`, `SET`,
/// `RESET`, `ROLE`, `set_config`, `session_authorization`, `authorization`, `LISTEN`,
/// `UNLISTEN`, `NOTIFY`, `pg_notify`, `PREPARE`, `DEALLOCATE`, `DISCARD`, `TEMP`,
/// `TEMPORARY`, `LOAD` and `LOCK`, and any word starting `pg_advisory`. So `UPDATE … SET`
/// is refused too: managed statements are `SELECT`s of functions, `INSERT`s and `DELETE`s.
pub const fn changes_session(sql: &str) -> bool {
    const WORDS: [&[u8]; 18] = [
        b"do",
        b"set",
        b"reset",
        b"role",
        b"set_config",
        b"session_authorization",
        b"authorization",
        b"listen",
        b"unlisten",
        b"notify",
        b"pg_notify",
        b"prepare",
        b"deallocate",
        b"discard",
        b"temp",
        b"temporary",
        b"load",
        b"lock",
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
        if word_at(s, i, b"pg_advisory", true) {
            return true;
        }
        let mut w = 0;
        while w < WORDS.len() {
            if word_at(s, i, WORDS[w], false) {
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

    /// Storage's owner mode (object-storage m9, console requests only, decided by the human):
    /// `f` in one transaction on a reset session of the service's own login, with the
    /// branch's statement timeout. `f` runs the service's own fixed statements (storage's
    /// object handlers), never a caller's text. The Postgres boundary above holds as for
    /// [`Self::query`]: the connection is checked to be the service's login with no
    /// memberships, so nothing it runs can become another role; it reaches what that login's
    /// grants reach. A failed reset or a cancelled call closes the connection. A failed
    /// `COMMIT` goes through `on_commit` ([`with_transaction_mapped`]).
    pub async fn transaction<T>(
        &self,
        branch: Uuid,
        database: &str,
        statement_timeout_ms: i64,
        request_id: &str,
        on_commit: impl FnOnce(&tokio_postgres::Error) -> QueryFailure,
        f: impl AsyncFnOnce(&tokio_postgres::Client) -> Result<T, QueryFailure>,
    ) -> Result<T, ServiceError> {
        let conn = self.pools.acquire(branch, database).await?;
        let permits = conn.permits;
        // Declared after `permits`: dropped (pooled or closed) before they're released.
        let mut lease = Lease::new(conn.client);
        let timeout = statement_timeout_ms.max(0).to_string();
        let out = with_transaction_mapped(&lease, RESET_SQL, request_id, on_commit, async |c| {
            c.query_typed(
                "SELECT pg_catalog.set_config('statement_timeout', $1, true), pg_catalog.set_config('transaction_timeout', $1, true)",
                &[(&timeout, Type::TEXT)],
            )
            .await
            .map_err(|e| QueryFailure::from_pg(&e, request_id))?;
            f(c).await
        })
        .await;
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

    /// Session state `RESET ALL` doesn't clear (advisory locks, listeners, prepared
    /// statements, temporary tables, loaded libraries, held locks) is refused too.
    #[test]
    fn session_state_outside_reset_all_is_refused() {
        for sql in [
            "SELECT pg_catalog.pg_advisory_lock(42)",
            "SELECT PG_ADVISORY_XACT_LOCK(1)",
            "LISTEN storage",
            "SELECT pg_notify('c', 'x')",
            "NOTIFY storage",
            "PREPARE p AS SELECT 1",
            "DEALLOCATE ALL",
            "DISCARD PLANS",
            "CREATE TEMP TABLE t (a int)",
            "CREATE TEMPORARY TABLE t (a int)",
            "LOAD 'auto_explain'",
            "LOCK TABLE storage.objects",
        ] {
            assert!(changes_session(sql), "{sql:?}");
        }
        assert!(!changes_session("SELECT storage.write_ticket($1)"));
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
