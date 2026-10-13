use chelabase_schema::ddl::*;
use chelabase_schema::model::Table;
use chelabase_types::table_api::*;
use proptest::prelude::*;

fn t(schema: &str, name: &str) -> TableRef {
    TableRef { schema: schema.into(), name: name.into() }
}
fn notes() -> TableRef {
    t("public", "notes")
}
fn sqls(ops: &[Operation]) -> Vec<String> {
    render(ops, &RenderContext::default()).unwrap().into_iter().map(|s| s.sql).collect()
}
fn index(name: Option<&str>, keys: Vec<IndexKey>) -> IndexSpec {
    IndexSpec { table: notes(), name: name.map(Into::into), method: None, unique: false, nulls_not_distinct: false, keys, include: vec![], predicate: None, concurrently: false }
}
fn key(name: &str) -> IndexKey {
    IndexKey::Column { name: name.into(), descending: false, nulls: None }
}

#[test]
fn index_operations() {
    let mut full = index(Some("notes_lower_idx"), vec![
        IndexKey::Expression { sql: "lower(body)".into(), descending: true, nulls: Some(NullsOrder::Last) },
        IndexKey::Column { name: "Created".into(), descending: false, nulls: Some(NullsOrder::First) },
    ]);
    full.unique = true;
    full.nulls_not_distinct = true;
    full.include = vec!["id".into()];
    full.predicate = Some("deleted_at IS NULL".into());
    full.method = Some("btree".into());
    let mut gin = index(None, vec![key("tags")]);
    gin.method = Some("gin".into());
    assert_eq!(
        sqls(&[
            Operation::CreateIndex(index(Some("notes_body_idx"), vec![key("body")])),
            Operation::CreateIndex(full),
            Operation::CreateIndex(gin),
            Operation::DropIndex { schema: "public".into(), name: "notes_body_idx".into(), concurrently: false },
            Operation::RenameIndex { schema: "public".into(), name: "a".into(), to: "b".into() },
        ]),
        [
            "CREATE INDEX notes_body_idx ON public.notes USING btree (body)",
            "CREATE UNIQUE INDEX notes_lower_idx ON public.notes USING btree ((lower(body)) DESC NULLS LAST, \"Created\" NULLS FIRST) INCLUDE (id) NULLS NOT DISTINCT WHERE (deleted_at IS NULL)",
            "CREATE INDEX ON public.notes USING gin (tags)",
            "DROP INDEX public.notes_body_idx RESTRICT",
            "ALTER INDEX public.a RENAME TO b",
        ]
    );
}

#[test]
fn concurrently_is_alone_and_marked() {
    let mut ix = index(Some("i"), vec![key("body")]);
    ix.concurrently = true;
    let out = render(&[Operation::CreateIndex(ix.clone())], &RenderContext::default()).unwrap();
    assert_eq!(out[0].sql, "CREATE INDEX CONCURRENTLY i ON public.notes USING btree (body)");
    assert!(out[0].concurrently);
    assert_eq!(out[0].lock, LockLevel::ShareUpdateExclusive);
    let drop = render(&[Operation::DropIndex { schema: "public".into(), name: "i".into(), concurrently: true }], &RenderContext::default()).unwrap();
    assert_eq!(drop[0].sql, "DROP INDEX CONCURRENTLY public.i");
    let e = render(&[Operation::DropTable { table: t("public", "x") }, Operation::CreateIndex(ix)], &RenderContext::default()).unwrap_err();
    assert_eq!((e.op_index, e.kind), (1, DdlErrorKind::ConcurrentlyNotAlone));
    let cascade = RenderContext { cascade: true, ..Default::default() };
    let e = render(&[Operation::DropIndex { schema: "public".into(), name: "i".into(), concurrently: true }], &cascade).unwrap_err();
    assert_eq!(e.kind, DdlErrorKind::Unsupported("cascade with concurrently"));
    assert_eq!(render(&[Operation::CreateIndex(index(None, vec![]))], &RenderContext::default()).unwrap_err().kind, DdlErrorKind::Empty("keys"));
}

#[test]
fn rls_and_policy_operations() {
    let policy = PolicySpec {
        name: "Pay".into(),
        command: PolicyCommandSpec::Update,
        permissive: false,
        roles: vec!["Billing".into(), "anon".into()],
        using: Some("true".into()),
        with_check: Some("amount > 0".into()),
    };
    let public = PolicySpec { name: "all_rows".into(), command: PolicyCommandSpec::All, permissive: true, roles: vec![], using: Some("(true)".into()), with_check: None };
    assert_eq!(
        sqls(&[
            Operation::SetRls { table: notes(), enabled: true, forced: false },
            Operation::CreatePolicy { table: notes(), policy },
            Operation::CreatePolicy { table: notes(), policy: public },
            Operation::AlterPolicy { table: notes(), name: "Pay".into(), roles: Some(vec!["anon".into()]), using: Some("true".into()), with_check: Some("x".into()) },
            Operation::AlterPolicy { table: notes(), name: "Pay".into(), roles: Some(vec![]), using: None, with_check: None },
            Operation::RenamePolicy { table: notes(), name: "Pay".into(), to: "pay".into() },
            Operation::DropPolicy { table: notes(), name: "pay".into() },
        ]),
        [
            "ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE public.notes NO FORCE ROW LEVEL SECURITY",
            "CREATE POLICY \"Pay\" ON public.notes AS RESTRICTIVE FOR UPDATE TO \"Billing\", anon USING (true) WITH CHECK (amount > 0)",
            "CREATE POLICY all_rows ON public.notes AS PERMISSIVE FOR ALL TO PUBLIC USING (true)",
            "ALTER POLICY \"Pay\" ON public.notes TO anon USING (true) WITH CHECK (x)",
            "ALTER POLICY \"Pay\" ON public.notes TO PUBLIC",
            "ALTER POLICY \"Pay\" ON public.notes RENAME TO pay",
            "DROP POLICY pay ON public.notes",
        ]
    );
    let e = render(&[Operation::AlterPolicy { table: notes(), name: "p".into(), roles: None, using: None, with_check: None }], &RenderContext::default()).unwrap_err();
    assert_eq!(e.kind, DdlErrorKind::Empty("change"));
}

#[test]
fn grant_operations_one_statement_per_privilege_and_column() {
    let g = |columns: &[&str], privileges: Vec<Privilege>, grantee: Grantee, with_grant_option: bool| TableGrant {
        columns: columns.iter().map(|c| c.to_string()).collect(),
        privileges,
        grantee,
        with_grant_option,
    };
    assert_eq!(
        sqls(&[
            Operation::Grant { table: notes(), grant: g(&[], vec![Privilege::Select], Grantee::Public, false) },
            Operation::Grant { table: notes(), grant: g(&["body", "Tags"], vec![Privilege::Update, Privilege::Insert], Grantee::Role("Editors".into()), true) },
            Operation::Grant { table: notes(), grant: g(&[], vec![Privilege::Maintain, Privilege::Truncate, Privilege::References, Privilege::Trigger, Privilege::Delete], Grantee::Role("ops".into()), false) },
            Operation::Revoke { table: notes(), grant: g(&[], vec![Privilege::Select], Grantee::Role("anon".into()), false) },
            Operation::Revoke { table: notes(), grant: g(&["body"], vec![Privilege::Update], Grantee::Role("Editors".into()), true) },
        ]),
        [
            "GRANT SELECT ON public.notes TO PUBLIC",
            "GRANT UPDATE (body) ON public.notes TO \"Editors\" WITH GRANT OPTION",
            "GRANT UPDATE (\"Tags\") ON public.notes TO \"Editors\" WITH GRANT OPTION",
            "GRANT INSERT (body) ON public.notes TO \"Editors\" WITH GRANT OPTION",
            "GRANT INSERT (\"Tags\") ON public.notes TO \"Editors\" WITH GRANT OPTION",
            "GRANT MAINTAIN ON public.notes TO ops",
            "GRANT TRUNCATE ON public.notes TO ops",
            "GRANT REFERENCES ON public.notes TO ops",
            "GRANT TRIGGER ON public.notes TO ops",
            "GRANT DELETE ON public.notes TO ops",
            "REVOKE SELECT ON public.notes FROM anon",
            "REVOKE GRANT OPTION FOR UPDATE (body) ON public.notes FROM \"Editors\"",
        ]
    );
    let none = g(&[], vec![], Grantee::Public, false);
    assert_eq!(render(&[Operation::Grant { table: notes(), grant: none }], &RenderContext::default()).unwrap_err().kind, DdlErrorKind::Empty("privileges"));
}

#[test]
fn enum_operations() {
    let s = |x: &str| x.to_string();
    assert_eq!(
        sqls(&[
            Operation::CreateEnum { schema: s("public"), name: s("mood"), values: vec![s("sad"), s("ok"), s("it's great")] },
            Operation::AddEnumValue { schema: s("public"), name: s("mood"), value: s("meh"), position: Some(EnumPosition::Before(s("ok"))) },
            Operation::AddEnumValue { schema: s("public"), name: s("mood"), value: s("wow"), position: Some(EnumPosition::After(s("it's great"))) },
            Operation::AddEnumValue { schema: s("public"), name: s("mood"), value: s("last"), position: None },
            Operation::RenameEnumValue { schema: s("public"), name: s("mood"), from: s("ok"), to: s("fine") },
            Operation::RenameEnum { schema: s("public"), name: s("mood"), to: s("Feeling") },
            Operation::DropEnum { schema: s("public"), name: s("Old") },
        ]),
        [
            "CREATE TYPE public.mood AS ENUM ('sad', 'ok', 'it''s great')",
            "ALTER TYPE public.mood ADD VALUE 'meh' BEFORE 'ok'",
            "ALTER TYPE public.mood ADD VALUE 'wow' AFTER 'it''s great'",
            "ALTER TYPE public.mood ADD VALUE 'last'",
            "ALTER TYPE public.mood RENAME VALUE 'ok' TO 'fine'",
            "ALTER TYPE public.mood RENAME TO \"Feeling\"",
            "DROP TYPE public.\"Old\" RESTRICT",
        ]
    );
    let out = render(&[Operation::CreateEnum { schema: s("public"), name: s("e"), values: vec![] }], &RenderContext::default()).unwrap();
    assert_eq!((out[0].sql.as_str(), out[0].table.clone(), out[0].lock), ("CREATE TYPE public.e AS ENUM ()", None, LockLevel::None));
    let long = "é".repeat(32);
    let e = render(&[Operation::CreateEnum { schema: s("public"), name: s("e"), values: vec![long.clone()] }], &RenderContext::default()).unwrap_err();
    assert_eq!(e.kind, DdlErrorKind::InvalidName(long), "labels are names: at most 63 bytes");
}

#[test]
fn locks_of_the_remaining_operations() {
    // `LockLevel::None` and `Option::None` both appear: no glob import.
    use LockLevel::{AccessExclusive, Share, ShareUpdateExclusive};
    let lock = |op: Operation| render(&[op], &RenderContext::default()).unwrap()[0].lock;
    let g = TableGrant { columns: vec![], privileges: vec![Privilege::Select], grantee: Grantee::Public, with_grant_option: false };
    assert_eq!(lock(Operation::CreateIndex(index(None, vec![key("a")]))), Share);
    assert_eq!(lock(Operation::DropIndex { schema: "public".into(), name: "i".into(), concurrently: false }), AccessExclusive);
    assert_eq!(lock(Operation::RenameIndex { schema: "public".into(), name: "i".into(), to: "j".into() }), ShareUpdateExclusive);
    assert_eq!(lock(Operation::SetRls { table: notes(), enabled: true, forced: true }), AccessExclusive);
    assert_eq!(lock(Operation::CreatePolicy { table: notes(), policy: PolicySpec { name: "p".into(), command: PolicyCommandSpec::All, permissive: true, roles: vec![], using: None, with_check: None } }), AccessExclusive);
    assert_eq!(lock(Operation::Grant { table: notes(), grant: g.clone() }), LockLevel::None);
    assert_eq!(lock(Operation::Revoke { table: notes(), grant: g }), LockLevel::None);
    assert_eq!(lock(Operation::AddEnumValue { schema: "public".into(), name: "e".into(), value: "v".into(), position: None }), LockLevel::None);
}

fn table_with(columns: &[(&str, &str)]) -> Table {
    serde_json::from_value(serde_json::json!({
        "oid": 1, "schema": "public", "name": "notes", "owner": { "oid": 10, "name": "postgres" }, "comment": null,
        "rls_enabled": false, "rls_forced": false, "unlogged": false, "options": [],
        "columns": columns.iter().enumerate().map(|(i, (n, ty))| serde_json::json!({
            "attnum": i + 1, "name": n, "data_type": ty, "type_oid": 25, "collation": null, "default": null,
            "not_null": false, "identity": null, "generated": null, "comment": null, "grants": []
        })).collect::<Vec<_>>(),
        "constraints": [], "indexes": [], "policies": [], "grants": []
    }))
    .unwrap()
}

fn ctx(total_bytes: i64) -> RenderContext {
    RenderContext {
        tables: vec![ContextTable {
            table: table_with(&[("body", "character varying(20)"), ("price", "numeric(10,2)"), ("note", "text"), ("n", "integer")]),
            total_bytes,
        }],
        cascade: false,
    }
}

fn retype(column: &str, name: &str, modifiers: &[i32], using: Option<&str>) -> Operation {
    Operation::AlterColumn {
        table: notes(),
        column: column.into(),
        change: ColumnChange {
            data_type: Some(TypeSpec { schema: "pg_catalog".into(), name: name.into(), modifiers: modifiers.to_vec(), array: false }),
            using: using.map(Into::into),
            ..Default::default()
        },
    }
}

#[test]
fn type_changes_that_rewrite() {
    let rewrites = |op: Operation, c: &RenderContext| render(&[op], c).unwrap()[0].rewrites_table;
    let c = ctx(0);
    for (op, want) in [
        (retype("body", "varchar", &[40], None), false),
        (retype("body", "varchar", &[20], None), false),
        (retype("body", "varchar", &[10], None), true),
        (retype("body", "varchar", &[], None), false),
        (retype("body", "text", &[], None), false),
        (retype("note", "varchar", &[], None), false),
        (retype("note", "varchar", &[5], None), true),
        (retype("price", "numeric", &[12, 2], None), false),
        (retype("price", "numeric", &[12, 3], None), true),
        (retype("price", "numeric", &[8, 2], None), true),
        (retype("price", "numeric", &[], None), false),
        (retype("n", "int8", &[], None), true),
        (retype("body", "varchar", &[40], Some("body")), true),
        (retype("ghost", "text", &[], None), true),
    ] {
        assert_eq!(rewrites(op.clone(), &c), want, "{op:?}");
    }
    assert!(rewrites(retype("body", "text", &[], None), &RenderContext::default()), "an unknown table rewrites");
}

#[test]
fn the_dry_run_skips_big_rewrites_and_everything_after() {
    let ops = [
        Operation::SetTableComment { table: notes(), comment: Some("x".into()) },
        retype("n", "int8", &[], None),
        Operation::RenameColumn { table: notes(), column: "note".into(), to: "memo".into() },
    ];
    let small = ctx(1000);
    let stmts = render(&ops, &small).unwrap();
    assert_eq!(dry_run_plan(&stmts, &small, 268_435_456), vec![None, None, None]);
    let big = ctx(300_000_000);
    let stmts = render(&ops, &big).unwrap();
    assert_eq!(
        dry_run_plan(&stmts, &big, 268_435_456),
        vec![None, Some(SkipReason::LargeTableRewrite), Some(SkipReason::AfterSkipped)]
    );
    let mut ix = index(Some("i"), vec![key("a")]);
    ix.concurrently = true;
    let stmts = render(&[Operation::CreateIndex(ix)], &RenderContext::default()).unwrap();
    assert_eq!(dry_run_plan(&stmts, &RenderContext::default(), 268_435_456), vec![Some(SkipReason::Concurrently)]);
}

#[test]
fn context_tables_and_lock_sql() {
    let ops = [
        retype("body", "text", &[], None),
        Operation::AddColumn { table: notes(), column: ColumnSpec { name: "x".into(), data_type: TypeSpec { schema: "pg_catalog".into(), name: "int4".into(), modifiers: vec![], array: false }, not_null: false, default: None, identity: None, generated: None, collation: None, comment: None } },
        Operation::DropTable { table: t("app", "Old") },
        Operation::DropColumn { table: notes(), column: "y".into() },
        Operation::CreateEnum { schema: "public".into(), name: "e".into(), values: vec![] },
    ];
    assert_eq!(context_tables(&ops), vec![notes(), t("app", "Old")]);
    assert_eq!(lock_sql(&ops), vec!["LOCK TABLE app.\"Old\" IN ACCESS EXCLUSIVE MODE".to_string(), "LOCK TABLE public.notes IN ACCESS EXCLUSIVE MODE".to_string()]);
    assert!(dependents_sql(&ops[..2]).is_none(), "no drop, no dependents query");
    assert!(dependents_sql(&ops).is_some());
}

#[test]
fn the_dependents_hash_is_order_free_and_sensitive() {
    let d = |k: &str, n: &str| Dependent { kind: k.into(), schema: Some("public".into()), name: n.into(), parent: None, description: format!("{k} {n}") };
    let a = dependents_hash(&[d("view", "v"), d("table constraint", "fk")]);
    let b = dependents_hash(&[d("table constraint", "fk"), d("view", "v")]);
    assert_eq!(a, b);
    assert_eq!(a.len(), 64);
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(a, dependents_hash(&[d("view", "v")]));
    assert_ne!(dependents_hash(&[]), a);
    let parsed = parse_dependents(r#"[{"kind":"view","schema":"public","name":"v","parent":null,"description":"view v"},{"kind":"table constraint","schema":"public","name":"fk","parent":"other","description":"constraint fk on table other"}]"#).unwrap();
    assert_eq!(parsed[0].kind, "table constraint", "sorted");
}

// --- Hostile names: every name reaches Postgres as exactly the name given. ---

fn name() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-zA-Z0-9 _\"'$.;()\\-é中ß]{1,24}").unwrap().prop_filter("63 bytes", |s| s.len() <= 63)
}

fn parse_one(sql: &str) -> pg_query::NodeEnum {
    let parsed = pg_query::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(parsed.protobuf.stmts.len(), 1, "{sql}");
    parsed.protobuf.stmts[0].stmt.as_ref().unwrap().node.clone().unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn create_table_names_survive(schema in name(), table in name(), column in name()) {
        let c = CreateTable {
            table: t(&schema, &table),
            columns: vec![ColumnSpec { name: column.clone(), data_type: TypeSpec { schema: schema.clone(), name: table.clone(), modifiers: vec![], array: true }, not_null: false, default: None, identity: None, generated: None, collation: None, comment: None }],
            primary_key: vec![], constraints: vec![], rls: None, policies: vec![], grants: vec![], comment: None, owner: None,
        };
        let out = render(&[Operation::CreateTable(c)], &RenderContext::default()).unwrap();
        let pg_query::NodeEnum::CreateStmt(stmt) = parse_one(&out[0].sql) else { panic!("{}", out[0].sql) };
        let rel = stmt.relation.as_ref().unwrap();
        prop_assert_eq!(&rel.schemaname, &schema);
        prop_assert_eq!(&rel.relname, &table);
        let Some(pg_query::NodeEnum::ColumnDef(def)) = stmt.table_elts[0].node.as_ref() else { panic!() };
        prop_assert_eq!(&def.colname, &column);
    }

    #[test]
    fn rename_and_policy_names_survive(table in name(), column in name(), to in name(), role in name()) {
        let out = render(&[
            Operation::RenameColumn { table: t("public", &table), column: column.clone(), to: to.clone() },
            Operation::CreatePolicy { table: t("public", &table), policy: PolicySpec { name: to.clone(), command: PolicyCommandSpec::All, permissive: true, roles: vec![role.clone()], using: Some("true".into()), with_check: None } },
            Operation::Grant { table: t("public", &table), grant: TableGrant { columns: vec![column.clone()], privileges: vec![Privilege::Select], grantee: Grantee::Role(role.clone()), with_grant_option: false } },
        ], &RenderContext::default()).unwrap();
        let pg_query::NodeEnum::RenameStmt(r) = parse_one(&out[0].sql) else { panic!("{}", out[0].sql) };
        prop_assert_eq!(&r.subname, &column);
        prop_assert_eq!(&r.newname, &to);
        let pg_query::NodeEnum::CreatePolicyStmt(p) = parse_one(&out[1].sql) else { panic!("{}", out[1].sql) };
        prop_assert_eq!(&p.policy_name, &to);
        let Some(pg_query::NodeEnum::RoleSpec(spec)) = p.roles[0].node.as_ref() else { panic!() };
        prop_assert_eq!(&spec.rolename, &role);
        let pg_query::NodeEnum::GrantStmt(g) = parse_one(&out[2].sql) else { panic!("{}", out[2].sql) };
        let Some(pg_query::NodeEnum::RoleSpec(grantee)) = g.grantees[0].node.as_ref() else { panic!() };
        prop_assert_eq!(&grantee.rolename, &role);
    }

    #[test]
    fn enum_labels_survive(schema in name(), label in name()) {
        let out = render(&[Operation::CreateEnum { schema: schema.clone(), name: "e".into(), values: vec![label.clone()] }], &RenderContext::default()).unwrap();
        let pg_query::NodeEnum::CreateEnumStmt(e) = parse_one(&out[0].sql) else { panic!("{}", out[0].sql) };
        let Some(pg_query::NodeEnum::String(s)) = e.vals[0].node.as_ref() else { panic!() };
        prop_assert_eq!(&s.sval, &label);
    }
}
