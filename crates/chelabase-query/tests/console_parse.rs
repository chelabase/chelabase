//! RT1 final fix round (batch B, review I3): the console-only filter flag `"parse": true`.
//!
//! `{"col": c, "eq": "<text>", "parse": true}` plans as `c = CAST($n AS <type>)` and `in` as
//! `c = ANY (CAST($n AS <type>[]))`, where `<type>` is the column's own type, schema-qualified
//! and quoted (`ColumnInfo::type_qualified`, e.g. `"pg_catalog"."int8"`), so a by-key read, save
//! or delete can use the primary key's index (the `text` form compares `c::text` and can't).
//! The value is bound as `text` (`text[]` for `in`); Postgres parses it with the type's input
//! function. The flag is part of `shape_hash`, refused by the data API like any unknown key,
//! never combined with `text`, and refused on array columns. Console `in` lists (`text` or
//! `parse`) hold at most `MAX_CONSOLE_IN` items.

use chelabase_query::query::{
    CondOp, Filter, MAX_CONSOLE_IN, Name, Op, ParseMode, Select, parse, parse_with,
};
use chelabase_query::schema::{Raw, Schema, Visibility};
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

fn refused(json: &str, mode: ParseMode) -> String {
    match parse_with(json.as_bytes(), mode) {
        Ok(p) => panic!("{json} should be refused in {mode:?}, parsed as {:?}", p.op),
        Err(e) => {
            assert_eq!(e.code(), "invalid_query", "{json}");
            e.to_string()
        }
    }
}

fn where_tree(filter: &str) -> String {
    format!(r#"{{"v":1,"op":"select","from":"t","where":{filter}}}"#)
}

fn the_filter(op: Op) -> Filter {
    match op {
        Op::Select(s) => s.filter.expect("a filter"),
        other => panic!("not a select: {other:?}"),
    }
}

#[test]
fn console_parses_the_parse_flag_beside_eq_and_in() {
    let f = the_filter(console(&where_tree(
        r#"{"and":[{"col":"id","eq":"7","parse":true},{"not":{"col":"k","in":["a","b"],"parse":true}},{"col":"n","eq":"1","text":true},{"col":"m","eq":1}]}"#,
    )));
    let Filter::And(parts) = f else {
        panic!("not an and");
    };
    assert_eq!(
        parts[0],
        Filter::Cond {
            col: "id".into(),
            op: CondOp::Eq,
            value: json!("7"),
            text: false,
            parse: true,
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
            text: false,
            parse: true,
        }
    );
    // The other forms are untouched.
    assert_eq!(
        parts[2],
        Filter::Cond {
            col: "n".into(),
            op: CondOp::Eq,
            value: json!("1"),
            text: true,
            parse: false,
        }
    );
    assert_eq!(
        parts[3],
        Filter::Cond {
            col: "m".into(),
            op: CondOp::Eq,
            value: json!(1),
            text: false,
            parse: false,
        }
    );
    // In writes and batches too.
    for tree in [
        r#"{"v":1,"op":"update","table":"t","set":{"a":1},"where":{"col":"id","eq":"7","parse":true}}"#,
        r#"{"v":1,"op":"delete","from":"t","where":{"col":"id","in":["7"],"parse":true}}"#,
        r#"{"v":1,"op":"batch","ops":[{"op":"count","from":"t","where":{"col":"id","eq":"7","parse":true}}]}"#,
    ] {
        console(tree);
    }
}

#[test]
fn the_data_api_refuses_parse_like_any_unknown_key() {
    for (form, unknown) in [
        (
            where_tree(r#"{"col":"id","eq":"1","parse":true}"#),
            where_tree(r#"{"col":"id","eq":"1","zzzzz":true}"#),
        ),
        (
            where_tree(r#"{"col":"id","in":["1"],"parse":true}"#),
            where_tree(r#"{"col":"id","in":["1"],"zzzzz":true}"#),
        ),
    ] {
        console(&form);
        let expected = refused(&unknown, ParseMode::DataApi);
        assert_eq!(
            refused(&form, ParseMode::DataApi).replace("parse", "zzzzz"),
            expected,
            "{form}"
        );
        let via_parse = match parse(form.as_bytes()) {
            Ok(p) => panic!("{form} parsed by `parse`: {:?}", p.op),
            Err(e) => e.to_string(),
        };
        assert_eq!(via_parse.replace("parse", "zzzzz"), expected, "{form}");
    }
}

#[test]
fn parse_goes_with_eq_or_in_and_strings_only_and_never_with_text() {
    console(&where_tree(r#"{"col":"id","eq":"1","parse":true}"#));
    console(&where_tree(r#"{"col":"id","in":["1","2"],"parse":true}"#));
    console(&where_tree(r#"{"col":"id","in":[],"parse":true}"#));
    for filter in [
        // never both comparison forms
        r#"{"col":"id","eq":"1","parse":true,"text":true}"#,
        r#"{"col":"id","in":["1"],"text":true,"parse":true}"#,
        // operators other than eq / in
        r#"{"col":"id","neq":"1","parse":true}"#,
        r#"{"col":"id","lt":"1","parse":true}"#,
        r#"{"col":"id","gte":"1","parse":true}"#,
        r#"{"col":"id","like":"1%","parse":true}"#,
        r#"{"col":"id","is":null,"parse":true}"#,
        r#"{"col":"id","parse":true}"#,
        // values that aren't strings
        r#"{"col":"id","eq":1,"parse":true}"#,
        r#"{"col":"id","eq":null,"parse":true}"#,
        r#"{"col":"id","eq":true,"parse":true}"#,
        r#"{"col":"id","eq":["1"],"parse":true}"#,
        r#"{"col":"id","in":[1],"parse":true}"#,
        r#"{"col":"id","in":["1",null],"parse":true}"#,
        r#"{"col":"id","in":"1","parse":true}"#,
        // the flag itself is `true`
        r#"{"col":"id","eq":"1","parse":false}"#,
        r#"{"col":"id","eq":"1","parse":"true"}"#,
        r#"{"col":"id","eq":"1","parse":1}"#,
        r#"{"col":"id","eq":"1","parse":null}"#,
    ] {
        refused(&where_tree(filter), ParseMode::Console);
    }
}

fn in_filter(n: usize, flag: &str) -> String {
    let items = (0..n)
        .map(|i| format!("\"{i}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"col":"id","in":[{items}],"{flag}":true}}"#)
}

#[test]
fn console_in_lists_are_capped() {
    assert_eq!(MAX_CONSOLE_IN, 1000);
    for flag in ["text", "parse"] {
        console(&where_tree(&in_filter(MAX_CONSOLE_IN, flag)));
        let msg = refused(
            &where_tree(&in_filter(MAX_CONSOLE_IN + 1, flag)),
            ParseMode::Console,
        );
        assert!(
            msg.contains(&MAX_CONSOLE_IN.to_string()),
            "{flag}: the refusal names the cap: {msg}"
        );
        // Nested inside and/or/not too.
        let nested = format!(
            r#"{{"not":{{"or":[{{"col":"x","eq":1}},{}]}}}}"#,
            in_filter(MAX_CONSOLE_IN + 1, flag)
        );
        refused(&where_tree(&nested), ParseMode::Console);
    }
}

// ---- planning

struct Fake {
    tables: Vec<TableInfo>,
}

fn col(name: &str, type_sql: &str, type_qualified: &str) -> ColumnInfo {
    ColumnInfo {
        name: name.into(),
        type_sql: type_sql.into(),
        type_qualified: type_qualified.into(),
        array: false,
        text_like: false,
        generated: false,
    }
}

fn fake() -> Fake {
    Fake {
        tables: vec![TableInfo {
            schema: "public".into(),
            name: "t".into(),
            columns: vec![
                col("id", "bigint", r#""pg_catalog"."int8""#),
                col("u", "uuid", r#""pg_catalog"."uuid""#),
                col(
                    "at",
                    "timestamp with time zone",
                    r#""pg_catalog"."timestamptz""#,
                ),
                col("price", "numeric(10,2)", r#""pg_catalog"."numeric""#),
                col("mood", r#""public"."mood""#, r#""public"."mood""#),
                col("we\"ird", "text", r#""pg_catalog"."text""#),
                ColumnInfo {
                    array: true,
                    ..col("tags", "text[]", r#""pg_catalog"."_text""#)
                },
            ],
        }],
    }
}

impl Catalog for Fake {
    fn table(&self, name: &Name) -> Option<&TableInfo> {
        self.tables
            .iter()
            .find(|t| Some(&t.schema) == name.schema.as_ref() && t.name == name.name)
    }

    fn function(&self, _: &Name, _: &[&str]) -> Result<&FunctionInfo, FnLookupError> {
        Err(FnLookupError::NotFound)
    }
}

fn cond(col: &str, op: CondOp, value: Value, text: bool, parse: bool) -> Filter {
    Filter::Cond {
        col: col.into(),
        op,
        value,
        text,
        parse,
    }
}

fn parse_eq(col: &str, v: &str) -> Filter {
    cond(col, CondOp::Eq, json!(v), false, true)
}

fn select(filter: Option<Filter>) -> Op {
    Op::Select(Select {
        from: Name {
            schema: None,
            name: "t".into(),
        },
        select: None,
        filter,
        order_by: vec![],
        limit: None,
        offset: None,
    })
}

fn planned(op: &Op) -> Plan {
    let mut plans = plan(op, &fake(), "public").unwrap_or_else(|e| panic!("{op:?}: {e}"));
    assert_eq!(plans.len(), 1);
    plans.remove(0)
}

fn plan_error(op: &Op) -> PlanError {
    plan(op, &fake(), "public").expect_err("should not plan")
}

/// `sql` without white space, so `ANY (CAST` and `ANY(CAST` read the same.
fn squeezed(sql: &str) -> String {
    sql.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn parse_eq_casts_the_bound_text_to_the_qualified_column_type() {
    let tricky = "O'Brien \"x\" \\ 1; DROP TABLE t";
    let op = select(Some(Filter::And(vec![
        parse_eq("id", "9007199254740993"),
        parse_eq("u", "6f1c1b5e-2d43-4b8e-9a51-0f4f6a1a2b3c"),
        parse_eq("at", "2026-10-16 12:00:00.123456+00"),
        parse_eq("price", "1.50"),
        parse_eq("mood", "happy"),
        Filter::Not(Box::new(parse_eq("we\"ird", tricky))),
    ])));
    let p = planned(&op);
    let sql = squeezed(&p.sql);
    for expect in [
        r#""id"=CAST($1AS"pg_catalog"."int8")"#,
        r#""u"=CAST($2AS"pg_catalog"."uuid")"#,
        r#""at"=CAST($3AS"pg_catalog"."timestamptz")"#,
        r#""price"=CAST($4AS"pg_catalog"."numeric")"#,
        r#""mood"=CAST($5AS"public"."mood")"#,
        r#"NOT("we""ird"=CAST($6AS"pg_catalog"."text"))"#,
    ] {
        assert!(sql.contains(expect), "{expect} not in {}", p.sql);
    }
    // The column is compared as itself (index-usable): never its text, never a collation.
    // (The select's JSON wrapper casts its result to text; only the conditions matter here.)
    for column in [r#""id""#, r#""u""#, r#""at""#, r#""price""#, r#""mood""#, r#""we""ird""#] {
        assert!(
            !p.sql.contains(&format!("{column}::text")),
            "{column} compared as text: {}",
            p.sql
        );
    }
    assert!(!p.sql.contains("COLLATE"), "{}", p.sql);
    // Values are parameters, bound as text.
    assert!(!p.sql.contains("O'Brien") && !p.sql.contains("9007199254740993"));
    assert!(p.param_types()[..6].iter().all(|t| *t == ParamType::Text));
    let texts: Vec<_> = p.binds[..6]
        .iter()
        .map(|b| match resolve_bind(&op, b, 1000) {
            Some(BindValue::Text(t)) => t.map(|c| c.into_owned()),
            other => panic!("not text: {other:?}"),
        })
        .collect();
    assert_eq!(texts[0].as_deref(), Some("9007199254740993"));
    assert_eq!(texts[5].as_deref(), Some(tricky));
}

#[test]
fn parse_in_casts_a_text_array_to_the_qualified_array_type() {
    let op = select(Some(cond(
        "at",
        CondOp::In,
        json!(["2026-10-16 12:00:00+00", "it's"]),
        false,
        true,
    )));
    let p = planned(&op);
    let sql = squeezed(&p.sql);
    assert!(
        sql.contains(r#""at"=ANY(CAST($1AS"pg_catalog"."timestamptz"[]))"#),
        "{}",
        p.sql
    );
    assert!(
        !p.sql.contains("it's") && !p.sql.contains("jsonb"),
        "{}",
        p.sql
    );
    assert_eq!(p.param_types()[0], ParamType::TextArray);
    assert_eq!(
        resolve_bind(&op, &p.binds[0], 1000),
        Some(BindValue::TextArray(vec!["2026-10-16 12:00:00+00", "it's"]))
    );
    // One parameter for the list, then limit and offset.
    assert_eq!(p.binds.len(), 3, "{}", p.sql);
}

#[test]
fn parse_is_refused_on_array_columns_and_unknown_columns() {
    for f in [
        parse_eq("tags", "{a}"),
        cond("tags", CondOp::In, json!(["{a}"]), false, true),
    ] {
        assert_eq!(
            plan_error(&select(Some(f.clone()))).code(),
            "invalid_query",
            "{f:?}"
        );
    }
    assert_eq!(
        plan_error(&select(Some(parse_eq("nope", "1")))).code(),
        "unknown_column"
    );
    // Planned trees that combine the two forms (built by hand, past the parser) are refused.
    assert_eq!(
        plan_error(&select(Some(cond(
            "id",
            CondOp::Eq,
            json!("1"),
            true,
            true
        ))))
        .code(),
        "invalid_query"
    );
}

#[test]
fn parse_is_part_of_the_shape() {
    let h = |f: Filter| shape_hash(&select(Some(f)));
    let eq = |text, parse| cond("id", CondOp::Eq, json!("1"), text, parse);
    let inn = |text, parse| cond("id", CondOp::In, json!(["1"]), text, parse);
    assert_ne!(h(eq(false, true)), h(eq(false, false)));
    assert_ne!(h(eq(false, true)), h(eq(true, false)));
    assert_ne!(h(inn(false, true)), h(inn(true, false)));
    assert_ne!(h(inn(false, true)), h(inn(false, false)));
    // Values and list lengths still don't matter.
    assert_eq!(
        h(eq(false, true)),
        h(cond("id", CondOp::Eq, json!("2"), false, true))
    );
    assert_eq!(
        h(inn(false, true)),
        h(cond("id", CondOp::In, json!(["1", "2", "3"]), false, true))
    );
    // Trees with one hash make one plan.
    let a = select(Some(inn(false, true)));
    let b = select(Some(cond("id", CondOp::In, json!(["9", "8"]), false, true)));
    assert_eq!(planned(&a), planned(&b));
}

// ---- the catalog fills `type_qualified`

#[allow(clippy::too_many_arguments)]
fn ty(
    oid: u32,
    schema: &str,
    name: &str,
    typtype: &str,
    elem: u32,
    base: u32,
    category: &str,
    len: i16,
    formatted: &str,
) -> Value {
    json!([
        oid, schema, name, typtype, elem, base, category, len, formatted
    ])
}

#[test]
fn introspection_qualifies_every_column_type() {
    let raw: Raw = serde_json::from_value(json!({
        "namespaces": [[2200, "public"], [16500, "My Schema"]],
        "types": [
            ty(20, "pg_catalog", "int8", "b", 0, 0, "N", 8, "bigint"),
            ty(1016, "pg_catalog", "_int8", "b", 20, 0, "A", -1, "bigint[]"),
            ty(25, "pg_catalog", "text", "b", 0, 0, "S", -1, "text"),
            ty(1043, "pg_catalog", "varchar", "b", 0, 0, "S", -1, "character varying"),
            ty(2950, "pg_catalog", "uuid", "b", 0, 0, "U", 16, "uuid"),
            ty(1184, "pg_catalog", "timestamptz", "b", 0, 0, "D", 8, "timestamp with time zone"),
            ty(90001, "public", "mood", "e", 0, 0, "E", 4, "mood"),
            ty(90003, "My Schema", "Pos \"Int\"", "d", 0, 20, "N", 8, "\"My Schema\".\"Pos \"\"Int\"\"\""),
        ],
        "enums": [[90001, "happy"]],
        "relations": [[
            40000, "public", "t", "r", false, false,
            [
                ["id", 20, "bigint", false, false, false],
                ["ids", 1016, "bigint[]", true, false, false],
                ["name", 1043, "character varying(3)", true, false, false],
                ["u", 2950, "uuid", true, false, false],
                ["at", 1184, "timestamp(3) with time zone", true, false, false],
                ["mood", 90001, "mood", true, false, false],
                ["pos", 90003, "\"My Schema\".\"Pos \"\"Int\"\"\"", true, false, false],
                ["t", 25, "text", true, false, false]
            ],
            ["id"]
        ]],
        "functions": []
    }))
    .unwrap();
    let schema = Schema::build("v1".into(), raw);
    let view = schema.view(
        Visibility::from_json(
            r#"{"schemas":[2200,16500],"tables":[[40000,true,true,true,true]],"functions":[]}"#,
        )
        .unwrap(),
    );
    let t = view
        .table(&Name {
            schema: Some("public".into()),
            name: "t".into(),
        })
        .expect("the table");
    let got: Vec<(&str, &str)> = t
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.type_qualified.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("id", r#""pg_catalog"."int8""#),
            ("ids", r#""pg_catalog"."_int8""#),
            ("name", r#""pg_catalog"."varchar""#),
            ("u", r#""pg_catalog"."uuid""#),
            ("at", r#""pg_catalog"."timestamptz""#),
            ("mood", r#""public"."mood""#),
            ("pos", r#""My Schema"."Pos ""Int""""#),
            ("t", r#""pg_catalog"."text""#),
        ]
    );
}
