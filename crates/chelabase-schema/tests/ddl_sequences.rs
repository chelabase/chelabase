//! T3 review (Critical + Minor): what the renderer knows about an object from the catalog
//! (a drop's target for the dependents check and its lock, a column's current type) describes
//! it as it is before the request. So an operation that refers to an object an earlier
//! operation of the same request created, renamed, moved, retyped or dropped must not rely on
//! that knowledge.
//!
//! Invariant:
//! - a drop (`drop_table`, `drop_column`, `drop_enum`) whose target an earlier operation
//!   changed is refused with `ChangedEarlier { by }` (`by` = the first such operation);
//! - an `alter_column` type change on a column whose table or column an earlier operation
//!   changed counts as rewriting;
//! - nothing else is refused for this reason.
use chelabase_schema::ddl::*;
use chelabase_schema::model::Table;
use chelabase_types::table_api::*;
use proptest::prelude::*;

fn t(schema: &str, name: &str) -> TableRef {
    TableRef { schema: schema.into(), name: name.into() }
}
fn ty(name: &str) -> TypeSpec {
    TypeSpec { schema: "pg_catalog".into(), name: name.into(), modifiers: vec![], array: false }
}
fn col(name: &str) -> ColumnSpec {
    ColumnSpec { name: name.into(), data_type: ty("text"), not_null: false, default: None, identity: None, generated: None, collation: None, comment: None }
}
fn retype(table: TableRef, column: &str, to: &str) -> Operation {
    Operation::AlterColumn { table, column: column.into(), change: ColumnChange { data_type: Some(ty(to)), ..Default::default() } }
}
fn cascade() -> RenderContext {
    RenderContext { cascade: true, ..ctx() }
}

/// `public.a` and `app.b`, each with `x text` and `y character varying(20)`: retyping `y` to
/// `text` doesn't rewrite while both are untouched.
fn ctx() -> RenderContext {
    let table = |schema: &str, name: &str| -> Table {
        serde_json::from_value(serde_json::json!({
            "oid": 1, "schema": schema, "name": name, "owner": { "oid": 10, "name": "postgres" }, "comment": null,
            "rls_enabled": false, "rls_forced": false, "unlogged": false, "options": [],
            "columns": [
                { "attnum": 1, "name": "x", "data_type": "text", "type_oid": 25, "collation": null, "default": null,
                  "not_null": false, "identity": null, "generated": null, "comment": null, "grants": [] },
                { "attnum": 2, "name": "y", "data_type": "character varying(20)", "type_oid": 1043, "collation": null, "default": null,
                  "not_null": false, "identity": null, "generated": null, "comment": null, "grants": [] }
            ],
            "constraints": [], "indexes": [], "policies": [], "grants": []
        }))
        .unwrap()
    };
    RenderContext {
        tables: vec![ContextTable { table: table("public", "a"), total_bytes: 0 }, ContextTable { table: table("app", "b"), total_bytes: 0 }],
        cascade: false,
        ..Default::default()
    }
}

fn refused(ops: &[Operation], c: &RenderContext) -> (usize, DdlErrorKind) {
    let e = render(ops, c).unwrap_err();
    (e.op_index, e.kind)
}

#[test]
fn the_reviews_examples_are_refused() {
    let s = |x: &str| x.to_string();
    // Renaming an enum, then dropping it by its new name.
    let ops = [
        Operation::RenameEnum { schema: s("public"), name: s("mood"), to: s("feel") },
        Operation::DropEnum { schema: s("public"), name: s("feel") },
    ];
    assert_eq!(refused(&ops, &cascade()), (1, DdlErrorKind::ChangedEarlier { by: 0 }));
    // Renaming a column, then dropping it.
    let ops = [
        Operation::RenameColumn { table: t("public", "a"), column: s("y"), to: s("z") },
        Operation::DropColumn { table: t("public", "a"), column: s("z") },
    ];
    assert_eq!(refused(&ops, &cascade()), (1, DdlErrorKind::ChangedEarlier { by: 0 }));
    // Swapping two tables' names, then dropping one: the first change of `a` is op 1.
    let ops = [
        Operation::RenameTable { table: t("public", "a"), to: s("tmp") },
        Operation::RenameTable { table: t("public", "b"), to: s("a") },
        Operation::DropTable { table: t("public", "a") },
    ];
    assert_eq!(refused(&ops, &cascade()), (2, DdlErrorKind::ChangedEarlier { by: 0 }));
    // Without cascade too (the dependents list and the lock would still name the wrong object).
    let ops = [Operation::RenameTable { table: t("public", "a"), to: s("n") }, Operation::DropTable { table: t("public", "n") }];
    assert_eq!(refused(&ops, &ctx()), (1, DdlErrorKind::ChangedEarlier { by: 0 }));
    // Creating, then dropping.
    let create = CreateTable { table: t("public", "c"), columns: vec![col("x")], primary_key: vec![], constraints: vec![], rls: None, policies: vec![], grants: vec![], comment: None, owner: None };
    let ops = [Operation::CreateTable(create), Operation::DropTable { table: t("public", "c") }];
    assert_eq!(refused(&ops, &ctx()), (1, DdlErrorKind::ChangedEarlier { by: 0 }));
    // A drop of something no earlier operation touched is fine.
    let ops = [Operation::RenameColumn { table: t("public", "a"), column: s("x"), to: s("w") }, Operation::DropTable { table: t("app", "b") }];
    assert!(render(&ops, &cascade()).is_ok());
}

#[test]
fn a_column_changed_earlier_has_an_unknown_type() {
    let s = |x: &str| x.to_string();
    let untouched = render(&[retype(t("public", "a"), "y", "text")], &ctx()).unwrap();
    assert!(!untouched[0].rewrites_table, "varchar(20) -> text doesn't rewrite");
    // The reviewer's case: `x` (text) moves aside, `y` (varchar(20)) takes its name; retyping the
    // new `x` mustn't use the old `x`'s type.
    let ops = [
        Operation::RenameColumn { table: t("public", "a"), column: s("x"), to: s("tmp") },
        Operation::RenameColumn { table: t("public", "a"), column: s("y"), to: s("x") },
        retype(t("public", "a"), "x", "varchar"),
    ];
    assert!(render(&ops, &ctx()).unwrap().last().unwrap().rewrites_table);
    let ops = [Operation::SetTableSchema { table: t("app", "b"), schema: s("public") }, retype(t("public", "b"), "y", "text")];
    assert!(render(&ops, &ctx()).unwrap().last().unwrap().rewrites_table);
}

// T3 review, Minor 2: with cascade, dropping a type locks every table whose column goes.
#[test]
fn a_cascading_type_drop_takes_access_exclusive() {
    let op = Operation::DropEnum { schema: "public".into(), name: "mood".into() };
    assert_eq!(render(std::slice::from_ref(&op), &cascade()).unwrap()[0].lock, LockLevel::AccessExclusive);
    assert_eq!(render(&[op], &ctx()).unwrap()[0].lock, LockLevel::None);
}

// T3 review, Important (coordinator ruling R4): each dependent carries Postgres's own
// description (`pg_describe_object`), which the orchestrator compares with the real drop's
// `drop cascades to …` notices; the hash covers it.
#[test]
fn dependents_carry_postgres_descriptions_in_the_hash() {
    let d = |desc: &str| Dependent { kind: "view".into(), schema: Some("public".into()), name: "v".into(), parent: None, description: desc.into() };
    assert_ne!(dependents_hash(&[d("view v")]), dependents_hash(&[d("view w")]));
    let parsed = parse_dependents(r#"[{"kind":"view","schema":"public","name":"v","parent":null,"description":"view v"}]"#).unwrap();
    assert_eq!(parsed[0].description, "view v");
    let sql = dependents_sql(&[Operation::DropTable { table: t("public", "a") }]).unwrap();
    assert!(sql.contains("pg_describe_object"), "{sql}");
}

// --- The invariant over random sequences. ---

#[derive(Clone, Debug, PartialEq, Eq)]
enum Key {
    Table(String, String),
    Column(String, String, String),
    Enum(String, String),
}

fn schema() -> impl Strategy<Value = String> {
    prop_oneof![Just("public".to_string()), Just("app".to_string())]
}
fn tname() -> impl Strategy<Value = String> {
    prop_oneof![Just("a".to_string()), Just("b".to_string()), Just("c".to_string())]
}
fn cname() -> impl Strategy<Value = String> {
    prop_oneof![Just("x".to_string()), Just("y".to_string()), Just("z".to_string())]
}

fn tr() -> impl Strategy<Value = TableRef> {
    (schema(), tname()).prop_map(|(s, n)| t(&s, &n))
}

fn op() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (schema(), tname()).prop_map(|(s, n)| Operation::CreateTable(CreateTable {
            table: t(&s, &n), columns: vec![col("x")], primary_key: vec![], constraints: vec![], rls: None,
            policies: vec![], grants: vec![], comment: None, owner: None,
        })),
        (tr(), tname()).prop_map(|(table, to)| Operation::RenameTable { table, to }),
        (tr(), schema()).prop_map(|(table, schema)| Operation::SetTableSchema { table, schema }),
        tr().prop_map(|table| Operation::DropTable { table }),
        (tr(), cname()).prop_map(|(table, c)| Operation::AddColumn { table, column: col(&c) }),
        (tr(), cname(), cname()).prop_map(|(table, column, to)| Operation::RenameColumn { table, column, to }),
        (tr(), cname()).prop_map(|(table, c)| retype(table, &c, "text")),
        (tr(), cname()).prop_map(|(table, column)| Operation::DropColumn { table, column }),
        (schema(), tname()).prop_map(|(schema, name)| Operation::CreateEnum { schema, name, values: vec![] }),
        (schema(), tname(), tname()).prop_map(|(schema, name, to)| Operation::RenameEnum { schema, name, to }),
        (schema(), tname()).prop_map(|(schema, name)| Operation::DropEnum { schema, name }),
    ]
}

/// What `op` changes: the identities it creates, renames (both names), moves (both), retypes
/// or drops.
fn changes(op: &Operation) -> Vec<Key> {
    let tk = |r: &TableRef| Key::Table(r.schema.clone(), r.name.clone());
    let ck = |r: &TableRef, c: &str| Key::Column(r.schema.clone(), r.name.clone(), c.to_string());
    match op {
        Operation::CreateTable(c) => vec![tk(&c.table)],
        Operation::RenameTable { table, to } => vec![tk(table), Key::Table(table.schema.clone(), to.clone())],
        Operation::SetTableSchema { table, schema } => vec![tk(table), Key::Table(schema.clone(), table.name.clone())],
        Operation::DropTable { table } => vec![tk(table)],
        Operation::AddColumn { table, column } => vec![ck(table, &column.name)],
        Operation::RenameColumn { table, column, to } => vec![ck(table, column), ck(table, to)],
        Operation::AlterColumn { table, column, .. } => vec![ck(table, column)],
        Operation::DropColumn { table, column } => vec![ck(table, column)],
        Operation::CreateEnum { schema, name, .. } | Operation::DropEnum { schema, name } => vec![Key::Enum(schema.clone(), name.clone())],
        Operation::RenameEnum { schema, name, to } => vec![Key::Enum(schema.clone(), name.clone()), Key::Enum(schema.clone(), to.clone())],
        _ => vec![],
    }
}

/// The keys an operation's catalog knowledge depends on (a drop's target; a retyped column).
fn depends_on(op: &Operation) -> Vec<Key> {
    match op {
        Operation::DropTable { table } => vec![Key::Table(table.schema.clone(), table.name.clone())],
        Operation::DropColumn { table, column } | Operation::AlterColumn { table, column, .. } => vec![
            Key::Table(table.schema.clone(), table.name.clone()),
            Key::Column(table.schema.clone(), table.name.clone(), column.clone()),
        ],
        Operation::DropEnum { schema, name } => vec![Key::Enum(schema.clone(), name.clone())],
        _ => vec![],
    }
}

/// The first earlier operation changing what `ops[i]` depends on.
fn changed_by(ops: &[Operation], i: usize) -> Option<usize> {
    let deps = depends_on(&ops[i]);
    let column_key = |k: &Key| match k {
        // A column change also changes the table's column under that name only.
        Key::Column(s, t, _) => Some(Key::Table(s.clone(), t.clone())),
        _ => None,
    };
    (0..i).find(|&j| {
        changes(&ops[j]).iter().any(|k| {
            deps.contains(k)
                // A change to any column of a table matters to a drop of that same column only.
                && !(matches!(ops[i], Operation::DropTable { .. }) && column_key(k).is_some())
        })
    })
}

fn is_drop(op: &Operation) -> bool {
    matches!(op, Operation::DropTable { .. } | Operation::DropColumn { .. } | Operation::DropEnum { .. })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn catalog_knowledge_is_never_stale(ops in proptest::collection::vec(op(), 1..6), cascade in any::<bool>()) {
        let c = RenderContext { cascade, ..ctx() };
        // The first drop changed earlier is the refusal, if any.
        let first_refusal = (0..ops.len()).find_map(|i| (is_drop(&ops[i])).then(|| changed_by(&ops, i).map(|by| (i, by))).flatten());
        match render(&ops, &c) {
            Ok(stmts) => {
                prop_assert!(first_refusal.is_none(), "should refuse {first_refusal:?}: {ops:?}");
                for s in &stmts {
                    if let Operation::AlterColumn { .. } = &ops[s.op_index]
                        && changed_by(&ops, s.op_index).is_some()
                    {
                        prop_assert!(s.rewrites_table, "a type change after a change must count as rewriting: {ops:?}");
                    }
                }
            }
            Err(e) => {
                let (i, by) = first_refusal.unwrap_or_else(|| panic!("refused without a reason: {e:?} {ops:?}"));
                prop_assert_eq!((e.op_index, e.kind), (i, DdlErrorKind::ChangedEarlier { by }));
            }
        }
    }
}
