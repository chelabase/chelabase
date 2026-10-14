//! The data API's OpenAPI request schema (`QueryRequest` in the committed
//! `openapi/data.json`, which rest's `document_is_current` keeps current) against the
//! parser: the client's fixtures fit it, and the schema and [`chelabase_query::parse`]
//! accept and reject the same inputs, except for the gaps JSON Schema can't express.

use std::path::Path;

use serde_json::{Value, json};

fn root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn validator() -> jsonschema::Validator {
    let path = root().join("openapi/data.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; run `just api-gen`", path.display()));
    let doc: Value = serde_json::from_str(&text).unwrap();
    let schema = json!({
        "$ref": "#/components/schemas/QueryRequest",
        "components": doc["components"],
    });
    jsonschema::draft202012::new(&schema).expect("QueryRequest is a valid JSON Schema")
}

fn schema_errors(validator: &jsonschema::Validator, tree: &Value) -> Vec<String> {
    validator
        .iter_errors(tree)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect()
}

/// A case's JSON text, parsed with this crate's arbitrary-precision numbers.
fn tree(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn ops(n: usize) -> String {
    let op = r#"{"op": "select", "from": "t"}"#;
    format!(
        r#"{{"v": 1, "op": "batch", "ops": [{}]}}"#,
        vec![op; n].join(", ")
    )
}

#[test]
fn every_client_fixture_matches_the_request_schema() {
    let validator = validator();
    let dir = root().join("packages/client/fixtures");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let fixture = tree(&std::fs::read_to_string(&path).unwrap());
        let errors = schema_errors(&validator, &fixture["tree"]);
        assert!(errors.is_empty(), "{}: {errors:?}", path.display());
        count += 1;
    }
    assert!(count >= 19, "only {count} fixtures in {}", dir.display());
}

#[test]
fn schema_and_parser_reject_the_same_inputs() {
    let validator = validator();
    let cases: Vec<(&str, String)> =
        vec![
        ("missing v", r#"{"op": "select", "from": "t"}"#.into()),
        ("v is 2", r#"{"v": 2, "op": "select", "from": "t"}"#.into()),
        ("v is a string", r#"{"v": "1", "op": "select", "from": "t"}"#.into()),
        ("unknown top-level field", r#"{"v": 1, "op": "select", "from": "t", "x": 1}"#.into()),
        ("unknown op", r#"{"v": 1, "op": "upsert", "into": "t"}"#.into()),
        (
            "database in a batch member",
            r#"{"v": 1, "op": "batch", "ops": [{"op": "select", "from": "t", "database": "d"}]}"#
                .into(),
        ),
        (
            "nested batch",
            r#"{"v": 1, "op": "batch", "ops": [{"op": "batch", "ops": []}]}"#.into(),
        ),
        ("batch with 51 ops", ops(51)),
        // T8 review M3: a count takes no select, paging, ordering or `all`.
        ("count with limit", r#"{"v": 1, "op": "count", "from": "t", "limit": 1}"#.into()),
        ("count with select", r#"{"v": 1, "op": "count", "from": "t", "select": ["a"]}"#.into()),
        ("count with all", r#"{"v": 1, "op": "count", "from": "t", "all": true}"#.into()),
        (
            "database in a batch member count",
            r#"{"v": 1, "op": "batch", "ops": [{"op": "count", "from": "t", "database": "d"}]}"#
                .into(),
        ),
        ("empty select", r#"{"v": 1, "op": "select", "from": "t", "select": []}"#.into()),
        ("negative limit", r#"{"v": 1, "op": "select", "from": "t", "limit": -1}"#.into()),
        ("empty values", r#"{"v": 1, "op": "insert", "into": "t", "values": []}"#.into()),
        ("empty row", r#"{"v": 1, "op": "insert", "into": "t", "values": [{}]}"#.into()),
        (
            "empty set",
            r#"{"v": 1, "op": "update", "table": "t", "set": {}, "all": true}"#.into(),
        ),
        (
            "update without where or all",
            r#"{"v": 1, "op": "update", "table": "t", "set": {"a": 1}}"#.into(),
        ),
        (
            "delete with all false and no where",
            r#"{"v": 1, "op": "delete", "from": "t", "all": false}"#.into(),
        ),
        (
            "filter with two operators",
            r#"{"v": 1, "op": "select", "from": "t", "where": {"col": "a", "eq": 1, "gt": 0}}"#
                .into(),
        ),
        (
            "empty and",
            r#"{"v": 1, "op": "select", "from": "t", "where": {"and": []}}"#.into(),
        ),
        (
            "filter without an operator",
            r#"{"v": 1, "op": "select", "from": "t", "where": {"col": "a"}}"#.into(),
        ),
        (
            "eq null",
            r#"{"v": 1, "op": "select", "from": "t", "where": {"col": "a", "eq": null}}"#.into(),
        ),
        (
            "is a number",
            r#"{"v": 1, "op": "select", "from": "t", "where": {"col": "a", "is": 1}}"#.into(),
        ),
        (
            "like a number",
            r#"{"v": 1, "op": "select", "from": "t", "where": {"col": "a", "like": 1}}"#.into(),
        ),
        (
            "returning a column name",
            r#"{"v": 1, "op": "delete", "from": "t", "all": true, "returning": "id"}"#.into(),
        ),
        (
            "empty returning",
            r#"{"v": 1, "op": "delete", "from": "t", "all": true, "returning": []}"#.into(),
        ),
        (
            "select item without as",
            r#"{"v": 1, "op": "select", "from": "t", "select": [{"col": "a"}]}"#.into(),
        ),
        (
            "alias with NUL",
            r#"{"v": 1, "op": "select", "from": "t", "select": [{"col": "a", "as": "x\u0000y"}]}"#
                .into(),
        ),
        ("name with two dots", r#"{"v": 1, "op": "select", "from": "a.b.c"}"#.into()),
        ("empty name", r#"{"v": 1, "op": "select", "from": ""}"#.into()),
        (
            "empty database",
            r#"{"v": 1, "database": "", "op": "select", "from": "t"}"#.into(),
        ),
        (
            "null database",
            r#"{"v": 1, "database": null, "op": "select", "from": "t"}"#.into(),
        ),
        ("args a list", r#"{"v": 1, "op": "call", "fn": "f", "args": []}"#.into()),
        (
            "order direction up",
            r#"{"v": 1, "op": "select", "from": "t", "orderBy": [{"col": "a", "dir": "up"}]}"#
                .into(),
        ),
        (
            "delete with null where and no all",
            r#"{"v": 1, "op": "delete", "from": "t", "where": null}"#.into(),
        ),
        ("null orderBy", r#"{"v": 1, "op": "select", "from": "t", "orderBy": null}"#.into()),
        ("null args", r#"{"v": 1, "op": "call", "fn": "f", "args": null}"#.into()),
    ];
    for (name, text) in &cases {
        assert!(
            chelabase_query::parse(text.as_bytes()).is_err(),
            "{name}: the parser accepts {text}"
        );
        assert!(
            !validator.is_valid(&tree(text)),
            "{name}: the schema accepts {text}"
        );
    }
}

#[test]
fn schema_and_parser_accept_the_same_edge_inputs() {
    let validator = validator();
    let cases = [
        ("empty batch", r#"{"v": 1, "op": "batch", "ops": []}"#),
        (
            "count with a null where",
            r#"{"v": 1, "op": "count", "from": "t", "where": null}"#,
        ),
        (
            "null where",
            r#"{"v": 1, "op": "select", "from": "t", "where": null}"#,
        ),
        (
            "null limit",
            r#"{"v": 1, "op": "select", "from": "t", "limit": null}"#,
        ),
        (
            "a column named and",
            r#"{"v": 1, "op": "select", "from": "t", "where": {"col": "and", "eq": "x"}}"#,
        ),
        (
            "update with where and all",
            r#"{"v": 1, "op": "update", "table": "t", "set": {"a": 1}, "where": {"col": "id", "eq": 1}, "all": true}"#,
        ),
        (
            "a big integer in in",
            r#"{"v": 1, "op": "select", "from": "t", "where": {"col": "id", "in": [9007199254740993, 123456789012345678901234567890]}}"#,
        ),
        (
            "a big integer in values",
            r#"{"v": 1, "op": "insert", "into": "t", "values": [{"id": 9007199254740993, "n": 123456789012345678901234567890}]}"#,
        ),
        // A null optional field is the field left out.
        (
            "null select",
            r#"{"v": 1, "op": "select", "from": "t", "select": null, "offset": null}"#,
        ),
        (
            "null returning",
            r#"{"v": 1, "op": "insert", "into": "t", "values": [{"a": 1}], "returning": null}"#,
        ),
        (
            "update with null where and all",
            r#"{"v": 1, "op": "update", "table": "t", "set": {"a": 1}, "where": null, "all": true}"#,
        ),
    ];
    for (name, text) in cases {
        if let Err(e) = chelabase_query::parse(text.as_bytes()) {
            panic!("{name}: the parser rejects {text}: {e}");
        }
        let errors = schema_errors(&validator, &tree(text));
        assert!(
            errors.is_empty(),
            "{name}: the schema rejects {text}: {errors:?}"
        );
    }
}

/// The rules JSON Schema can't express (ruling P3): the server rejects these, the schema
/// doesn't. A rule that becomes expressible, or a schema that drifts, fails here.
#[test]
fn known_gaps_are_exactly_these() {
    let validator = validator();
    let e32 = "é".repeat(32);
    let cases: Vec<(&str, String)> = vec![
        (
            "insert rows with different keys",
            r#"{"v": 1, "op": "insert", "into": "t", "values": [{"a": 1}, {"b": 2}]}"#.into(),
        ),
        (
            "an alias of 64 bytes in 32 characters",
            format!(
                r#"{{"v": 1, "op": "select", "from": "t", "select": [{{"col": "a", "as": "{e32}"}}]}}"#
            ),
        ),
        (
            "a database name of 64 bytes in 32 characters",
            format!(r#"{{"v": 1, "database": "{e32}", "op": "select", "from": "t"}}"#),
        ),
        (
            "v written 1.0",
            r#"{"v": 1.0, "op": "select", "from": "t"}"#.into(),
        ),
        (
            "limit written 1.0",
            r#"{"v": 1, "op": "select", "from": "t", "limit": 1.0}"#.into(),
        ),
        (
            "limit above i64::MAX",
            r#"{"v": 1, "op": "select", "from": "t", "limit": 9223372036854775808}"#.into(),
        ),
        (
            "offset above i64::MAX",
            r#"{"v": 1, "op": "select", "from": "t", "offset": 9223372036854775808}"#.into(),
        ),
    ];
    for (name, text) in &cases {
        assert!(
            chelabase_query::parse(text.as_bytes()).is_err(),
            "{name}: the parser now accepts {text}"
        );
        let errors = schema_errors(&validator, &tree(text));
        assert!(
            errors.is_empty(),
            "{name}: the schema now rejects {text} ({errors:?}); drop it from the gaps"
        );
    }
}
