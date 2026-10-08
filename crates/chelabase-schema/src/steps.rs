//! User steps and own SQL, classified with pg_query (P8, T4).

use serde::{Deserialize, Serialize};

use crate::plan::{OnlineCheck, Phase, StepId};

/// A user's SQL inserted into the plan after `after` (`None`: at the start of its phase).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserStep {
    pub id: StepId,
    pub after: Option<StepId>,
    pub sql: String,
}

/// One statement of a user step and the phase it runs in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedStatement {
    pub sql: String,
    pub phase: Phase,
    /// pg_query's node name (`IndexStmt`, `AlterEnumStmt`, …).
    pub statement: String,
    /// What a phase 2 resume checks before running it again (a concurrent index: valid by
    /// name), as for the planner's own steps (T14 minor 16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online: Option<OnlineCheck>,
}

/// A statement that can't run in a merge (transaction control, role or database DDL, …), or SQL
/// that doesn't parse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("statement {index}: {reason}")]
pub struct StepError {
    /// The statement's position in the step (0-based).
    pub index: usize,
    pub sql: String,
    pub reason: String,
}

/// Splits `sql` into statements and assigns each a phase (P8); refused statements are errors.
///
/// - refused: transaction control, role and database DDL, `ALTER SYSTEM`, `VACUUM`, and
///   anything that changes the session's role (`SET ROLE`, `RESET ROLE`, `SET SESSION
///   AUTHORIZATION`, `RESET ALL`, `DISCARD`): the scripts run in the owner's own session
///   (E15, psql `-U postgres`), and a step can't reach beyond it;
/// - phase 2: `CREATE INDEX CONCURRENTLY`, `DROP INDEX CONCURRENTLY`, `REINDEX … CONCURRENTLY`;
/// - phase 1a: `ALTER TYPE … ADD VALUE`;
/// - phase 1: everything else.
///
/// Phase 2 and phase 1a statements must survive a resume or a retry (T14 minor 16): a concurrent
/// index is named and on a schema-qualified table (a resume checks it by name, and rebuilds it
/// when an interrupted build left it invalid), a concurrent drop says `IF EXISTS`, and an added
/// enum value `IF NOT EXISTS`; otherwise refused.
pub fn classify_steps(sql: &str) -> Result<Vec<ClassifiedStatement>, StepError> {
    let parsed = pg_query::parse(sql).map_err(|e| StepError {
        index: 0,
        sql: sql.trim().to_string(),
        reason: format!("the SQL doesn't parse: {e}"),
    })?;
    let mut out = Vec::new();
    for raw in &parsed.protobuf.stmts {
        let Some(node) = raw.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            continue;
        };
        let start = usize::try_from(raw.stmt_location).unwrap_or(0).min(sql.len());
        let end = match usize::try_from(raw.stmt_len) {
            Ok(0) | Err(_) => sql.len(),
            Ok(len) => (start + len).min(sql.len()),
        };
        let text = sql.get(start..end).unwrap_or("").trim().to_string();
        match classify(node).and_then(|phase| Ok((phase, resumable(node)?))) {
            Ok((phase, online)) => out.push(ClassifiedStatement {
                sql: text,
                phase,
                statement: node_name(node),
                online,
            }),
            Err(reason) => {
                return Err(StepError {
                    index: out.len(),
                    sql: text,
                    reason: reason.to_string(),
                });
            }
        }
    }
    Ok(out)
}

/// The node's variant name (`IndexStmt`, …), from its `Debug` form.
fn node_name(node: &pg_query::NodeEnum) -> String {
    let debug = format!("{node:?}");
    debug
        .split(['(', ' ', '{'])
        .next()
        .unwrap_or("")
        .to_string()
}

fn classify(node: &pg_query::NodeEnum) -> Result<Phase, &'static str> {
    use pg_query::NodeEnum as N;
    use pg_query::protobuf::{ObjectType, VariableSetKind};
    match node {
        N::TransactionStmt(_) => Err("transaction control can't run in a merge step"),
        N::CreateRoleStmt(_)
        | N::AlterRoleStmt(_)
        | N::AlterRoleSetStmt(_)
        | N::DropRoleStmt(_)
        | N::GrantRoleStmt(_) => {
            Err("role DDL can't run in a merge step: use the roles API before the merge")
        }
        N::RenameStmt(r) if r.rename_type == ObjectType::ObjectRole as i32 => {
            Err("role DDL can't run in a merge step: use the roles API before the merge")
        }
        N::CreatedbStmt(_)
        | N::DropdbStmt(_)
        | N::AlterDatabaseStmt(_)
        | N::AlterDatabaseSetStmt(_)
        | N::AlterDatabaseRefreshCollStmt(_) => Err("database DDL can't run in a merge step"),
        N::RenameStmt(r) if r.rename_type == ObjectType::ObjectDatabase as i32 => {
            Err("database DDL can't run in a merge step")
        }
        N::AlterOwnerStmt(o) if o.object_type == ObjectType::ObjectDatabase as i32 => {
            Err("database DDL can't run in a merge step")
        }
        N::AlterSystemStmt(_) => Err("ALTER SYSTEM can't run in a merge step"),
        N::VacuumStmt(_) => Err("VACUUM and ANALYZE can't run in a merge step"),
        N::VariableSetStmt(s)
            if s.kind == VariableSetKind::VarResetAll as i32
                || matches!(s.name.as_str(), "role" | "session_authorization") =>
        {
            Err("a merge step can't change the session's role")
        }
        N::DiscardStmt(_) => Err("a merge step can't change the session's role"),
        N::IndexStmt(s) if s.concurrent => Ok(Phase::Online),
        N::DropStmt(s) if s.concurrent => Ok(Phase::Online),
        N::ReindexStmt(s) if reindex_concurrently(s) => Ok(Phase::Online),
        N::AlterEnumStmt(s) if s.old_val.is_empty() && !s.new_val.is_empty() => {
            Ok(Phase::EnumAdd)
        }
        _ => Ok(Phase::Tx),
    }
}

/// T14 minor 16: a phase 2 or phase 1a statement's resume check, or why it can't be resumed.
fn resumable(node: &pg_query::NodeEnum) -> Result<Option<OnlineCheck>, &'static str> {
    use pg_query::NodeEnum as N;
    match node {
        N::IndexStmt(s) if s.concurrent => {
            if s.idxname.is_empty() {
                return Err("name the concurrent index: a resumed merge checks it by name");
            }
            let Some(rel) = &s.relation else {
                return Err("a concurrent index needs its table");
            };
            if rel.schemaname.is_empty() {
                return Err(
                    "schema-qualify the concurrent index's table: a resumed merge checks it there",
                );
            }
            Ok(Some(OnlineCheck::IndexValid {
                schema: rel.schemaname.clone(),
                table: rel.relname.clone(),
                name: s.idxname.clone(),
            }))
        }
        N::DropStmt(s) if s.concurrent && !s.missing_ok => {
            Err("write DROP INDEX CONCURRENTLY IF EXISTS: a resumed merge runs it again")
        }
        N::AlterEnumStmt(s) if s.old_val.is_empty() && !s.new_val.is_empty() && !s.skip_if_new_val_exists => {
            Err("write ADD VALUE IF NOT EXISTS: a retried merge runs it again")
        }
        _ => Ok(None),
    }
}

/// `REINDEX (CONCURRENTLY [true]) …` or `REINDEX … CONCURRENTLY` (both parse to a param).
fn reindex_concurrently(s: &pg_query::protobuf::ReindexStmt) -> bool {
    use pg_query::NodeEnum as N;
    s.params.iter().any(|p| match &p.node {
        Some(N::DefElem(d)) if d.defname.eq_ignore_ascii_case("concurrently") => {
            match d.arg.as_ref().and_then(|a| a.node.as_ref()) {
                None => true,
                Some(N::Boolean(b)) => b.boolval,
                Some(N::Integer(i)) => i.ival != 0,
                Some(N::String(s)) => {
                    !matches!(s.sval.to_ascii_lowercase().as_str(), "false" | "off" | "0")
                }
                Some(_) => true,
            }
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phases(sql: &str) -> Vec<(Phase, String)> {
        classify_steps(sql)
            .unwrap()
            .into_iter()
            .map(|s| (s.phase, s.statement))
            .collect()
    }

    fn refused(sql: &str) -> StepError {
        classify_steps(sql).expect_err(sql)
    }

    #[test]
    fn transaction_control_in_step_is_refused() {
        for sql in [
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT s",
            "START TRANSACTION",
            "END",
        ] {
            let e = refused(sql);
            assert_eq!(e.index, 0, "{sql}");
            assert!(e.reason.contains("transaction"), "{sql}: {}", e.reason);
        }
        let e = refused("UPDATE t SET a = 1;\n  COMMIT ;");
        assert_eq!(e.index, 1);
        assert_eq!(e.sql, "COMMIT");
    }

    #[test]
    fn role_ddl_in_step_is_refused() {
        for sql in [
            "CREATE ROLE r",
            "CREATE USER u",
            "ALTER ROLE r BYPASSRLS",
            "ALTER ROLE r SET search_path = x",
            "ALTER ROLE r RENAME TO s",
            "DROP ROLE r",
            "GRANT r TO s",
            "REVOKE r FROM s",
            "CREATE DATABASE d",
            "DROP DATABASE d",
            "ALTER DATABASE d SET x = 1",
            "ALTER DATABASE d OWNER TO r",
            "ALTER SYSTEM SET work_mem = '1MB'",
            "VACUUM t",
            // E15: the scripts run in the owner's own session; a step can't change its role.
            "SET ROLE cloud_admin",
            "RESET ROLE",
            "SET SESSION AUTHORIZATION cloud_admin",
            "RESET ALL",
            "DISCARD ALL",
        ] {
            let e = refused(sql);
            assert_eq!(e.sql, sql);
            assert!(!e.reason.is_empty());
        }
    }

    #[test]
    fn concurrent_statements_go_to_phase2() {
        assert_eq!(
            phases(
                "CREATE INDEX CONCURRENTLY i ON public.t (a); DROP INDEX CONCURRENTLY IF EXISTS i; \
                 REINDEX (CONCURRENTLY) INDEX i; REINDEX INDEX CONCURRENTLY i"
            ),
            [
                (Phase::Online, "IndexStmt".to_string()),
                (Phase::Online, "DropStmt".to_string()),
                (Phase::Online, "ReindexStmt".to_string()),
                (Phase::Online, "ReindexStmt".to_string()),
            ]
        );
        assert_eq!(
            phases("CREATE INDEX i ON t (a); DROP INDEX i; REINDEX INDEX i"),
            [
                (Phase::Tx, "IndexStmt".to_string()),
                (Phase::Tx, "DropStmt".to_string()),
                (Phase::Tx, "ReindexStmt".to_string()),
            ]
        );
    }

    #[test]
    fn enum_value_add_goes_to_phase1a() {
        assert_eq!(
            phases(
                "ALTER TYPE e ADD VALUE IF NOT EXISTS 'x' AFTER 'y'; ALTER TYPE e RENAME VALUE 'x' TO 'z'"
            ),
            [
                (Phase::EnumAdd, "AlterEnumStmt".to_string()),
                (Phase::Tx, "AlterEnumStmt".to_string()),
            ]
        );
    }

    #[test]
    fn statements_are_sliced_and_trimmed() {
        let s = classify_steps("  UPDATE t SET a = 'x;y' ;\n\nDO $$ BEGIN PERFORM 1; END $$;  ")
            .unwrap();
        let sqls: Vec<&str> = s.iter().map(|c| c.sql.as_str()).collect();
        assert_eq!(sqls, ["UPDATE t SET a = 'x;y'", "DO $$ BEGIN PERFORM 1; END $$"]);
        assert!(s.iter().all(|c| c.phase == Phase::Tx));
        assert!(classify_steps("-- nothing\n").unwrap().is_empty());
    }

    #[test]
    fn unparsable_sql_is_refused() {
        let e = refused("UPDATE t SET");
        assert_eq!(e.index, 0);
        assert!(e.reason.contains("parse"), "{}", e.reason);
    }
}
