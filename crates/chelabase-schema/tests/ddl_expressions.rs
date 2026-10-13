//! End-of-step review, finding 3: a caller expression (a default, a check, a `USING` cast, an
//! index predicate or key expression, a policy clause, a generated column) is exactly one SQL
//! expression; text that would add clauses to the statement around it is refused.
use chelabase_schema::ddl::{DdlErrorKind, RenderContext, render};
use chelabase_types::table_api::*;

fn notes() -> TableRef {
    TableRef { schema: "public".into(), name: "notes".into() }
}
fn ty(name: &str) -> TypeSpec {
    TypeSpec { schema: "pg_catalog".into(), name: name.into(), modifiers: vec![], array: false }
}
fn col(default: Option<&str>, generated: Option<&str>) -> ColumnSpec {
    ColumnSpec {
        name: "x".into(),
        data_type: ty("int4"),
        not_null: false,
        default: default.map(Into::into),
        identity: None,
        generated: generated.map(Into::into),
        collation: None,
        comment: None,
    }
}

/// Every slot where the caller's expression text goes, filled with `e`.
fn slots(e: &str) -> Vec<Operation> {
    let policy = |using: Option<&str>, with_check: Option<&str>| PolicySpec {
        name: "p".into(),
        command: PolicyCommandSpec::All,
        permissive: true,
        roles: vec![],
        using: using.map(Into::into),
        with_check: with_check.map(Into::into),
    };
    let index = |keys: Vec<IndexKey>, predicate: Option<&str>| IndexSpec {
        table: notes(),
        name: Some("i".into()),
        method: None,
        unique: false,
        nulls_not_distinct: false,
        keys,
        include: vec![],
        predicate: predicate.map(Into::into),
        concurrently: false,
    };
    vec![
        Operation::AddColumn { table: notes(), column: col(Some(e), None) },
        Operation::AddColumn { table: notes(), column: col(None, Some(e)) },
        Operation::AlterColumn {
            table: notes(),
            column: "x".into(),
            change: ColumnChange { data_type: Some(ty("int8")), using: Some(e.into()), ..Default::default() },
        },
        Operation::AlterColumn {
            table: notes(),
            column: "x".into(),
            change: ColumnChange { default: Some(DefaultChange::Set(e.into())), ..Default::default() },
        },
        Operation::AddConstraint {
            table: notes(),
            constraint: ConstraintSpec { name: None, kind: ConstraintKindSpec::Check { expression: e.into() }, not_valid: false },
        },
        Operation::CreateIndex(index(vec![IndexKey::Column { name: "x".into(), descending: false, nulls: None }], Some(e))),
        Operation::CreateIndex(index(vec![IndexKey::Expression { sql: e.into(), descending: false, nulls: None }], None)),
        Operation::CreatePolicy { table: notes(), policy: policy(Some(e), None) },
        Operation::CreatePolicy { table: notes(), policy: policy(None, Some(e)) },
        Operation::AlterPolicy { table: notes(), name: "p".into(), roles: None, using: Some(e.into()), with_check: None },
    ]
}

#[test]
fn smuggled_clauses_are_refused_in_every_slot() {
    for bad in [
        "c::int, DROP COLUMN d CASCADE",
        "1, ADD COLUMN y int",
        "true) WITH CHECK (false",
        "x) WHERE (true",
        "1; DROP TABLE public.notes",
        "1 -- the rest",
        "(SELECT 1); SELECT 2",
        "",
        "   ",
    ] {
        for (i, op) in slots(bad).into_iter().enumerate() {
            let e = render(&[op], &RenderContext::default()).unwrap_err();
            assert_eq!(e.kind, DdlErrorKind::InvalidExpression(bad.to_string()), "slot {i}: {bad:?}");
        }
    }
}

#[test]
fn ordinary_expressions_still_render() {
    for good in [
        "0",
        "now()",
        "lower(body)",
        "user_id = auth.uid()",
        "(a > 0) AND (b < 10)",
        "x::text",
        "CASE WHEN x > 0 THEN 'pos' ELSE 'neg' END",
        "x IN (1, 2, 3)",
        "EXISTS (SELECT 1 FROM public.users u WHERE u.id = x)",
        "'it''s; fine -- really'",
    ] {
        for (i, op) in slots(good).into_iter().enumerate() {
            assert!(render(&[op], &RenderContext::default()).is_ok(), "slot {i}: {good:?}");
        }
    }
}
