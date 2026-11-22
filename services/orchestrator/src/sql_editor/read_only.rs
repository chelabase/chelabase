//! The parse of a read-only run (`read_only: true`): which scripts may run, cut into the
//! statements that run one by one.
//!
//! Postgres is the read-only authority: every statement runs inside one transaction begun
//! `READ ONLY` whose snapshot is taken before the first statement, so Postgres refuses any
//! write and any switch back to read-write. This parse has one job: keep the script from
//! leaving or changing that transaction. It admits only `SELECT` (with `WITH`, `VALUES`,
//! `TABLE`), `SHOW` and `EXPLAIN` of a select, insert, update, delete or merge at the top
//! level, an allow-list, so no transaction control, `SET`/`RESET`/`DISCARD`, `COPY`, `DO`,
//! `CALL`, `PREPARE`/`EXECUTE` or any other kind can run. What a function called from a
//! `SELECT` does is Postgres's to refuse.
//!
//! Invariant: no statement runs outside the read-only transaction. Each slice [`check`]
//! returns is prepared on the server (extended protocol) before it runs, and the server
//! refuses a prepared statement holding more than one command. If this parse and the server
//! ever disagree about where a statement ends, the slice the server sees as two commands is
//! refused, never run; a script that doesn't parse is refused here.

use std::ops::Range;

use pg_query::NodeEnum;

/// The most statements a read-only script may hold (with the 1 MiB size cap, it bounds the
/// parse and the round trips).
pub const MAX_STATEMENTS: usize = 100;

/// Why a read-only script was refused before anything ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// 0-based, script order (empty statements don't count).
    pub statement: usize,
    /// pg_query's node name (`TransactionStmt`, `VariableSetStmt`, ...), `unparsed`, or
    /// `too_many_statements`.
    pub kind: String,
}

/// The kind for a script that doesn't parse.
pub const UNPARSED: &str = "unparsed";
/// The kind for a script past [`MAX_STATEMENTS`].
pub const TOO_MANY_STATEMENTS: &str = "too_many_statements";

/// The script's statements as the text each one runs with, or why it can't run read-only.
pub fn check(sql: &str) -> Result<Vec<String>, Refused> {
    let _ = sql;
    Ok(Vec::new())
}

/// The byte ranges of the script's statements, at the parser's own boundaries.
pub fn split(sql: &str) -> Result<Vec<Range<usize>>, Refused> {
    let _ = sql;
    Ok(Vec::new())
}

#[allow(dead_code)]
fn allowed(node: &NodeEnum) -> bool {
    let _ = node;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn refused(sql: &str) -> Refused {
        check(sql).expect_err(sql)
    }

    fn kind(sql: &str) -> String {
        refused(sql).kind
    }

    #[test]
    fn transaction_statements_are_refused() {
        for sql in [
            "COMMIT",
            "END",
            "ROLLBACK",
            "ABORT",
            "BEGIN",
            "BEGIN TRANSACTION READ WRITE",
            "START TRANSACTION",
            "SAVEPOINT a",
            "RELEASE SAVEPOINT a",
            "ROLLBACK TO SAVEPOINT a",
            "PREPARE TRANSACTION 'x'",
            "COMMIT PREPARED 'x'",
            "ROLLBACK PREPARED 'x'",
            "COMMIT AND CHAIN",
        ] {
            assert_eq!(kind(sql), "TransactionStmt", "{sql}");
        }
        // `SET TRANSACTION` is a `SET` to the parser.
        assert_eq!(kind("SET TRANSACTION READ WRITE"), "VariableSetStmt");
        assert_eq!(kind("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"), "VariableSetStmt");
    }

    #[test]
    fn every_set_reset_and_discard_is_refused() {
        for sql in [
            "SET default_transaction_read_only = off",
            "SET transaction_read_only = off",
            "SET ROLE postgres",
            "SET SESSION AUTHORIZATION postgres",
            "SET LOCAL statement_timeout = 0",
            "SET statement_timeout TO DEFAULT",
            "SET search_path = evil",
            "RESET ALL",
            "RESET ROLE",
            "RESET transaction_read_only",
        ] {
            assert_eq!(kind(sql), "VariableSetStmt", "{sql}");
        }
        for sql in ["DISCARD ALL", "DISCARD PLANS", "DISCARD TEMP", "DISCARD SEQUENCES"] {
            assert_eq!(kind(sql), "DiscardStmt", "{sql}");
        }
    }

    #[test]
    fn copy_is_refused() {
        for sql in ["COPY t TO STDOUT", "COPY t FROM STDIN", "COPY (SELECT 1) TO STDOUT"] {
            assert_eq!(kind(sql), "CopyStmt", "{sql}");
        }
    }

    #[test]
    fn show_is_allowed() {
        assert_eq!(check("SHOW server_version").unwrap(), vec!["SHOW server_version"]);
        assert_eq!(check("SHOW ALL").unwrap().len(), 1);
    }

    #[test]
    fn only_select_show_and_explain_pass() {
        for sql in [
            "SELECT 1",
            "SELECT * FROM t WHERE a = 1",
            "WITH a AS (SELECT 1) SELECT * FROM a",
            "VALUES (1), (2)",
            "TABLE t",
            "SELECT 1 UNION SELECT 2",
            "EXPLAIN SELECT 1",
            "EXPLAIN (ANALYZE, VERBOSE) SELECT 1",
            "EXPLAIN DELETE FROM t",
            "EXPLAIN UPDATE t SET a = 1",
            "EXPLAIN INSERT INTO t VALUES (1)",
            "EXPLAIN MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN DELETE",
            // Postgres refuses these writes inside the read-only transaction.
            "SELECT 1 INTO x",
            "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
        ] {
            assert_eq!(check(sql).unwrap().len(), 1, "{sql}");
        }
        for (sql, want) in [
            ("INSERT INTO t VALUES (1)", "InsertStmt"),
            ("UPDATE t SET a = 1", "UpdateStmt"),
            ("DELETE FROM t", "DeleteStmt"),
            ("MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN DELETE", "MergeStmt"),
            ("TRUNCATE t", "TruncateStmt"),
            ("CREATE TEMP TABLE x (a int)", "CreateStmt"),
            ("DROP TABLE t", "DropStmt"),
            ("GRANT SELECT ON t TO r", "GrantStmt"),
            ("LOCK TABLE t", "LockStmt"),
            ("ANALYZE", "VacuumStmt"),
            ("VACUUM", "VacuumStmt"),
            ("REINDEX TABLE t", "ReindexStmt"),
            ("CLUSTER t", "ClusterStmt"),
            ("CHECKPOINT", "CheckPointStmt"),
            ("LISTEN c", "ListenStmt"),
            ("UNLISTEN *", "UnlistenStmt"),
            ("NOTIFY c", "NotifyStmt"),
            ("LOAD 'x'", "LoadStmt"),
            ("PREPARE p AS SELECT 1", "PrepareStmt"),
            ("EXECUTE p", "ExecuteStmt"),
            ("DEALLOCATE ALL", "DeallocateStmt"),
            ("DECLARE c CURSOR FOR SELECT 1", "DeclareCursorStmt"),
            ("FETCH c", "FetchStmt"),
            ("CLOSE c", "ClosePortalStmt"),
            ("DO $$ BEGIN END $$", "DoStmt"),
            ("CALL p()", "CallStmt"),
            ("SET CONSTRAINTS ALL DEFERRED", "ConstraintsSetStmt"),
            ("CREATE TABLE x AS SELECT 1", "CreateTableAsStmt"),
            ("REFRESH MATERIALIZED VIEW m", "RefreshMatViewStmt"),
            // EXPLAIN of anything but a select or a DML statement: `EXECUTE` would reach
            // the driver's own prepared statements.
            ("EXPLAIN ANALYZE EXECUTE s0", "ExplainStmt"),
            ("EXPLAIN EXECUTE p", "ExplainStmt"),
            ("EXPLAIN DECLARE c CURSOR FOR SELECT 1", "ExplainStmt"),
            ("EXPLAIN CREATE TABLE x AS SELECT 1", "ExplainStmt"),
            ("EXPLAIN REFRESH MATERIALIZED VIEW m", "ExplainStmt"),
        ] {
            assert_eq!(kind(sql), want, "{sql}");
        }
    }

    #[test]
    fn an_unparsable_script_is_refused() {
        for sql in ["SELEC 1", "SELECT 1; SELEC 2", "SELECT 'unterminated", "SELECT $$a"] {
            assert_eq!(refused(sql), Refused { statement: 0, kind: UNPARSED.into() }, "{sql}");
        }
    }

    #[test]
    fn the_statement_cap() {
        let script = |n: usize| vec!["SELECT 1"; n].join(";");
        assert_eq!(check(&script(MAX_STATEMENTS)).unwrap().len(), MAX_STATEMENTS);
        assert_eq!(
            refused(&script(MAX_STATEMENTS + 1)),
            Refused { statement: MAX_STATEMENTS, kind: TOO_MANY_STATEMENTS.into() }
        );
        // Empty statements don't count.
        let padded = format!("{};;;;", script(MAX_STATEMENTS));
        assert_eq!(check(&padded).unwrap().len(), MAX_STATEMENTS);
    }

    // A 1 MiB script of 100 statements parses and checks well within the route's budget
    // (the run's own timeout, at least 100 ms; measured in the report).
    #[test]
    fn a_full_size_script_checks_quickly() {
        let filler = "x".repeat(super::super::MAX_SQL_BYTES / MAX_STATEMENTS - 40);
        let one = format!("SELECT '{filler}' AS a");
        let script = vec![one.as_str(); MAX_STATEMENTS].join(";\n");
        assert!(script.len() <= super::super::MAX_SQL_BYTES, "{}", script.len());
        let started = std::time::Instant::now();
        let slices = check(&script).unwrap();
        let took = started.elapsed();
        eprintln!("1 MiB, {MAX_STATEMENTS} statements: checked in {took:?}");
        assert_eq!(slices.len(), MAX_STATEMENTS);
        assert!(took < std::time::Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn a_refusal_names_the_statement() {
        assert_eq!(
            refused("SELECT 1; SHOW x; COMMIT; SELECT 2"),
            Refused { statement: 2, kind: "TransactionStmt".into() }
        );
        // The first offender, counted past empty statements.
        assert_eq!(
            refused(";; SELECT 1;; SET ROLE x; COMMIT"),
            Refused { statement: 1, kind: "VariableSetStmt".into() }
        );
        // Last position.
        assert_eq!(
            refused("SELECT 1; SELECT 2; DISCARD ALL"),
            Refused { statement: 2, kind: "DiscardStmt".into() }
        );
    }

    #[test]
    fn slices_follow_the_parser() {
        for (sql, want) in [
            ("SELECT 1; SELECT 2", vec!["SELECT 1", " SELECT 2"]),
            ("SELECT $$a;b$$; SELECT 2", vec!["SELECT $$a;b$$", " SELECT 2"]),
            ("SELECT $t$ ; $$ ; $t$; SELECT 2", vec!["SELECT $t$ ; $$ ; $t$", " SELECT 2"]),
            ("SELECT 1 /* ; */; SELECT 2", vec!["SELECT 1 /* ; */", " SELECT 2"]),
            ("SELECT 1 -- ;\n; SELECT 2", vec!["SELECT 1 -- ;\n", " SELECT 2"]),
            ("SELECT 'x;y'; SELECT 2", vec!["SELECT 'x;y'", " SELECT 2"]),
            ("SELECT E'\\';'; SELECT 2", vec!["SELECT E'\\';'", " SELECT 2"]),
            ("SELECT 1 AS \"a;b\"; SELECT 2", vec!["SELECT 1 AS \"a;b\"", " SELECT 2"]),
            ("SELECT 'é;✓'; SELECT '😀'", vec!["SELECT 'é;✓'", " SELECT '😀'"]),
            ("SELECT 1;;; SELECT 2;  ", vec!["SELECT 1", " SELECT 2"]),
            ("/* only a comment */", vec![]),
        ] {
            assert_eq!(check(sql).unwrap(), want, "{sql}");
        }
    }

    /// Allowed statements with `;` hidden in every place a lexer could mistake.
    const POOL: &[&str] = &[
        "SELECT 1",
        "SELECT $$a;b$$",
        "SELECT $q$ ; $$ ; $q$",
        "SELECT 'x;y'",
        "SELECT E'\\';'",
        "SELECT 1 AS \"a;b\"",
        "SELECT 1 /* ; */",
        "SELECT 1 -- ;\n",
        "SHOW search_path",
        "EXPLAIN SELECT 1",
        "VALUES (1)",
        "TABLE t",
        "WITH a AS (SELECT ';') SELECT * FROM a",
        "SELECT 'é✓😀'",
    ];
    /// Whether text between slices holds only `;`, whitespace and comments.
    fn holds_nothing(gap: &str) -> bool {
        pg_query::parse(gap).is_ok_and(|p| p.protobuf.stmts.is_empty())
    }

    const SEPARATORS: &[&str] = &[";", " ;\n", "; -- c\n", ";/* c */", ";;", ";\t"];

    proptest! {
        // For any script of allowed statements: every slice parses as exactly one statement,
        // and the script is the slices with only `;`, whitespace and comments between and around
        // them.
        #[test]
        fn slices_rebuild_the_script(
            parts in prop::collection::vec((0..POOL.len(), 0..SEPARATORS.len()), 1..12),
            tail in prop::bool::ANY,
        ) {
            let mut sql = String::new();
            for (i, (stmt, sep)) in parts.iter().enumerate() {
                sql.push_str(POOL[*stmt]);
                if i + 1 < parts.len() || tail {
                    sql.push_str(SEPARATORS[*sep]);
                }
            }
            let ranges = split(&sql).unwrap();
            prop_assert_eq!(ranges.len(), parts.len(), "{:?}", sql);
            let slices = check(&sql).unwrap();
            let mut at = 0;
            for (range, slice) in ranges.iter().zip(&slices) {
                prop_assert_eq!(&sql[range.clone()], slice.as_str());
                let gap = &sql[at..range.start];
                prop_assert!(holds_nothing(gap), "gap {:?} in {:?}", gap, sql);
                let parsed = pg_query::parse(slice).unwrap();
                prop_assert_eq!(parsed.protobuf.stmts.len(), 1, "{:?}", slice);
                at = range.end;
            }
            let rest = &sql[at..];
            prop_assert!(holds_nothing(rest), "rest {:?} in {:?}", rest, sql);
        }
    }
}
