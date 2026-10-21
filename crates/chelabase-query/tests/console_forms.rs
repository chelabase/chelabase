//! RT1 T4: the console-only query forms. `{"text_of": col, "as": alias}` as a select item and
//! `"text": true` beside `eq`/`in` in a filter compare and read a column by its `::text`
//! (S9, S10, S11; W5, R18). The data API refuses both like any unknown key: the parse
//! mode comes from the route, never from the body.

use chelabase_query::query::{
    CondOp, Count, Delete, Filter, Name, Op, ParseMode, Select, SelectItem, Update, parse,
    parse_with,
};
use chelabase_query::shape_hash;
use chelabase_query::sql::{
    BindValue, Catalog, ColumnInfo, FnLookupError, FunctionInfo, ParamType, Plan, PlanError,
    TableInfo, plan, resolve_bind,
};
use serde_json::{Value, json};

// ---- parsing

fn console(json: &str) -> Op {
    parse_with(json.as_bytes(), ParseMode::Console)
        .unwrap_or_else(|e| panic!("{json} should parse in console mode: {e}"))
        .op
}

/// The message of the `invalid_query` `json` gets in `mode`.
fn refused(json: &str, mode: ParseMode) -> String {
    match parse_with(json.as_bytes(), mode) {
        Ok(p) => panic!("{json} should be refused in {mode:?}, parsed as {:?}", p.op),
        Err(e) => {
            assert_eq!(e.code(), "invalid_query", "{json}");
            e.to_string()
        }
    }
}

fn select_tree(item: &str) -> String {
    format!(r#"{{"v":1,"op":"select","from":"t","select":[{item}]}}"#)
}

fn where_tree(filter: &str) -> String {
    format!(r#"{{"v":1,"op":"select","from":"t","where":{filter}}}"#)
}

fn only_select(op: Op) -> Select {
    match op {
        Op::Select(s) => s,
        other => panic!("not a select: {other:?}"),
    }
}

#[test]
fn console_parses_text_of_with_its_alias() {
    let s = only_select(console(&select_tree(
        r#"{"text_of":"id","as":"id_text"}, "name", {"col":"n","as":"m"}"#,
    )));
    assert_eq!(
        s.select,
        Some(vec![
            SelectItem::TextOf {
                col: "id".into(),
                alias: "id_text".into()
            },
            SelectItem::Col("name".into()),
            SelectItem::Aliased {
                col: "n".into(),
                alias: "m".into()
            },
        ])
    );
}

#[test]
fn console_parses_text_filters() {
    let s = only_select(console(&where_tree(
        r#"{"and":[{"col":"id","eq":"1.50","text":true},{"not":{"col":"k","in":["a","b"],"text":true}},{"col":"n","eq":1}]}"#,
    )));
    let Some(Filter::And(parts)) = s.filter else {
        panic!("not an and");
    };
    assert_eq!(
        parts[0],
        Filter::Cond {
            col: "id".into(),
            op: CondOp::Eq,
            value: json!("1.50"),
            text: true,
            parse: false,
        }
    );
    let Filter::Not(inner) = &parts[1] else {
        panic!("not a not");
    };
    assert_eq!(
        **inner,
        Filter::Cond {
            col: "k".into(),
            op: CondOp::In,
            value: json!(["a", "b"]),
            text: true,
            parse: false,
        }
    );
    // A plain filter is untouched by the mode.
    assert_eq!(
        parts[2],
        Filter::Cond {
            col: "n".into(),
            op: CondOp::Eq,
            value: json!(1),
            text: false,
            parse: false,
        }
    );
}

#[test]
fn console_accepts_the_forms_in_writes_counts_and_batches() {
    let text = r#"{"col":"id","eq":"7","text":true}"#;
    for tree in [
        format!(r#"{{"v":1,"op":"update","table":"t","set":{{"a":1}},"where":{text}}}"#),
        format!(r#"{{"v":1,"op":"delete","from":"t","where":{text}}}"#),
        format!(r#"{{"v":1,"op":"count","from":"t","where":{text}}}"#),
        format!(
            r#"{{"v":1,"op":"batch","ops":[{{"op":"count","from":"t"}},{{"op":"select","from":"t","where":{text},"select":[{{"text_of":"id","as":"x"}}]}}]}}"#
        ),
    ] {
        console(&tree);
    }
    let Op::Update(Update { filter, .. }) = console(&format!(
        r#"{{"v":1,"op":"update","table":"t","set":{{"a":1}},"where":{text}}}"#
    )) else {
        panic!("not an update");
    };
    assert!(matches!(filter, Some(Filter::Cond { text: true, .. })));
}

#[test]
fn the_data_api_refuses_the_forms_like_any_unknown_key() {
    // Each form next to the same shape with a made-up key: identical error, key name aside.
    type Shape = (&'static str, fn(&str) -> String);
    let shapes: [Shape; 4] = [
        ("text_of", |k| {
            select_tree(&format!(r#"{{"{k}":"id","as":"x"}}"#))
        }),
        ("text", |k| {
            where_tree(&format!(r#"{{"col":"id","eq":"1","{k}":true}}"#))
        }),
        ("text", |k| {
            where_tree(&format!(r#"{{"col":"id","in":["1"],"{k}":true}}"#))
        }),
        ("text", |k| {
            format!(
                r#"{{"v":1,"op":"batch","ops":[{{"op":"select","from":"t"}},{{"op":"delete","from":"t","where":{{"col":"id","eq":"1","{k}":true}}}}]}}"#
            )
        }),
    ];
    for (key, build) in shapes {
        let form = build(key);
        // The console takes the very same text.
        parse_with(form.as_bytes(), ParseMode::Console)
            .unwrap_or_else(|e| panic!("{form} should parse in console mode: {e}"));
        let unknown = build("zzz");
        let expected = refused(&unknown, ParseMode::DataApi);
        for (what, got) in [
            ("parse_with(DataApi)", refused(&form, ParseMode::DataApi)),
            (
                "parse",
                match parse(form.as_bytes()) {
                    Ok(p) => panic!("{form} parsed by `parse`: {:?}", p.op),
                    Err(e) => {
                        assert_eq!(e.code(), "invalid_query");
                        e.to_string()
                    }
                },
            ),
        ] {
            assert_eq!(got.replace(key, "zzz"), expected, "{what}: {form}");
        }
    }
}

#[test]
fn parse_is_the_data_api_mode() {
    for body in [
        r#"{"v":1,"op":"select","from":"t"}"#,
        r#"{"v":1,"database":"d","op":"count","from":"t","where":{"col":"a","gt":3}}"#,
        r#"{"v":2,"op":"select","from":"t"}"#,
        r#"{"v":1,"op":"select","from":"t","select":[{"text_of":"a","as":"b"}]}"#,
        "nope",
    ] {
        assert_eq!(
            parse(body.as_bytes()),
            parse_with(body.as_bytes(), ParseMode::DataApi),
            "{body}"
        );
    }
}

#[test]
fn the_mode_comes_from_the_route_never_from_the_body() {
    let item = r#"{"text_of":"id","as":"x"}"#;
    console(&select_tree(item));
    // A body that asks for the console is refused as having an unknown field, and the form
    // inside it stays refused: no field switches the data API's grammar.
    for field in [
        r#""mode":"console""#,
        r#""console":true"#,
        r#""parse_mode":"Console""#,
        r#""ParseMode":"Console""#,
    ] {
        let body = format!(r#"{{"v":1,"op":"select","from":"t",{field},"select":[{item}]}}"#);
        let msg = refused(&body, ParseMode::DataApi);
        assert!(msg.contains("unknown field"), "{field}: {msg}");
        // The same fields are no more welcome in the console: it is not switchable either.
        let msg = refused(&body, ParseMode::Console);
        assert!(msg.contains("unknown field"), "{field}: {msg}");
        // Nor inside the select item or the filter.
        refused(
            &select_tree(&format!(r#"{{"text_of":"id","as":"x",{field}}}"#)),
            ParseMode::DataApi,
        );
        refused(
            &where_tree(&format!(r#"{{"col":"id","eq":"1","text":true,{field}}}"#)),
            ParseMode::Console,
        );
    }
    // Without the extra field the data API still refuses what the console takes.
    refused(&select_tree(item), ParseMode::DataApi);
    console(&select_tree(item));
}

#[test]
fn text_beside_the_wrong_operator_or_value_is_refused() {
    // The forms it is refused beside are the ones that do work.
    console(&where_tree(r#"{"col":"id","eq":"1","text":true}"#));
    console(&where_tree(r#"{"col":"id","in":["1"],"text":true}"#));
    // `text` goes with `eq` and `in` only.
    for filter in [
        r#"{"col":"id","neq":"1","text":true}"#,
        r#"{"col":"id","lt":"1","text":true}"#,
        r#"{"col":"id","lte":"1","text":true}"#,
        r#"{"col":"id","gt":"1","text":true}"#,
        r#"{"col":"id","gte":"1","text":true}"#,
        r#"{"col":"id","like":"1%","text":true}"#,
        r#"{"col":"id","ilike":"1%","text":true}"#,
        r#"{"col":"id","is":null,"text":true}"#,
        r#"{"col":"id","is":true,"text":true}"#,
        // no operator at all
        r#"{"col":"id","text":true}"#,
        // eq / in with values that aren't strings
        r#"{"col":"id","eq":1,"text":true}"#,
        r#"{"col":"id","eq":1.5,"text":true}"#,
        r#"{"col":"id","eq":true,"text":true}"#,
        r#"{"col":"id","eq":null,"text":true}"#,
        r#"{"col":"id","eq":["a"],"text":true}"#,
        r#"{"col":"id","eq":{"a":"b"},"text":true}"#,
        r#"{"col":"id","in":[1],"text":true}"#,
        r#"{"col":"id","in":["a",1],"text":true}"#,
        r#"{"col":"id","in":[null],"text":true}"#,
        r#"{"col":"id","in":[true],"text":true}"#,
        r#"{"col":"id","in":[["a"]],"text":true}"#,
        r#"{"col":"id","in":"a","text":true}"#,
        // `text` itself is a bool
        r#"{"col":"id","eq":"1","text":"true"}"#,
        r#"{"col":"id","eq":"1","text":1}"#,
        r#"{"col":"id","eq":"1","text":null}"#,
        // two operators are still two operators
        r#"{"col":"id","eq":"1","in":["1"],"text":true}"#,
    ] {
        refused(&where_tree(filter), ParseMode::Console);
    }
}

#[test]
fn text_of_needs_a_string_column_an_alias_and_nothing_else() {
    let long_ok = "a".repeat(63);
    let long_bad = "a".repeat(64);
    console(&select_tree(&format!(
        r#"{{"text_of":"id","as":"{long_ok}"}}"#
    )));
    for item in [
        r#"{"text_of":"id"}"#.to_owned(),
        r#"{"text_of":"id","as":""}"#.to_owned(),
        format!(r#"{{"text_of":"id","as":"{long_bad}"}}"#),
        r#"{"text_of":"id","as":"a\u0000b"}"#.to_owned(),
        r#"{"text_of":"id","as":1}"#.to_owned(),
        r#"{"text_of":"id","as":null}"#.to_owned(),
        r#"{"text_of":1,"as":"x"}"#.to_owned(),
        r#"{"text_of":null,"as":"x"}"#.to_owned(),
        r#"{"text_of":["id"],"as":"x"}"#.to_owned(),
        r#"{"text_of":{"col":"id"},"as":"x"}"#.to_owned(),
        r#"{"text_of":"id","col":"id","as":"x"}"#.to_owned(),
        r#"{"text_of":"id","as":"x","zzz":1}"#.to_owned(),
        r#"{"as":"x"}"#.to_owned(),
    ] {
        refused(&select_tree(&item), ParseMode::Console);
    }
    console(&select_tree(&format!(
        r#"{{"text_of":"id","as":"{long_ok}"}}"#
    )));
    // The Unicode alias is measured in bytes, as for any alias: 32 two-byte chars are 64.
    let wide = "é".repeat(32);
    refused(
        &select_tree(&format!(r#"{{"text_of":"id","as":"{wide}"}}"#)),
        ParseMode::Console,
    );
}

#[test]
fn console_mode_leaves_the_data_api_grammar_alone() {
    for body in [
        r#"{"v":2,"op":"select","from":"t"}"#,
        r#"{"v":1,"op":"select","from":"t","select":[]}"#,
        r#"{"v":1,"op":"select","from":"t","where":{"col":"a","equals":1}}"#,
        r#"{"v":1,"op":"select","from":"t","where":{"col":"a","eq":1,"neq":2}}"#,
        r#"{"v":1,"op":"delete","from":"t"}"#,
    ] {
        assert_eq!(
            refused(body, ParseMode::Console),
            refused(body, ParseMode::DataApi),
            "{body}"
        );
    }
    for body in [
        r#"{"v":1,"op":"select","from":"t","select":["a",{"col":"b","as":"c"}],"where":{"col":"a","like":"x%"}}"#,
        r#"{"v":1,"op":"select","from":"t","where":{"col":"a","in":[1,2]}}"#,
    ] {
        assert_eq!(
            parse_with(body.as_bytes(), ParseMode::Console),
            parse_with(body.as_bytes(), ParseMode::DataApi),
            "{body}"
        );
    }
}

// ---- planning

struct Odd {
    tables: Vec<TableInfo>,
}

fn col(name: &str, type_sql: &str) -> ColumnInfo {
    ColumnInfo {
        name: name.into(),
        type_sql: type_sql.into(),
        type_qualified: String::new(),
        array: false,
        text_like: false,
        generated: false,
    }
}

impl Odd {
    fn new() -> Self {
        Odd {
            tables: vec![TableInfo {
                schema: "public".into(),
                name: "t".into(),
                columns: vec![
                    col("id", "bigint"),
                    col("we\"ird", "text"),
                    col("x y", "numeric(10,2)"),
                    col("\u{dc}n\u{ef}", "timestamp with time zone"),
                    col("select", "text"),
                    col("meta", "jsonb"),
                    ColumnInfo {
                        array: true,
                        ..col("tags", "text[]")
                    },
                ],
            }],
        }
    }
}

impl Catalog for Odd {
    fn table(&self, name: &Name) -> Option<&TableInfo> {
        self.tables
            .iter()
            .find(|t| Some(&t.schema) == name.schema.as_ref() && t.name == name.name)
    }

    fn function(&self, _: &Name, _: &[&str]) -> Result<&FunctionInfo, FnLookupError> {
        Err(FnLookupError::NotFound)
    }
}

fn name() -> Name {
    Name {
        schema: None,
        name: "t".into(),
    }
}

fn cond(col: &str, op: CondOp, value: Value, text: bool) -> Filter {
    Filter::Cond {
        col: col.into(),
        op,
        value,
        text,
        parse: false,
    }
}

fn text_eq(col: &str, v: &str) -> Filter {
    cond(col, CondOp::Eq, json!(v), true)
}

fn select(items: Option<Vec<SelectItem>>, filter: Option<Filter>) -> Op {
    Op::Select(Select {
        from: name(),
        select: items,
        filter,
        order_by: vec![],
        limit: None,
        offset: None,
    })
}

fn text_of(col: &str, alias: &str) -> SelectItem {
    SelectItem::TextOf {
        col: col.into(),
        alias: alias.into(),
    }
}

fn planned(op: &Op) -> Plan {
    let mut plans = plan(op, &Odd::new(), "public").unwrap_or_else(|e| panic!("{op:?}: {e}"));
    assert_eq!(plans.len(), 1);
    plans.remove(0)
}

fn plan_error(op: &Op) -> PlanError {
    plan(op, &Odd::new(), "public").expect_err("should not plan")
}

/// The text bound for `op`'s parameters, in order, for the ones that bind text.
fn bound_texts(op: &Op, p: &Plan) -> Vec<Option<String>> {
    p.binds
        .iter()
        .filter_map(|b| match resolve_bind(op, b, 1000)? {
            BindValue::Text(t) => Some(t.map(|c| c.into_owned())),
            _ => None,
        })
        .collect()
}

#[test]
fn text_of_selects_the_column_cast_to_text() {
    let op = select(
        Some(vec![
            text_of("id", "id_text"),
            text_of("x y", "xy"),
            text_of("we\"ird", "a\"b"),
            text_of("\u{dc}n\u{ef}", "u"),
            SelectItem::Col("select".into()),
        ]),
        None,
    );
    let sql = planned(&op).sql;
    for expect in [
        r#""id"::text AS "id_text""#,
        r#""x y"::text AS "xy""#,
        r#""we""ird"::text AS "a""b""#,
        "\"\u{dc}n\u{ef}\"::text AS \"u\"",
        r#", "select" FROM "public"."t""#,
    ] {
        assert!(sql.contains(expect), "{expect} not in {sql}");
    }
    // No filter, no extra bind: limit and offset only.
    assert_eq!(planned(&op).binds.len(), 2);
}

#[test]
fn text_of_names_are_unique_with_the_other_items() {
    use SelectItem::{Aliased, Col};
    let aliased = |c: &str, a: &str| Aliased {
        col: c.into(),
        alias: a.into(),
    };
    // Two items with one output name: the same refusal as for aliases.
    for items in [
        vec![text_of("id", "a"), text_of("select", "a")],
        vec![text_of("id", "a"), aliased("select", "a")],
        vec![aliased("select", "a"), text_of("id", "a")],
        vec![Col("id".into()), text_of("select", "id")],
        vec![text_of("select", "id"), Col("id".into())],
        vec![text_of("id", "id"), Col("id".into())],
        vec![text_of("id", "x"), text_of("id", "x")],
    ] {
        let err = plan_error(&select(Some(items.clone()), None));
        assert_eq!(err.code(), "invalid_query", "{items:?}");
        assert!(err.to_string().contains("twice"), "{items:?}: {err}");
    }
    // The same column twice under different names, or named like itself, is fine.
    let sql = planned(&select(
        Some(vec![text_of("id", "a"), text_of("id", "b")]),
        None,
    ))
    .sql;
    assert!(
        sql.contains(r#""id"::text AS "a", "id"::text AS "b""#),
        "{sql}"
    );
    for items in [
        vec![text_of("id", "a"), text_of("id", "b")],
        vec![text_of("id", "id")],
        vec![Col("id".into()), text_of("id", "id_text")],
    ] {
        planned(&select(Some(items), None));
    }
    // Columns come from the catalog.
    let err = plan_error(&select(Some(vec![text_of("nope", "a")]), None));
    assert_eq!(err.code(), "unknown_column");
}

/// The comparison, as the SQL text starts it (`<col>::text COLLATE "C" = `).
fn after(sql: &str, head: &str) -> String {
    let at = sql
        .find(head)
        .unwrap_or_else(|| panic!("{head} not in {sql}"));
    sql[at + head.len()..].to_owned()
}

#[test]
fn text_eq_compares_the_text_under_collate_c_with_a_bound_value() {
    let tricky = "O'Brien \"x\" \\ 1.50; DROP TABLE t";
    let op = select(
        None,
        Some(Filter::And(vec![
            text_eq("id", "9007199254740993"),
            Filter::Not(Box::new(text_eq("we\"ird", tricky))),
            cond("x y", CondOp::Eq, json!("1.50"), true),
        ])),
    );
    let p = planned(&op);
    assert!(
        !p.sql.contains("O'Brien") && !p.sql.contains("9007199254740993"),
        "{}",
        p.sql
    );
    assert!(!p.capped_sql.contains("O'Brien"), "{}", p.capped_sql);
    for head in [
        r#""id"::text COLLATE "C" = "#,
        r#""we""ird"::text COLLATE "C" = "#,
        r#""x y"::text COLLATE "C" = "#,
    ] {
        let rest = after(&p.sql, head);
        // The value is a parameter, typed text (the cast to text is harmless).
        assert!(
            rest.starts_with('$') || rest.starts_with("CAST($"),
            "{head}{rest}"
        );
    }
    // No cast of the value to the column's type: the column goes to text, not the value.
    assert!(
        !p.sql.contains("AS bigint") && !p.sql.contains("AS numeric"),
        "{}",
        p.sql
    );
    assert_eq!(
        bound_texts(&op, &p),
        vec![
            Some("9007199254740993".to_owned()),
            Some(tricky.to_owned()),
            Some("1.50".to_owned())
        ]
    );
    assert!(p.param_types()[..3].iter().all(|t| *t == ParamType::Text));
}

#[test]
fn text_in_compares_against_a_text_array_under_collate_c() {
    let op = select(
        None,
        Some(cond(
            "\u{dc}n\u{ef}",
            CondOp::In,
            json!(["2026-10-16 12:00:00+00", "it's"]),
            true,
        )),
    );
    let p = planned(&op);
    let sql = &p.sql;
    let rest = after(sql, "\"\u{dc}n\u{ef}\"::text COLLATE \"C\" = ANY");
    assert!(rest.trim_start().starts_with('('), "{sql}");
    assert!(rest.contains("text[]"), "{sql}");
    assert!(
        !sql.contains("it's") && !sql.contains("2026-10-16"),
        "{sql}"
    );
    assert!(!sql.contains("timestamp with time zone"), "{sql}");
    // One parameter for the list, then limit and offset.
    assert_eq!(p.binds.len(), 3, "{sql}");
}

#[test]
fn text_comparisons_work_on_any_column_type_and_in_every_operation() {
    for column in ["id", "x y", "meta", "select", "\u{dc}n\u{ef}"] {
        let sql = planned(&select(None, Some(text_eq(column, "v")))).sql;
        let head = format!(
            "{}::text COLLATE \"C\" = ",
            chelabase_query::sql::quote_ident(column)
        );
        assert!(sql.contains(&head), "{head} not in {sql}");
        assert!(
            !sql.contains("AS numeric") && !sql.contains("AS jsonb"),
            "{sql}"
        );
    }
    let f = || Some(text_eq("id", "7"));
    let update = Op::Update(Update {
        table: name(),
        set: json!({"select": "a"}),
        filter: f(),
        all: false,
        returning: None,
    });
    let delete = Op::Delete(Delete {
        from: name(),
        filter: f(),
        all: false,
        returning: None,
    });
    let count = Op::Count(Count {
        from: name(),
        filter: f(),
    });
    for op in [update, delete, count, Op::Batch(vec![select(None, f())])] {
        let sql = planned_all(&op).join(" ");
        assert!(sql.contains(r#""id"::text COLLATE "C" = "#), "{sql}");
    }
    // Unknown columns are still unknown.
    assert_eq!(
        plan_error(&select(None, Some(text_eq("nope", "v")))).code(),
        "unknown_column"
    );
}

fn planned_all(op: &Op) -> Vec<String> {
    plan(op, &Odd::new(), "public")
        .unwrap()
        .into_iter()
        .map(|p| p.sql)
        .collect()
}

#[test]
fn a_plain_condition_is_byte_for_byte_what_it_was() {
    let eq = planned(&select(None, Some(cond("id", CondOp::Eq, json!(1), false)))).sql;
    assert!(eq.contains(r#"WHERE "id" = CAST($1 AS bigint)"#), "{eq}");
    assert!(
        !eq.contains("COLLATE") && !eq.contains(r#""id"::text"#),
        "{eq}"
    );
    let is_null = planned(&select(
        None,
        Some(cond("id", CondOp::Is, Value::Null, false)),
    ))
    .sql;
    assert!(is_null.contains(r#"WHERE "id" IS NULL"#), "{is_null}");
    let both = planned(&select(
        None,
        Some(Filter::Or(vec![
            cond("id", CondOp::Is, Value::Null, false),
            text_eq("select", "a"),
        ])),
    ))
    .sql;
    assert!(
        both.contains(r#"("id" IS NULL OR "select"::text COLLATE "C" = "#),
        "{both}"
    );
    let inn = planned(&select(
        None,
        Some(cond("id", CondOp::In, json!([1, 2]), false)),
    ))
    .sql;
    assert!(!inn.contains("COLLATE"), "{inn}");
}

#[test]
fn the_bind_plan_finds_each_text_value_by_its_place() {
    // A plan made for one tree serves any tree of the same shape: the value is read from
    // the request's own tree, by path, through and/or/not.
    let tree = |a: &str, b: &str| {
        select(
            None,
            Some(Filter::Or(vec![
                Filter::Not(Box::new(text_eq("id", a))),
                Filter::And(vec![text_eq("select", b)]),
            ])),
        )
    };
    let first = tree("1", "x");
    let second = tree("2", "y\"z");
    assert_eq!(shape_hash(&first), shape_hash(&second));
    let p = planned(&first);
    assert_eq!(
        p.sql.matches(r#"::text COLLATE "C" = "#).count(),
        2,
        "{}",
        p.sql
    );
    assert_eq!(
        bound_texts(&second, &p),
        vec![Some("2".to_owned()), Some("y\"z".to_owned())]
    );
}

// ---- the shape hash

#[test]
fn text_and_text_of_are_part_of_the_shape() {
    let h = |op: &Op| shape_hash(op);
    let eq = |text| select(None, Some(cond("id", CondOp::Eq, json!("1"), text)));
    let inn = |text| select(None, Some(cond("id", CondOp::In, json!(["1"]), text)));
    assert_ne!(h(&eq(true)), h(&eq(false)));
    assert_ne!(h(&inn(true)), h(&inn(false)));
    assert_ne!(h(&eq(true)), h(&inn(true)));
    // Values and list lengths still don't matter.
    let eq_v = select(None, Some(cond("id", CondOp::Eq, json!("other"), true)));
    assert_eq!(h(&eq(true)), h(&eq_v));
    let in_v = select(
        None,
        Some(cond("id", CondOp::In, json!(["1", "2", "3"]), true)),
    );
    assert_eq!(h(&inn(true)), h(&in_v));
    // Nested conditions too.
    let nested = |text| {
        select(
            None,
            Some(Filter::Not(Box::new(Filter::And(vec![cond(
                "id",
                CondOp::Eq,
                json!("1"),
                text,
            )])))),
        )
    };
    assert_ne!(h(&nested(true)), h(&nested(false)));

    // A select item: text_of is neither the bare column nor the aliased one.
    let item = |i: SelectItem| select(Some(vec![i]), None);
    let aliased = item(SelectItem::Aliased {
        col: "id".into(),
        alias: "a".into(),
    });
    let tof = item(text_of("id", "a"));
    assert_ne!(h(&tof), h(&aliased));
    assert_ne!(h(&tof), h(&item(SelectItem::Col("id".into()))));
    assert_ne!(h(&tof), h(&item(text_of("id", "b"))));
    assert_ne!(h(&tof), h(&item(text_of("select", "a"))));
    assert_eq!(h(&tof), h(&item(text_of("id", "a"))));
}

#[test]
fn trees_with_one_shape_hash_make_one_plan() {
    // The cache's contract, for the new forms: same hash, same SQL.
    let a = select(
        Some(vec![text_of("id", "k")]),
        Some(cond("select", CondOp::In, json!(["a"]), true)),
    );
    let b = select(
        Some(vec![text_of("id", "k")]),
        Some(cond("select", CondOp::In, json!(["b", "c"]), true)),
    );
    assert_eq!(shape_hash(&a), shape_hash(&b));
    assert_eq!(planned(&a), planned(&b));
    let plain = select(
        Some(vec![text_of("id", "k")]),
        Some(cond("select", CondOp::In, json!(["a"]), false)),
    );
    assert_ne!(planned(&a).sql, planned(&plain).sql);
}
