//! The editor and the merge planner render the same object the same way.
use super::*;
use crate::plan::ddl as merge;
use crate::testkit::*;
use chelabase_types::table_api::*;

fn notes() -> TableRef {
    TableRef { schema: "public".into(), name: "notes".into() }
}

#[test]
fn a_column_renders_as_the_merge_planner_renders_it() {
    let spec = ColumnSpec {
        name: "Body".into(),
        data_type: TypeSpec { schema: "pg_catalog".into(), name: "text".into(), modifiers: vec![], array: false },
        not_null: true,
        default: Some("''".into()),
        identity: None,
        generated: None,
        collation: Some(CollationRef { schema: "pg_catalog".into(), name: "C".into() }),
        comment: None,
    };
    let mut model = col(2, "Body", "pg_catalog.text");
    model.collation = Some("pg_catalog.\"C\"".into());
    model.default = Some("''".into());
    model.not_null = true;
    let out = render(&[Operation::AddColumn { table: notes(), column: spec }], &RenderContext::default()).unwrap();
    assert_eq!(out[0].sql, format!("ALTER TABLE public.notes ADD COLUMN {}", merge::column_def(&model, "public", true)));
}

#[test]
fn a_table_comment_renders_as_the_merge_planner_renders_it() {
    let out = render(
        &[Operation::SetTableComment { table: notes(), comment: Some("a 'b' \\c".into()) }],
        &RenderContext::default(),
    )
    .unwrap();
    assert_eq!(out[0].sql, merge::comment("TABLE", "public.notes", &Some("a 'b' \\c".into())));
}

#[test]
fn indexes_policies_and_grants_render_as_the_merge_planner_renders_them() {
    let mut spec = IndexSpec {
        table: notes(), name: Some("notes_body_idx".into()), method: None, unique: true, nulls_not_distinct: false,
        keys: vec![IndexKey::Column { name: "body".into(), descending: false, nulls: None }],
        include: vec!["id".into()], predicate: Some("body <> ''".into()), concurrently: false,
    };
    let mut model = index(30, "notes_body_idx", &["body"]);
    model.unique = true;
    model.include = vec!["id".into()];
    model.predicate = Some("body <> ''".into());
    let out = render(&[Operation::CreateIndex(spec.clone())], &RenderContext::default()).unwrap();
    assert_eq!(out[0].sql, merge::create_index(&model, "notes_body_idx", "public.notes", false));
    spec.concurrently = true;
    let out = render(&[Operation::CreateIndex(spec)], &RenderContext::default()).unwrap();
    assert_eq!(out[0].sql, merge::create_index(&model, "notes_body_idx", "public.notes", true));

    let mut p = policy(40, "own", "user_id = 1");
    p.roles = vec![rref(50, "Billing")];
    let spec = PolicySpec { name: "own".into(), command: PolicyCommandSpec::All, permissive: true, roles: vec!["Billing".into()], using: Some("user_id = 1".into()), with_check: None };
    let out = render(&[Operation::CreatePolicy { table: notes(), policy: spec }], &RenderContext::default()).unwrap();
    assert_eq!(out[0].sql, merge::create_policy(&p, "public.notes"));

    let g = grant(rref(51, "Editors"), "UPDATE");
    let tg = TableGrant { columns: vec!["body".into()], privileges: vec![Privilege::Update], grantee: Grantee::Role("Editors".into()), with_grant_option: false };
    let out = render(&[Operation::Grant { table: notes(), grant: tg.clone() }, Operation::Revoke { table: notes(), grant: tg }], &RenderContext::default()).unwrap();
    assert_eq!(out[0].sql, merge::grant_sql(&g, "public.notes", Some("body")).unwrap());
    assert_eq!(out[1].sql, merge::revoke_sql(&g, "public.notes", Some("body"), false).unwrap());
}

// T6 review, minor: a table the request names that the catalog doesn't have is found before
// anything runs, unless an earlier operation of the request makes it.
#[test]
fn the_first_missing_table_skips_tables_the_request_makes() {
    let ops: Vec<Operation> = serde_json::from_value(serde_json::json!([
        { "create_table": { "table": { "schema": "public", "name": "memos" }, "columns": [] } },
        { "set_table_comment": { "table": { "schema": "public", "name": "memos" }, "comment": "c" } },
        { "rename_table": { "table": { "schema": "public", "name": "notes" }, "to": "n2" } },
        { "set_table_comment": { "table": { "schema": "public", "name": "n2" }, "comment": "c" } },
        { "set_table_schema": { "table": { "schema": "public", "name": "n2" }, "schema": "app" } },
        { "set_table_comment": { "table": { "schema": "app", "name": "n2" }, "comment": "c" } },
        { "set_table_comment": { "table": { "schema": "public", "name": "ghost" }, "comment": "c" } },
        { "set_table_comment": { "table": { "schema": "public", "name": "ghost2" }, "comment": "c" } },
    ]))
    .unwrap();
    let t = |s: &str, n: &str| TableRef { schema: s.into(), name: n.into() };
    let absent = [t("public", "memos"), t("public", "n2"), t("app", "n2"), t("public", "ghost"), t("public", "ghost2")];
    assert_eq!(first_missing_table(&ops, &absent), Some((6, &t("public", "ghost"))));
    assert_eq!(first_missing_table(&ops[..6], &absent), None);
    // A concurrent index's table counts too.
    let index: Vec<Operation> = serde_json::from_value(serde_json::json!([
        { "create_index": { "table": { "schema": "public", "name": "ghost" }, "name": "g", "keys": [{ "column": { "name": "x" } }], "concurrently": true } },
    ]))
    .unwrap();
    assert_eq!(first_missing_table(&index, &absent).map(|(i, _)| i), Some(0));
}

// End-of-step review, finding 3: the create-table slots (column default and generated, check,
// exclusion predicate, policy clauses) and an added exclusion's predicate are single
// expressions too.
#[test]
fn create_table_and_exclusion_slots_take_one_expression() {
    let bad = "true) WITH CHECK (false";
    let create = |slot: &str, e: &str| -> Operation {
        let mut v = serde_json::json!({ "create_table": {
            "table": { "schema": "public", "name": "memos" },
            "columns": [{ "name": "x", "data_type": { "schema": "pg_catalog", "name": "int4" } }],
        } });
        let t = &mut v["create_table"];
        match slot {
            "default" | "generated" => t["columns"][0][slot] = e.into(),
            "check" => t["constraints"] = serde_json::json!([{ "kind": { "check": { "expression": e } } }]),
            "exclusion" => {
                t["constraints"] = serde_json::json!([{ "kind": { "exclusion": {
                    "method": "gist",
                    "elements": [{ "column": "x", "operator": { "schema": "pg_catalog", "name": "=" } }],
                    "predicate": e,
                } } }])
            }
            "using" | "with_check" => t["policies"] = serde_json::json!([{ "name": "p", slot: e }]),
            _ => unreachable!(),
        }
        serde_json::from_value(v).unwrap()
    };
    for slot in ["default", "generated", "check", "exclusion", "using", "with_check"] {
        let e = render(&[create(slot, bad)], &RenderContext::default()).unwrap_err();
        assert_eq!(e.kind, DdlErrorKind::InvalidExpression(bad.into()), "{slot}");
        assert!(render(&[create(slot, "x > 0")], &RenderContext::default()).is_ok(), "{slot}");
    }
    let Operation::CreateTable(c) = create("exclusion", bad) else { unreachable!() };
    let added = Operation::AddConstraint { table: notes(), constraint: c.constraints[0].clone() };
    let e = render(&[added], &RenderContext::default()).unwrap_err();
    assert_eq!(e.kind, DdlErrorKind::InvalidExpression(bad.into()));
}
