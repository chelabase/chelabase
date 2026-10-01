use rest::query::{Name, Op, parse};

fn rejected(json: &str) -> String {
    match parse(json.as_bytes()) {
        Ok(op) => panic!("{json} should be rejected, parsed as {op:?}"),
        Err(e) => {
            assert_eq!(e.code(), "invalid_query");
            e.to_string()
        }
    }
}

fn accepted(json: &str) -> Op {
    parse(json.as_bytes()).unwrap_or_else(|e| panic!("{json} should parse: {e}"))
}

#[test]
fn v_must_be_1() {
    rejected(r#"{"op":"select","from":"t"}"#);
    rejected(r#"{"v":2,"op":"select","from":"t"}"#);
    rejected(r#"{"v":"1","op":"select","from":"t"}"#);
    rejected(r#"{"v":1.5,"op":"select","from":"t"}"#);
    accepted(r#"{"v":1,"op":"select","from":"t"}"#);
}

#[test]
fn not_json_or_not_an_object_is_rejected() {
    rejected("not json");
    rejected("[]");
    rejected(r#"{"v":1}"#);
    rejected(r#"{"v":1,"op":"merge","from":"t"}"#);
}

#[test]
fn unknown_field_anywhere_is_rejected() {
    // top level
    let msg = rejected(r#"{"v":1,"op":"select","from":"t","form":"t"}"#);
    assert!(msg.contains("form"), "{msg}");
    // a select item
    rejected(r#"{"v":1,"op":"select","from":"t","select":[{"col":"a","as":"b","x":1}]}"#);
    // an orderBy item
    rejected(r#"{"v":1,"op":"select","from":"t","orderBy":[{"col":"a","direction":"asc"}]}"#);
    // a filter: an unknown operator, and an unknown key beside the combinators
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"col":"a","equals":1}}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"and":[{"col":"a","eq":1}],"x":1}}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"not":{"col":"a","eq":1,"x":1}}}"#);
    // writes and calls
    rejected(r#"{"v":1,"op":"insert","into":"t","values":[{"a":1}],"upsert":true}"#);
    rejected(r#"{"v":1,"op":"update","table":"t","set":{"a":1},"all":true,"x":1}"#);
    rejected(r#"{"v":1,"op":"delete","from":"t","all":true,"x":1}"#);
    rejected(r#"{"v":1,"op":"call","fn":"f","args":{},"x":1}"#);
    // the batch wrapper and an op inside it
    rejected(r#"{"v":1,"op":"batch","ops":[],"x":1}"#);
    rejected(r#"{"v":1,"op":"batch","ops":[{"op":"delete","from":"t","all":true,"x":1}]}"#);
    // a field of one op on another
    rejected(r#"{"v":1,"op":"delete","from":"t","all":true,"limit":1}"#);
}

#[test]
fn one_operator_per_filter() {
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"col":"a","eq":1,"gt":0}}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"col":"a"}}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"eq":1}}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"and":[],"or":[]}}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"col":"a","eq":1,"and":[]}}"#);
}

#[test]
fn filter_values_must_fit_the_operator() {
    // `in` takes an array
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"col":"a","in":"x"}}"#);
    // `is` takes null, true or false
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"col":"a","is":"x"}}"#);
    // like and ilike take a string
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"col":"a","like":1}}"#);
    // a comparison with null is never true: `is` is the way
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"col":"a","eq":null}}"#);
    // and/or need a non-empty list; col must be a string
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"and":[]}}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"or":{}}}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","where":{"col":1,"eq":1}}"#);
    accepted(r#"{"v":1,"op":"select","from":"t","where":{"col":"a","is":false}}"#);
}

#[test]
fn update_and_delete_need_where_or_all() {
    rejected(r#"{"v":1,"op":"update","table":"t","set":{"a":1}}"#);
    rejected(r#"{"v":1,"op":"update","table":"t","set":{"a":1},"all":false}"#);
    rejected(r#"{"v":1,"op":"delete","from":"t"}"#);
    rejected(r#"{"v":1,"op":"delete","from":"t","all":false}"#);
    accepted(r#"{"v":1,"op":"update","table":"t","set":{"a":1},"all":true}"#);
    accepted(r#"{"v":1,"op":"delete","from":"t","where":{"col":"a","eq":1}}"#);
    accepted(r#"{"v":1,"op":"delete","from":"t","all":true}"#);
}

#[test]
fn update_set_must_be_a_nonempty_object() {
    rejected(r#"{"v":1,"op":"update","table":"t","set":{},"all":true}"#);
    rejected(r#"{"v":1,"op":"update","table":"t","set":[1],"all":true}"#);
}

#[test]
fn insert_needs_at_least_one_row() {
    rejected(r#"{"v":1,"op":"insert","into":"t","values":[]}"#);
    rejected(r#"{"v":1,"op":"insert","into":"t","values":{"a":1}}"#);
    rejected(r#"{"v":1,"op":"insert","into":"t","values":[1]}"#);
    rejected(r#"{"v":1,"op":"insert","into":"t","values":[{}]}"#);
}

#[test]
fn insert_rows_with_different_keys_are_rejected() {
    rejected(r#"{"v":1,"op":"insert","into":"t","values":[{"a":1},{"b":1}]}"#);
    rejected(r#"{"v":1,"op":"insert","into":"t","values":[{"a":1},{"a":1,"b":2}]}"#);
    rejected(r#"{"v":1,"op":"insert","into":"t","values":[{"a":1,"b":2},{"a":1}]}"#);
    accepted(r#"{"v":1,"op":"insert","into":"t","values":[{"a":1,"b":2},{"b":3,"a":null}]}"#);
}

#[test]
fn batch_of_51_is_rejected() {
    let one = r#"{"op":"delete","from":"t","all":true}"#;
    let batch = |n: usize| {
        format!(
            r#"{{"v":1,"op":"batch","ops":[{}]}}"#,
            vec![one; n].join(",")
        )
    };
    let msg = rejected(&batch(51));
    assert!(msg.contains("50"), "{msg}");
    match accepted(&batch(50)) {
        Op::Batch(ops) => assert_eq!(ops.len(), 50),
        other => panic!("{other:?}"),
    }
}

#[test]
fn nested_batch_is_rejected() {
    rejected(r#"{"v":1,"op":"batch","ops":[{"op":"batch","ops":[]}]}"#);
    rejected(r#"{"v":1,"op":"batch","ops":[{"v":1,"op":"batch","ops":[]}]}"#);
}

#[test]
fn ops_inside_a_batch_may_carry_v_1() {
    accepted(r#"{"v":1,"op":"batch","ops":[{"v":1,"op":"delete","from":"t","all":true}]}"#);
    rejected(r#"{"v":1,"op":"batch","ops":[{"v":2,"op":"delete","from":"t","all":true}]}"#);
    rejected(r#"{"v":1,"op":"batch"}"#);
    rejected(r#"{"v":1,"op":"batch","ops":{}}"#);
}

#[test]
fn limit_and_offset_are_not_negative() {
    rejected(r#"{"v":1,"op":"select","from":"t","limit":-1}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","offset":-1}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","limit":1.5}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","limit":"10"}"#);
    accepted(r#"{"v":1,"op":"select","from":"t","limit":0,"offset":0}"#);
}

#[test]
fn select_list_and_returning_shapes() {
    rejected(r#"{"v":1,"op":"select","from":"t","select":[]}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","select":"*"}"#);
    rejected(r#"{"v":1,"op":"select","from":"t","select":[1]}"#);
    rejected(r#"{"v":1,"op":"delete","from":"t","all":true,"returning":"id"}"#);
    rejected(r#"{"v":1,"op":"delete","from":"t","all":true,"returning":[]}"#);
    accepted(r#"{"v":1,"op":"delete","from":"t","all":true,"returning":"*"}"#);
    accepted(r#"{"v":1,"op":"delete","from":"t","all":true,"returning":["id"]}"#);
}

#[test]
fn bad_aliases_are_rejected() {
    let sel = |alias: &str| {
        format!(
            r#"{{"v":1,"op":"select","from":"t","select":[{{"col":"a","as":{}}}]}}"#,
            serde_json::to_string(alias).unwrap()
        )
    };
    rejected(&sel(""));
    let msg = rejected(&sel(&"a".repeat(64)));
    assert!(msg.contains("63"), "{msg}");
    // 64 bytes in 32 characters
    rejected(&sel(&"\u{e9}".repeat(32)));
    rejected(&sel("a\0b"));
    accepted(&sel(&"a".repeat(63)));
    accepted(&sel(r#"x" , (select 1) ""#));
}

#[test]
fn call_args_are_named() {
    rejected(r#"{"v":1,"op":"call","fn":"f","args":[1,2]}"#);
    accepted(r#"{"v":1,"op":"call","fn":"f"}"#);
    accepted(r#"{"v":1,"op":"call","fn":"s.f","args":{"a":1}}"#);
}

#[test]
fn names_have_at_most_one_dot_and_nonempty_parts() {
    for bad in ["", ".", "a.", ".a", "a.b.c", "a..b"] {
        rejected(&format!(r#"{{"v":1,"op":"select","from":"{bad}"}}"#));
    }
    match accepted(r#"{"v":1,"op":"select","from":"billing.invoices"}"#) {
        Op::Select(s) => assert_eq!(
            s.from,
            Name {
                schema: Some("billing".into()),
                name: "invoices".into()
            }
        ),
        other => panic!("{other:?}"),
    }
    match accepted(r#"{"v":1,"op":"select","from":"Order"}"#) {
        Op::Select(s) => assert_eq!(
            s.from,
            Name {
                schema: None,
                name: "Order".into()
            }
        ),
        other => panic!("{other:?}"),
    }
}
