mod common;

use common::{FakeCatalog, MAX_ROWS, one, op, plans};
use chelabase_query::query::{Path, Slot, resolve};
use chelabase_query::sql::{Bind, BindValue, PlanError, plan, resolve_bind};
use serde_json::json;

fn at(slot: Slot) -> Path {
    Path { op: None, slot }
}
fn text_where(i: &[usize]) -> Bind {
    Bind::Text(at(Slot::Where(i.to_vec())))
}
fn json_where(i: &[usize]) -> Bind {
    Bind::Json(at(Slot::Where(i.to_vec())))
}
const LIMIT: Bind = Bind::Limit { op: None };
const OFFSET: Bind = Bind::Offset { op: None };

fn code(json: &str) -> &'static str {
    match plans(json) {
        Ok(p) => panic!("{json} should fail, planned {p:?}"),
        Err(e) => e.code(),
    }
}

// ---- golden SQL, one per op ----

#[test]
fn golden_select() {
    let p = one(r#"{"v":1,"op":"select","from":"Order",
            "select":["id",{"col":"select","as":"s"}],
            "where":{"and":[{"col":"total","gte":100},
                            {"or":[{"col":"mood","in":["happy","sad"]},{"col":"select","is":null}]},
                            {"not":{"col":"select","like":"a%"}}]},
            "orderBy":[{"col":"id","dir":"desc","nulls":"last"},{"col":"total"}],
            "limit":50,"offset":10}"#);
    assert_eq!(
        p.sql,
        r#"SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT "id", "select" AS "s" FROM "public"."Order" WHERE ("total" >= CAST($1 AS numeric) AND ("mood" = ANY (SELECT CAST(x AS public.mood) FROM jsonb_array_elements_text($2) x) OR "select" IS NULL) AND NOT ("select" LIKE CAST($3 AS text))) ORDER BY "id" DESC NULLS LAST, "total" ASC LIMIT $4 OFFSET $5) t"#
    );
    assert_eq!(
        p.binds,
        vec![
            text_where(&[0]),
            json_where(&[1, 0]),
            text_where(&[2, 0]),
            LIMIT,
            OFFSET
        ]
    );
}

#[test]
fn golden_select_all_columns_is_explicit() {
    let p = one(r#"{"v":1,"op":"select","from":"Order"}"#);
    assert_eq!(
        p.sql,
        r#"SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT "id", "select", "total", "mood", "tags", "meta", "made" FROM "public"."Order" LIMIT $1 OFFSET $2) t"#
    );
    assert_eq!(p.binds, vec![LIMIT, OFFSET]);
}

#[test]
fn golden_select_operators() {
    let p = one(
        r#"{"v":1,"op":"select","from":"billing.invoices","select":["id"],
            "where":{"and":[{"col":"amount","eq":1},{"col":"amount","neq":2},{"col":"amount","lt":3},
                            {"col":"amount","lte":4},{"col":"amount","gt":5},{"col":"code","ilike":"%x"},
                            {"col":"paid","is":true},{"col":"paid","is":false},{"col":"code","eq":"abc"},
                            {"col":"id","in":[]}]}}"#,
    );
    assert_eq!(
        p.sql,
        r#"SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT "id" FROM "billing"."invoices" WHERE ("amount" = CAST($1 AS numeric) AND "amount" <> CAST($2 AS numeric) AND "amount" < CAST($3 AS numeric) AND "amount" <= CAST($4 AS numeric) AND "amount" > CAST($5 AS numeric) AND "code" ILIKE CAST($6 AS text) AND "paid" IS TRUE AND "paid" IS FALSE AND "code" = CAST($7 AS character varying) AND "id" = ANY (SELECT CAST(x AS uuid) FROM jsonb_array_elements_text($8) x)) LIMIT $9 OFFSET $10) t"#
    );
    assert_eq!(
        p.binds,
        vec![
            text_where(&[0]),
            text_where(&[1]),
            text_where(&[2]),
            text_where(&[3]),
            text_where(&[4]),
            text_where(&[5]),
            text_where(&[8]),
            json_where(&[9]),
            LIMIT,
            OFFSET
        ]
    );
}

#[test]
fn golden_array_and_jsonb_comparisons_bind_json() {
    let p = one(r#"{"v":1,"op":"select","from":"Order","select":["id"],
            "where":{"or":[{"col":"tags","eq":["a","b"]},{"col":"meta","eq":{"k":1}},{"col":"meta","in":[{"k":1},"x"]}]}}"#);
    assert_eq!(
        p.sql,
        r#"SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT "id" FROM "public"."Order" WHERE ("tags" = CAST(ARRAY(SELECT jsonb_array_elements_text($1)) AS text[]) OR "meta" = CAST($2 AS jsonb) OR "meta" = ANY (SELECT x FROM jsonb_array_elements($3) x)) LIMIT $4 OFFSET $5) t"#
    );
    assert_eq!(
        p.binds,
        vec![
            json_where(&[0]),
            json_where(&[1]),
            json_where(&[2]),
            LIMIT,
            OFFSET
        ]
    );
}

#[test]
fn golden_insert() {
    let p = one(r#"{"v":1,"op":"insert","into":"Order",
            "values":[{"total":"1.50","select":"x"},{"select":"y","total":"2"}],"returning":["id"]}"#);
    assert_eq!(
        p.sql,
        r#"WITH r AS (INSERT INTO "public"."Order" ("select", "total") SELECT "select", "total" FROM jsonb_populate_recordset(NULL::"public"."Order", $1) RETURNING "id") SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT * FROM r LIMIT $2) t"#
    );
    assert_eq!(p.binds, vec![Bind::Json(at(Slot::Rows)), LIMIT]);
}

#[test]
fn golden_insert_without_returning() {
    let p = one(r#"{"v":1,"op":"insert","into":"billing.invoices","values":[{"amount":1}]}"#);
    assert_eq!(
        p.sql,
        r#"WITH r AS (INSERT INTO "billing"."invoices" ("amount") SELECT "amount" FROM jsonb_populate_recordset(NULL::"billing"."invoices", $1)) SELECT '[]'::text"#
    );
    assert_eq!(p.binds, vec![Bind::Json(at(Slot::Rows))]);
}

#[test]
fn golden_update() {
    let p = one(
        r#"{"v":1,"op":"update","table":"Order","set":{"mood":"happy","select":"z"},
            "where":{"col":"id","eq":7},"returning":"*"}"#,
    );
    assert_eq!(
        p.sql,
        r#"WITH r AS (UPDATE "public"."Order" SET ("select", "mood") = (SELECT "select", "mood" FROM jsonb_populate_record(NULL::"public"."Order", $1)) WHERE "id" = CAST($2 AS bigint) RETURNING "id", "select", "total", "mood", "tags", "meta", "made") SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT * FROM r LIMIT $3) t"#
    );
    assert_eq!(
        p.binds,
        vec![Bind::Json(at(Slot::Set)), text_where(&[]), LIMIT]
    );
}

#[test]
fn golden_delete() {
    let p = one(r#"{"v":1,"op":"delete","from":"billing.invoices","all":true}"#);
    assert_eq!(
        p.sql,
        r#"WITH r AS (DELETE FROM "billing"."invoices") SELECT '[]'::text"#
    );
    assert!(p.binds.is_empty());

    let p = one(
        r#"{"v":1,"op":"delete","from":"billing.invoices","where":{"not":{"col":"paid","is":null}},"returning":["id","amount"]}"#,
    );
    assert_eq!(
        p.sql,
        r#"WITH r AS (DELETE FROM "billing"."invoices" WHERE NOT ("paid" IS NULL) RETURNING "id", "amount") SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT * FROM r LIMIT $1) t"#
    );
    assert_eq!(p.binds, vec![LIMIT]);
}

#[test]
fn golden_call() {
    let p = one(r#"{"v":1,"op":"call","fn":"add_points","args":{"n":5,"user_id":"u"}}"#);
    assert_eq!(
        p.sql,
        r#"SELECT to_json("public"."add_points"("user_id" => CAST($1 AS uuid), "n" => CAST($2 AS integer)))::text"#
    );
    assert_eq!(
        p.binds,
        vec![
            Bind::Text(at(Slot::Arg("user_id".into()))),
            Bind::Text(at(Slot::Arg("n".into())))
        ]
    );

    // an argument with a default may be left out
    let p = one(r#"{"v":1,"op":"call","fn":"public.add_points","args":{"user_id":"u"}}"#);
    assert_eq!(
        p.sql,
        r#"SELECT to_json("public"."add_points"("user_id" => CAST($1 AS uuid)))::text"#
    );
}

/// Array arguments take a JSON array (as array columns do) and json/jsonb arguments take
/// the JSON value as is; text casts would refuse `[1,2]` and mangle a JSON string.
#[test]
fn golden_call_array_and_json_arguments() {
    let p = one(
        r#"{"v":1,"op":"call","fn":"tag","args":{"ids":[1,2],"meta":{"a":1},"raw":"s","codes":["ab"]}}"#,
    );
    assert_eq!(
        p.sql,
        r#"SELECT to_json("public"."tag"("ids" => CAST(ARRAY(SELECT jsonb_array_elements_text($1)) AS integer[]), "meta" => CAST($2 AS jsonb), "raw" => CAST($3 AS json), "codes" => CAST(ARRAY(SELECT jsonb_array_elements_text($4)) AS character varying[])))::text"#
    );
    let arg = |n: &str| Bind::Json(at(Slot::Arg(n.into())));
    assert_eq!(
        p.binds,
        vec![arg("ids"), arg("meta"), arg("raw"), arg("codes")]
    );
}

/// `to_json` of a void is `""`: a void call answers NULL, which the engine sends as JSON
/// `null`. The call sits in the target list, so even a STABLE or IMMUTABLE function runs
/// (an unreferenced subquery would be pruned, and its RAISE never seen).
#[test]
fn golden_call_void() {
    let p = one(r#"{"v":1,"op":"call","fn":"touch"}"#);
    assert_eq!(p.sql, r#"SELECT NULLIF("public"."touch"()::text, '')"#);
    assert!(p.binds.is_empty());
}

#[test]
fn golden_call_set_returning() {
    let p = one(r#"{"v":1,"op":"call","fn":"recent_orders","args":{"n":3}}"#);
    assert_eq!(
        p.sql,
        r#"SELECT coalesce(json_agg(x.f), '[]')::text FROM (SELECT "public"."recent_orders"("n" => CAST($1 AS integer)) AS f) x"#
    );
    assert_eq!(p.binds, vec![Bind::Text(at(Slot::Arg("n".into())))]);

    // a row type with a column named `f` can't shadow the result
    let p = one(r#"{"v":1,"op":"call","fn":"pairs"}"#);
    assert_eq!(
        p.sql,
        r#"SELECT coalesce(json_agg(x.f), '[]')::text FROM (SELECT "public"."pairs"() AS f) x"#
    );
    assert!(p.binds.is_empty());
}

#[test]
fn golden_batch() {
    let ps = plans(
        r#"{"v":1,"op":"batch","ops":[
            {"op":"insert","into":"billing.invoices","values":[{"amount":1}]},
            {"op":"update","table":"Order","set":{"total":2},"where":{"col":"id","eq":1}}]}"#,
    )
    .unwrap();
    assert_eq!(ps.len(), 2);
    assert_eq!(
        ps[0].sql,
        r#"WITH r AS (INSERT INTO "billing"."invoices" ("amount") SELECT "amount" FROM jsonb_populate_recordset(NULL::"billing"."invoices", $1)) SELECT '[]'::text"#
    );
    assert_eq!(
        ps[0].binds,
        vec![Bind::Json(Path {
            op: Some(0),
            slot: Slot::Rows
        })]
    );
    assert_eq!(
        ps[1].sql,
        r#"WITH r AS (UPDATE "public"."Order" SET ("total") = (SELECT "total" FROM jsonb_populate_record(NULL::"public"."Order", $1)) WHERE "id" = CAST($2 AS bigint)) SELECT '[]'::text"#
    );
    assert_eq!(
        ps[1].binds,
        vec![
            Bind::Json(Path {
                op: Some(1),
                slot: Slot::Set
            }),
            Bind::Text(Path {
                op: Some(1),
                slot: Slot::Where(vec![])
            })
        ]
    );
}

// ---- binding ----

#[test]
fn binds_resolve_against_the_tree() {
    let tree = op(r#"{"v":1,"op":"batch","ops":[{"op":"select","from":"Order",
            "where":{"and":[{"col":"total","gte":100},{"col":"select","eq":"x"},{"col":"mood","in":["happy"]},
                            {"col":"id","eq":true},{"not":{"col":"meta","eq":{"a":[1]}}}]}}]}"#);
    let p = |slot| Path { op: Some(0), slot };
    assert_eq!(resolve(&tree, &p(Slot::Where(vec![0]))), Some(&json!(100)));
    assert_eq!(
        resolve(&tree, &p(Slot::Where(vec![4, 0]))),
        Some(&json!({"a":[1]}))
    );
    assert_eq!(resolve(&tree, &p(Slot::Where(vec![9]))), None);
    assert_eq!(
        resolve(
            &tree,
            &Path {
                op: None,
                slot: Slot::Where(vec![0])
            }
        ),
        None
    );

    let text =
        |i: usize| resolve_bind(&tree, &Bind::Text(p(Slot::Where(vec![i]))), MAX_ROWS).unwrap();
    assert_eq!(text(0), BindValue::Text(Some("100".into())));
    assert_eq!(text(1), BindValue::Text(Some("x".into())));
    assert_eq!(text(3), BindValue::Text(Some("true".into())));
    assert_eq!(
        resolve_bind(&tree, &Bind::Json(p(Slot::Where(vec![2]))), MAX_ROWS).unwrap(),
        BindValue::Json(&json!(["happy"]))
    );
    assert_eq!(
        resolve_bind(&tree, &Bind::Text(p(Slot::Where(vec![4, 0]))), MAX_ROWS).unwrap(),
        BindValue::Text(Some(r#"{"a":[1]}"#.into()))
    );

    let call = op(r#"{"v":1,"op":"call","fn":"add_points","args":{"user_id":null,"n":2}}"#);
    assert_eq!(
        resolve_bind(
            &call,
            &Bind::Text(at(Slot::Arg("user_id".into()))),
            MAX_ROWS
        )
        .unwrap(),
        BindValue::Text(None)
    );
    let ins = op(r#"{"v":1,"op":"insert","into":"Order","values":[{"id":1}]}"#);
    assert_eq!(resolve(&ins, &at(Slot::Rows)), Some(&json!([{"id":1}])));
    let upd = op(r#"{"v":1,"op":"update","table":"Order","set":{"id":1},"all":true}"#);
    assert_eq!(resolve(&upd, &at(Slot::Set)), Some(&json!({"id":1})));
}

#[test]
fn limit_is_clamped_and_defaulted() {
    let limit_of = |json: &str| {
        let tree = op(json);
        let p = plan(&tree, &FakeCatalog::new(), "public").unwrap();
        let limit = p[0]
            .binds
            .iter()
            .find(|b| matches!(b, Bind::Limit { .. }))
            .unwrap()
            .clone();
        let offset = p[0]
            .binds
            .iter()
            .find(|b| matches!(b, Bind::Offset { .. }))
            .unwrap()
            .clone();
        let int = |b: &Bind| match resolve_bind(&tree, b, MAX_ROWS).unwrap() {
            BindValue::Int(n) => BindValue::Int(n),
            other => panic!("{other:?}"),
        };
        (p[0].sql.clone(), int(&limit), int(&offset))
    };
    let (a, l, o) = limit_of(r#"{"v":1,"op":"select","from":"Order","limit":5000,"offset":7}"#);
    assert_eq!((l, o), (BindValue::Int(1000), BindValue::Int(7)));
    let (b, l, o) = limit_of(r#"{"v":1,"op":"select","from":"Order","limit":10}"#);
    assert_eq!((l, o), (BindValue::Int(10), BindValue::Int(0)));
    let (c, l, _) = limit_of(r#"{"v":1,"op":"select","from":"Order"}"#);
    assert_eq!(l, BindValue::Int(1000));
    let (_, l, _) = limit_of(r#"{"v":1,"op":"select","from":"Order","limit":0}"#);
    assert_eq!(l, BindValue::Int(0));
    // the clamp uses the max_rows given at binding, not one frozen into the plan
    let tree = op(r#"{"v":1,"op":"select","from":"Order","limit":500}"#);
    let p = plan(&tree, &FakeCatalog::new(), "public").unwrap();
    let limit = &p[0].binds[0];
    assert_eq!(limit, &Bind::Limit { op: None });
    assert_eq!(resolve_bind(&tree, limit, 1000), Some(BindValue::Int(500)));
    assert_eq!(resolve_bind(&tree, limit, 100), Some(BindValue::Int(100)));
    let bare = op(r#"{"v":1,"op":"select","from":"Order"}"#);
    assert_eq!(resolve_bind(&bare, limit, 7), Some(BindValue::Int(7)));
    // the limit is a parameter, so the text doesn't change
    assert_eq!(a, b);
    assert_eq!(b, c);
}

#[test]
fn returning_is_capped() {
    for json in [
        r#"{"v":1,"op":"insert","into":"Order","values":[{"id":1}],"returning":"*"}"#,
        r#"{"v":1,"op":"update","table":"Order","set":{"id":1},"all":true,"returning":["id"]}"#,
        r#"{"v":1,"op":"delete","from":"Order","all":true,"returning":["id"]}"#,
    ] {
        let tree = op(json);
        let p = plan(&tree, &FakeCatalog::new(), "public").unwrap();
        let last = p[0].binds.last().unwrap();
        assert_eq!(last, &Bind::Limit { op: None });
        assert_eq!(resolve_bind(&tree, last, 25).unwrap(), BindValue::Int(25));
        let n = p[0].binds.len();
        assert!(
            p[0].sql
                .ends_with(&format!("FROM (SELECT * FROM r LIMIT ${n}) t")),
            "{}",
            p[0].sql
        );
    }
}

// ---- names ----

#[test]
fn hostile_names_are_unknown_not_injected() {
    let hostile = [
        r#"x\"; drop table \"Order\"; --"#,
        "order",
        "\u{41e}rder",
        "ORDER",
        "public.order",
    ];
    for name in hostile {
        let json = format!(r#"{{"v":1,"op":"select","from":"{name}"}}"#);
        assert_eq!(code(&json), "unknown_table", "{name}");
        let json = format!(r#"{{"v":1,"op":"delete","from":"{name}","all":true}}"#);
        assert_eq!(code(&json), "unknown_table", "{name}");
    }
    for name in [r#"id\" ; drop"#, "ID", "Select", "s\u{435}lect"] {
        for json in [
            format!(r#"{{"v":1,"op":"select","from":"Order","select":["{name}"]}}"#),
            format!(r#"{{"v":1,"op":"select","from":"Order","where":{{"col":"{name}","eq":1}}}}"#),
            format!(r#"{{"v":1,"op":"select","from":"Order","orderBy":[{{"col":"{name}"}}]}}"#),
            format!(r#"{{"v":1,"op":"insert","into":"Order","values":[{{"{name}":1}}]}}"#),
            format!(r#"{{"v":1,"op":"update","table":"Order","set":{{"{name}":1}},"all":true}}"#),
            format!(r#"{{"v":1,"op":"delete","from":"Order","all":true,"returning":["{name}"]}}"#),
        ] {
            assert_eq!(code(&json), "unknown_column", "{json}");
        }
    }
    for name in [r#"add_points\"()"#, "Add_points", "billing.add_points"] {
        let json = format!(r#"{{"v":1,"op":"call","fn":"{name}","args":{{"user_id":"u"}}}}"#);
        assert_eq!(code(&json), "unknown_function", "{name}");
    }
    // an unknown argument name doesn't resolve either
    assert_eq!(
        code(r#"{"v":1,"op":"call","fn":"add_points","args":{"user_id":"u","x\"":1}}"#),
        "unknown_function"
    );
    // a required argument left out
    assert_eq!(
        code(r#"{"v":1,"op":"call","fn":"add_points","args":{"n":1}}"#),
        "unknown_function"
    );
}

#[test]
fn identifiers_come_from_the_catalog_and_quotes_are_doubled() {
    let mut cat = FakeCatalog::new();
    cat.tables[1].name = "in\"voices".into();
    cat.tables[1].columns[1].name = "am\"ount".into();
    let tree = op(r#"{"v":1,"op":"select","from":"billing.in\"voices","select":["am\"ount"]}"#);
    let p = plan(&tree, &cat, "public").unwrap();
    assert_eq!(
        p[0].sql,
        r#"SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT "am""ount" FROM "billing"."in""voices" LIMIT $1 OFFSET $2) t"#
    );
}

#[test]
fn hostile_aliases_are_quoted() {
    let p = one(r#"{"v":1,"op":"select","from":"Order","select":[
            {"col":"id","as":"x\" , (select 1) \""},{"col":"total","as":"t"},{"col":"select","as":"select"}]}"#);
    assert_eq!(
        p.sql,
        r#"SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT "id" AS "x"" , (select 1) """, "total" AS "t", "select" AS "select" FROM "public"."Order" LIMIT $1 OFFSET $2) t"#
    );
}

#[test]
fn qualified_and_default_schema_resolution() {
    let cat = FakeCatalog::new();
    let run = |json: &str, default_schema: &str| plan(&op(json), &cat, default_schema);
    let sel = |from: &str| format!(r#"{{"v":1,"op":"select","from":"{from}","select":["id"]}}"#);

    assert!(matches!(
        run(&sel("invoices"), "public"),
        Err(PlanError::UnknownTable(_))
    ));
    let p = run(&sel("invoices"), "billing").unwrap();
    assert!(
        p[0].sql.contains(r#"FROM "billing"."invoices""#),
        "{}",
        p[0].sql
    );
    let p = run(&sel("billing.invoices"), "public").unwrap();
    assert!(
        p[0].sql.contains(r#"FROM "billing"."invoices""#),
        "{}",
        p[0].sql
    );
    let p = run(&sel("public.Order"), "billing").unwrap();
    assert!(
        p[0].sql.contains(r#"FROM "public"."Order""#),
        "{}",
        p[0].sql
    );
    assert!(matches!(
        run(&sel("Order"), "billing"),
        Err(PlanError::UnknownTable(_))
    ));

    let call = r#"{"v":1,"op":"call","fn":"add_points","args":{"user_id":"u"}}"#;
    assert!(matches!(
        run(call, "billing"),
        Err(PlanError::UnknownFunction(_))
    ));
    assert!(run(call, "public").is_ok());
}

// ---- refusals at plan time ----

#[test]
fn like_on_numeric_is_invalid_query() {
    assert_eq!(
        code(r#"{"v":1,"op":"select","from":"Order","where":{"col":"total","like":"1%"}}"#),
        "invalid_query"
    );
    assert_eq!(
        code(r#"{"v":1,"op":"select","from":"Order","where":{"col":"tags","ilike":"a"}}"#),
        "invalid_query"
    );
    assert_eq!(
        code(r#"{"v":1,"op":"select","from":"Order","where":{"col":"id","like":"1"}}"#),
        "invalid_query"
    );
}

#[test]
fn is_true_on_a_non_boolean_is_invalid_query() {
    assert_eq!(
        code(r#"{"v":1,"op":"select","from":"Order","where":{"col":"id","is":true}}"#),
        "invalid_query"
    );
    // `is null` works on anything
    one(r#"{"v":1,"op":"select","from":"Order","where":{"col":"id","is":null}}"#);
}

#[test]
fn in_on_an_array_column_is_invalid_query() {
    assert_eq!(
        code(r#"{"v":1,"op":"select","from":"Order","where":{"col":"tags","in":[["a"]]}}"#),
        "invalid_query"
    );
}

#[test]
fn in_empty_array_is_valid() {
    let p =
        one(r#"{"v":1,"op":"select","from":"Order","select":["id"],"where":{"col":"id","in":[]}}"#);
    assert!(
        p.sql.contains(
            r#""id" = ANY (SELECT CAST(x AS bigint) FROM jsonb_array_elements_text($1) x)"#
        ),
        "{}",
        p.sql
    );
}

#[test]
fn writing_a_generated_column_is_invalid_query() {
    assert_eq!(
        code(r#"{"v":1,"op":"insert","into":"Order","values":[{"made":1}]}"#),
        "invalid_query"
    );
    assert_eq!(
        code(r#"{"v":1,"op":"update","table":"Order","set":{"made":1},"all":true}"#),
        "invalid_query"
    );
}

#[test]
fn duplicate_output_names_are_invalid_query() {
    assert_eq!(
        code(r#"{"v":1,"op":"select","from":"Order","select":["id","id"]}"#),
        "invalid_query"
    );
    assert_eq!(
        code(r#"{"v":1,"op":"select","from":"Order","select":["id",{"col":"total","as":"id"}]}"#),
        "invalid_query"
    );
    assert_eq!(
        code(r#"{"v":1,"op":"delete","from":"Order","all":true,"returning":["id","id"]}"#),
        "invalid_query"
    );
}

#[test]
fn ambiguous_function_is_invalid_query() {
    let mut cat = FakeCatalog::new();
    let twin = cat.functions[0].clone();
    cat.functions.push(twin);
    let tree = op(r#"{"v":1,"op":"call","fn":"add_points","args":{"user_id":"u"}}"#);
    match plan(&tree, &cat, "public") {
        Err(e @ PlanError::InvalidQuery(_)) => {
            assert_eq!(e.code(), "invalid_query");
            assert!(e.to_string().contains("add_points"), "{e}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn plan_errors_map_to_codes() {
    assert_eq!(PlanError::UnknownTable("t".into()).code(), "unknown_table");
    assert_eq!(
        PlanError::UnknownColumn {
            table: "t".into(),
            column: "c".into()
        }
        .code(),
        "unknown_column"
    );
    assert_eq!(
        PlanError::UnknownFunction("f".into()).code(),
        "unknown_function"
    );
    assert_eq!(PlanError::InvalidQuery("x".into()).code(), "invalid_query");
}

/// Bare JSON numbers keep their digits all the way to the bind: as the text of a `text`
/// parameter, and inside the JSON of a `jsonb` one (inserts, updates, `in`, json args).
#[test]
fn big_numbers_reach_the_binds_with_their_digits() {
    const BIG: &str = "123456789012345678901234567890";
    const FRACTION: &str = "1234567890.12345678901234567890";
    let text = |tree: &chelabase_query::query::Op, bind: Bind| match resolve_bind(tree, &bind, MAX_ROWS) {
        Some(BindValue::Text(Some(v))) => v.into_owned(),
        other => panic!("{other:?}"),
    };
    let json = |tree: &chelabase_query::query::Op, bind: Bind| match resolve_bind(tree, &bind, MAX_ROWS) {
        Some(BindValue::Json(v)) => serde_json::to_string(v).unwrap(),
        other => panic!("{other:?}"),
    };
    let ins = op(&format!(
        r#"{{"v":1,"op":"insert","into":"Order","values":[{{"total":{BIG}}},{{"total":{FRACTION}}}]}}"#
    ));
    assert_eq!(
        json(&ins, Bind::Json(at(Slot::Rows))),
        format!(r#"[{{"total":{BIG}}},{{"total":{FRACTION}}}]"#)
    );
    let upd = op(&format!(
        r#"{{"v":1,"op":"update","table":"Order","set":{{"total":{BIG}}},"all":true}}"#
    ));
    assert_eq!(
        json(&upd, Bind::Json(at(Slot::Set))),
        format!(r#"{{"total":{BIG}}}"#)
    );
    let sel = op(&format!(
        r#"{{"v":1,"op":"select","from":"Order","where":{{"and":[{{"col":"total","eq":{BIG}}},{{"col":"total","in":[{FRACTION}]}}]}}}}"#
    ));
    assert_eq!(text(&sel, text_where(&[0])), BIG);
    assert_eq!(json(&sel, json_where(&[1])), format!("[{FRACTION}]"));
    let call = op(&format!(
        r#"{{"v":1,"op":"call","fn":"add_points","args":{{"n":{BIG},"tag":{FRACTION}}}}}"#
    ));
    assert_eq!(text(&call, Bind::Text(at(Slot::Arg("n".into())))), BIG);
    assert_eq!(
        text(&call, Bind::Text(at(Slot::Arg("tag".into())))),
        FRACTION
    );
}
