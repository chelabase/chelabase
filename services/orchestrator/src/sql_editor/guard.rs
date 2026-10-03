//! The statement guard: classifies a script's statements before it runs.

use pg_query::NodeEnum;
use pg_query::protobuf::{AlterTableType, ExplainStmt, Node, SelectStmt, SetOperation};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct HeldStatement {
    /// 0-based, script order (empty statements don't count).
    pub index: usize,
    /// pg_query node name (`DropStmt`, `TruncateStmt`, ...) or `unparsed`.
    pub kind: String,
    /// The statement's first 200 chars, trimmed.
    pub text_preview: String,
    pub reason: HoldReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    Risky,
    Protected,
}

const PREVIEW_CHARS: usize = 200;

fn preview(text: &str) -> String {
    text.trim().chars().take(PREVIEW_CHARS).collect()
}

/// The statements `sql` must not run without `confirm`. Empty means run.
pub fn check(sql: &str, protected: bool) -> Vec<HeldStatement> {
    let reason = if protected { HoldReason::Protected } else { HoldReason::Risky };
    let Ok(parsed) = pg_query::parse(sql) else {
        return vec![HeldStatement {
            index: 0,
            kind: "unparsed".to_owned(),
            text_preview: preview(sql),
            reason,
        }];
    };
    let mut held = Vec::new();
    let mut index = 0;
    for raw in &parsed.protobuf.stmts {
        let Some(node) = raw.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            continue;
        };
        let hold = if protected { !is_read(node) } else { is_risky(node) };
        if hold {
            let start = usize::try_from(raw.stmt_location).unwrap_or(0).min(sql.len());
            let end = match usize::try_from(raw.stmt_len) {
                Ok(0) | Err(_) => sql.len(),
                Ok(len) => (start + len).min(sql.len()),
            };
            held.push(HeldStatement {
                index,
                kind: kind_name(node).to_owned(),
                text_preview: preview(sql.get(start..end).unwrap_or("")),
                reason,
            });
        }
        index += 1;
    }
    held
}

/// `NodeEnum::$variant(_) => "$variant"` for every variant listed: an exhaustive match, so
/// a pg_query upgrade that adds a node fails to compile here until it is listed.
macro_rules! node_names {
    ($node:expr; $($variant:ident),+ $(,)?) => {
        match $node {
            $(NodeEnum::$variant(_) => stringify!($variant),)+
        }
    };
}

/// The variant name, e.g. `DropStmt` (pg_query 6.2's node list).
fn kind_name(node: &NodeEnum) -> &'static str {
    node_names!(node;
        Alias, RangeVar, TableFunc, IntoClause, Var, Param, Aggref, GroupingFunc, WindowFunc,
        WindowFuncRunCondition, MergeSupportFunc, SubscriptingRef, FuncExpr, NamedArgExpr,
        OpExpr, DistinctExpr, NullIfExpr, ScalarArrayOpExpr, BoolExpr, SubLink, SubPlan,
        AlternativeSubPlan, FieldSelect, FieldStore, RelabelType, CoerceViaIo, ArrayCoerceExpr,
        ConvertRowtypeExpr, CollateExpr, CaseExpr, CaseWhen, CaseTestExpr, ArrayExpr, RowExpr,
        RowCompareExpr, CoalesceExpr, MinMaxExpr, SqlvalueFunction, XmlExpr, JsonFormat,
        JsonReturning, JsonValueExpr, JsonConstructorExpr, JsonIsPredicate, JsonBehavior,
        JsonExpr, JsonTablePath, JsonTablePathScan, JsonTableSiblingJoin, NullTest, BooleanTest,
        MergeAction, CoerceToDomain, CoerceToDomainValue, SetToDefault, CurrentOfExpr,
        NextValueExpr, InferenceElem, TargetEntry, RangeTblRef, JoinExpr, FromExpr,
        OnConflictExpr, Query, TypeName, ColumnRef, ParamRef, AExpr, TypeCast, CollateClause,
        RoleSpec, FuncCall, AStar, AIndices, AIndirection, AArrayExpr, ResTarget, MultiAssignRef,
        SortBy, WindowDef, RangeSubselect, RangeFunction, RangeTableFunc, RangeTableFuncCol,
        RangeTableSample, ColumnDef, TableLikeClause, IndexElem, DefElem, LockingClause,
        XmlSerialize, PartitionElem, PartitionSpec, PartitionBoundSpec, PartitionRangeDatum,
        SinglePartitionSpec, PartitionCmd, RangeTblEntry, RtepermissionInfo, RangeTblFunction,
        TableSampleClause, WithCheckOption, SortGroupClause, GroupingSet, WindowClause,
        RowMarkClause, WithClause, InferClause, OnConflictClause, CtesearchClause,
        CtecycleClause, CommonTableExpr, MergeWhenClause, TriggerTransition, JsonOutput,
        JsonArgument, JsonFuncExpr, JsonTablePathSpec, JsonTable, JsonTableColumn, JsonKeyValue,
        JsonParseExpr, JsonScalarExpr, JsonSerializeExpr, JsonObjectConstructor,
        JsonArrayConstructor, JsonArrayQueryConstructor, JsonAggConstructor, JsonObjectAgg,
        JsonArrayAgg, RawStmt, InsertStmt, DeleteStmt, UpdateStmt, MergeStmt, SelectStmt,
        SetOperationStmt, ReturnStmt, PlassignStmt, CreateSchemaStmt, AlterTableStmt,
        ReplicaIdentityStmt, AlterTableCmd, AlterCollationStmt, AlterDomainStmt, GrantStmt,
        ObjectWithArgs, AccessPriv, GrantRoleStmt, AlterDefaultPrivilegesStmt, CopyStmt,
        VariableSetStmt, VariableShowStmt, CreateStmt, Constraint, CreateTableSpaceStmt,
        DropTableSpaceStmt, AlterTableSpaceOptionsStmt, AlterTableMoveAllStmt,
        CreateExtensionStmt, AlterExtensionStmt, AlterExtensionContentsStmt, CreateFdwStmt,
        AlterFdwStmt, CreateForeignServerStmt, AlterForeignServerStmt, CreateForeignTableStmt,
        CreateUserMappingStmt, AlterUserMappingStmt, DropUserMappingStmt,
        ImportForeignSchemaStmt, CreatePolicyStmt, AlterPolicyStmt, CreateAmStmt, CreateTrigStmt,
        CreateEventTrigStmt, AlterEventTrigStmt, CreatePlangStmt, CreateRoleStmt, AlterRoleStmt,
        AlterRoleSetStmt, DropRoleStmt, CreateSeqStmt, AlterSeqStmt, DefineStmt,
        CreateDomainStmt, CreateOpClassStmt, CreateOpClassItem, CreateOpFamilyStmt,
        AlterOpFamilyStmt, DropStmt, TruncateStmt, CommentStmt, SecLabelStmt, DeclareCursorStmt,
        ClosePortalStmt, FetchStmt, IndexStmt, CreateStatsStmt, StatsElem, AlterStatsStmt,
        CreateFunctionStmt, FunctionParameter, AlterFunctionStmt, DoStmt, InlineCodeBlock,
        CallStmt, CallContext, RenameStmt, AlterObjectDependsStmt, AlterObjectSchemaStmt,
        AlterOwnerStmt, AlterOperatorStmt, AlterTypeStmt, RuleStmt, NotifyStmt, ListenStmt,
        UnlistenStmt, TransactionStmt, CompositeTypeStmt, CreateEnumStmt, CreateRangeStmt,
        AlterEnumStmt, ViewStmt, LoadStmt, CreatedbStmt, AlterDatabaseStmt,
        AlterDatabaseRefreshCollStmt, AlterDatabaseSetStmt, DropdbStmt, AlterSystemStmt,
        ClusterStmt, VacuumStmt, VacuumRelation, ExplainStmt, CreateTableAsStmt,
        RefreshMatViewStmt, CheckPointStmt, DiscardStmt, LockStmt, ConstraintsSetStmt,
        ReindexStmt, CreateConversionStmt, CreateCastStmt, CreateTransformStmt, PrepareStmt,
        ExecuteStmt, DeallocateStmt, DropOwnedStmt, ReassignOwnedStmt, AlterTsdictionaryStmt,
        AlterTsconfigurationStmt, PublicationTable, PublicationObjSpec, CreatePublicationStmt,
        AlterPublicationStmt, CreateSubscriptionStmt, AlterSubscriptionStmt,
        DropSubscriptionStmt, Integer, Float, Boolean, String, BitString, List, IntList, OidList,
        AConst
    )
}

fn is_risky(node: &NodeEnum) -> bool {
    match node {
        // What `EXPLAIN ANALYZE` and `PREPARE` wrap runs (`PREPARE` once executed).
        NodeEnum::ExplainStmt(e) => explain_analyzes(e) && inner_is_risky(e.query.as_deref()),
        NodeEnum::PrepareStmt(p) => inner_is_risky(p.query.as_deref()),
        NodeEnum::DropStmt(_)
        | NodeEnum::DropdbStmt(_)
        | NodeEnum::DropRoleStmt(_)
        | NodeEnum::DropOwnedStmt(_)
        | NodeEnum::DropSubscriptionStmt(_)
        | NodeEnum::DropTableSpaceStmt(_)
        | NodeEnum::TruncateStmt(_) => true,
        NodeEnum::DeleteStmt(s) => s.where_clause.is_none(),
        NodeEnum::UpdateStmt(s) => s.where_clause.is_none(),
        NodeEnum::AlterTableStmt(s) => s.cmds.iter().any(|c| {
            matches!(
                &c.node,
                Some(NodeEnum::AlterTableCmd(cmd))
                    if cmd.subtype == AlterTableType::AtDropColumn as i32
                        || cmd.subtype == AlterTableType::AtDropConstraint as i32
            )
        }),
        _ => false,
    }
}

fn is_read(node: &NodeEnum) -> bool {
    match node {
        NodeEnum::SelectStmt(s) => select_is_read(s),
        NodeEnum::VariableShowStmt(_) => true,
        NodeEnum::ExplainStmt(e) => {
            !explain_analyzes(e)
                && e.query
                    .as_ref()
                    .and_then(|q| q.node.as_ref())
                    .is_some_and(is_read)
        }
        _ => false,
    }
}

fn inner_is_risky(query: Option<&Node>) -> bool {
    query.and_then(|q| q.node.as_ref()).is_some_and(is_risky)
}

/// Whether the EXPLAIN runs its statement: an `analyze` option (`ANALYSE` parses to the same
/// name) without a false value.
fn explain_analyzes(e: &ExplainStmt) -> bool {
    e.options.iter().any(|o| match &o.node {
        Some(NodeEnum::DefElem(d)) if d.defname.eq_ignore_ascii_case("analyze") => {
            match d.arg.as_ref().and_then(|a| a.node.as_ref()) {
                None => true,
                Some(arg) => arg_is_true(arg),
            }
        }
        _ => false,
    })
}

/// EXPLAIN option values: `true`/`on`/`1` parse as String or Integer/Boolean nodes.
fn arg_is_true(arg: &NodeEnum) -> bool {
    match arg {
        NodeEnum::String(s) => !matches!(s.sval.to_ascii_lowercase().as_str(), "false" | "off" | "0"),
        NodeEnum::Integer(i) => i.ival != 0,
        NodeEnum::Boolean(b) => b.boolval,
        _ => true,
    }
}

fn select_is_read(s: &SelectStmt) -> bool {
    if s.into_clause.is_some() || !s.locking_clause.is_empty() {
        return false;
    }
    if let Some(with) = &s.with_clause {
        for cte in &with.ctes {
            if let Some(NodeEnum::CommonTableExpr(c)) = &cte.node {
                let writes = matches!(
                    c.ctequery.as_ref().and_then(|q| q.node.as_ref()),
                    Some(
                        NodeEnum::InsertStmt(_)
                            | NodeEnum::UpdateStmt(_)
                            | NodeEnum::DeleteStmt(_)
                            | NodeEnum::MergeStmt(_)
                    )
                );
                if writes {
                    return false;
                }
            }
        }
    }
    if s.op != SetOperation::SetopNone as i32 {
        for side in [&s.larg, &s.rarg] {
            if let Some(sub) = side
                && !select_is_read(sub)
            {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(h: &[HeldStatement]) -> Vec<usize> {
        h.iter().map(|x| x.index).collect()
    }

    #[test]
    fn drop_table_is_risky() {
        let h = check("DROP TABLE t", false);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].kind, "DropStmt");
        assert_eq!(h[0].reason, HoldReason::Risky);
        assert_eq!(h[0].text_preview, "DROP TABLE t");
    }

    #[test]
    fn delete_without_where_is_risky_with_where_is_not() {
        let h = check("DELETE FROM t; DELETE FROM t WHERE id = 1", false);
        assert_eq!(idx(&h), vec![0]);
        assert_eq!(h[0].kind, "DeleteStmt");
    }

    #[test]
    fn update_without_where_is_risky() {
        assert_eq!(idx(&check("UPDATE t SET a = 1; UPDATE t SET a = 1 WHERE b", false)), vec![0]);
    }

    #[test]
    fn truncate_is_risky() {
        assert_eq!(check("TRUNCATE t", false)[0].kind, "TruncateStmt");
    }

    #[test]
    fn alter_table_drop_column_is_risky_add_column_is_not() {
        let h = check("ALTER TABLE t ADD COLUMN a int; ALTER TABLE t DROP COLUMN a; ALTER TABLE t DROP CONSTRAINT c", false);
        assert_eq!(idx(&h), vec![1, 2]);
        assert!(check("ALTER TABLE t ADD COLUMN a int", false).is_empty());
    }

    #[test]
    fn create_and_insert_pass_on_unprotected() {
        assert!(check("CREATE TABLE t (a int); INSERT INTO t VALUES (1)", false).is_empty());
    }

    #[test]
    fn dollar_quoted_drop_is_not_risky() {
        assert!(check("DO $$ BEGIN DROP TABLE t; END $$", false).is_empty());
    }

    #[test]
    fn comment_only_script_holds_nothing() {
        assert!(check("-- hi\n/* x */", false).is_empty());
        assert!(check("-- hi\n/* x */", true).is_empty());
    }

    #[test]
    fn protected_holds_every_write() {
        let h = check("SELECT 1; INSERT INTO t VALUES (1); CREATE TABLE u (a int)", true);
        assert_eq!(idx(&h), vec![1, 2]);
        assert!(h.iter().all(|x| x.reason == HoldReason::Protected));
        let h = check("DROP TABLE t", true);
        assert_eq!(h[0].reason, HoldReason::Protected);
    }

    #[test]
    fn protected_allows_reads() {
        let sql = "SELECT * FROM t; VALUES (1); TABLE t; SHOW search_path; EXPLAIN SELECT 1; \
                   SELECT 1 UNION SELECT 2; EXPLAIN (ANALYZE false) SELECT 1";
        assert!(check(sql, true).is_empty());
    }

    #[test]
    fn explain_analyze_is_a_write() {
        for sql in [
            "EXPLAIN ANALYZE SELECT 1",
            "EXPLAIN (ANALYZE) SELECT 1",
            "EXPLAIN (ANALYZE true) SELECT 1",
            "EXPLAIN (analyze on, verbose) DELETE FROM t",
            "EXPLAIN SELECT 1 INTO x",
        ] {
            assert_eq!(check(sql, true).len(), 1, "{sql}");
        }
    }

    #[test]
    fn select_into_and_for_update_are_writes() {
        assert_eq!(check("SELECT 1 INTO x", true).len(), 1);
        assert_eq!(check("SELECT * FROM t FOR UPDATE", true).len(), 1);
    }

    #[test]
    fn data_modifying_cte_is_a_write() {
        let sql = "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d";
        assert_eq!(check(sql, true).len(), 1);
        assert!(check(sql, false).is_empty());
        let nested = "SELECT 1 UNION ALL (WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d)";
        assert_eq!(check(nested, true).len(), 1);
    }

    #[test]
    fn unparsed_fails_closed() {
        let h = check("SELEC 1", false);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].kind, "unparsed");
        assert_eq!(h[0].reason, HoldReason::Risky);
        assert_eq!(check("SELEC 1", true)[0].reason, HoldReason::Protected);
    }

    #[test]
    fn indexes_skip_empty_statements() {
        assert_eq!(idx(&check(";; DROP TABLE t", false)), vec![0]);
    }

    #[test]
    fn merge_and_call_are_writes_on_protected_only() {
        let merge = "MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN DELETE";
        let h = check(merge, true);
        assert_eq!((h.len(), h[0].kind.as_str()), (1, "MergeStmt"));
        assert!(check(merge, false).is_empty());
        let h = check("CALL p(1)", true);
        assert_eq!((h.len(), h[0].kind.as_str()), (1, "CallStmt"));
        assert!(check("CALL p(1)", false).is_empty());
    }

    #[test]
    fn british_analyse_is_a_write() {
        for sql in ["EXPLAIN (ANALYSE) SELECT 1", "EXPLAIN ANALYSE SELECT 1"] {
            let h = check(sql, true);
            assert_eq!((h.len(), h[0].kind.as_str()), (1, "ExplainStmt"), "{sql}");
        }
    }

    #[test]
    fn explain_analyze_of_a_risky_statement_is_risky() {
        for sql in [
            "EXPLAIN ANALYZE DELETE FROM t",
            "EXPLAIN (ANALYZE, VERBOSE) UPDATE t SET a = 1",
            "EXPLAIN (ANALYSE true) DELETE FROM t",
        ] {
            let h = check(sql, false);
            assert_eq!((h.len(), h[0].kind.as_str()), (1, "ExplainStmt"), "{sql}");
        }
        for sql in [
            "EXPLAIN DELETE FROM t",
            "EXPLAIN (ANALYZE false) DELETE FROM t",
            "EXPLAIN ANALYZE DELETE FROM t WHERE a = 1",
        ] {
            assert!(check(sql, false).is_empty(), "{sql}");
        }
    }

    #[test]
    fn prepare_of_a_risky_statement_is_risky() {
        let h = check("PREPARE p AS DELETE FROM t", false);
        assert_eq!((h.len(), h[0].kind.as_str()), (1, "PrepareStmt"));
        assert!(check("PREPARE p AS DELETE FROM t WHERE a = $1", false).is_empty());
        // EXECUTE can't be judged without the prepared statement: a known limit.
        assert!(check("EXECUTE p", false).is_empty());
    }

    #[test]
    fn multibyte_previews_after_other_statements() {
        let sql = "SELECT 'ü'; DROP TABLE \"é✓\"; SELECT '😀'; TRUNCATE \"ß\"";
        let h = check(sql, false);
        assert_eq!(idx(&h), vec![1, 3]);
        assert_eq!(h[0].text_preview, "DROP TABLE \"é✓\"");
        assert_eq!(h[1].text_preview, "TRUNCATE \"ß\"");
    }

    #[test]
    fn kind_names_are_the_node_names() {
        for (sql, kind) in [
            ("DROP TABLE t", "DropStmt"),
            ("TRUNCATE t", "TruncateStmt"),
            ("INSERT INTO t VALUES (1)", "InsertStmt"),
            ("CREATE TABLE u (a int)", "CreateStmt"),
            ("ALTER TABLE t ADD COLUMN b int", "AlterTableStmt"),
            ("VACUUM", "VacuumStmt"),
            ("DO $$ BEGIN END $$", "DoStmt"),
            ("SET search_path = x", "VariableSetStmt"),
            ("BEGIN", "TransactionStmt"),
            ("GRANT SELECT ON t TO r", "GrantStmt"),
        ] {
            assert_eq!(check(sql, true)[0].kind, kind, "{sql}");
        }
    }

    #[test]
    fn preview_is_200_chars_and_trimmed() {
        let long = format!("  DROP TABLE \"{}\"  ", "é".repeat(300));
        let h = check(&long, false);
        assert_eq!(h[0].text_preview.chars().count(), 200);
        assert!(h[0].text_preview.starts_with("DROP TABLE"));
    }
}
