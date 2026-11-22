use serde_json::Value;
use std::path::PathBuf;

fn openapi(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../openapi")
        .join(name)
}

fn read(name: &str) -> Value {
    let text = std::fs::read_to_string(openapi(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::from_str(&text).unwrap()
}

fn built() -> Value {
    mcp::spec::build(&read("management.json"), &read("data.json")).unwrap()
}

fn has_ref(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.contains_key("$ref") || m.values().any(has_ref),
        Value::Array(a) => a.iter().any(has_ref),
        _ => false,
    }
}

#[test]
fn spec_bundle_is_current() {
    let bundle = built();
    let json = mcp::spec::render(&bundle);
    let path = openapi("mcp.json");
    if std::env::var_os("CHELABASE_BLESS").is_some() {
        std::fs::write(&path, &json).unwrap();
    }
    let committed =
        std::fs::read_to_string(&path).expect("openapi/mcp.json is missing; run `just api-gen`");
    assert!(
        committed == json,
        "openapi/mcp.json is stale; run `just api-gen` and commit it"
    );
    assert!(!has_ref(
        &serde_json::from_str::<Value>(&committed).unwrap()
    ));
    assert!(
        committed.len() < 4 * 1024 * 1024,
        "bundle is {} bytes",
        committed.len()
    );
    let ops: usize = bundle["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|p| p.as_object().unwrap().len())
        .sum();
    let excluded = mcp::policy::TABLE
        .iter()
        .filter(|(_, _, c)| *c == mcp::policy::Class::Excluded)
        .count();
    assert_eq!(ops, mcp::policy::TABLE.len() - excluded);
}

/// `(operationId, METHOD, operation)` for every operation of the bundle.
fn bundle_operations(bundle: &Value) -> Vec<(String, String, Value)> {
    let mut out = Vec::new();
    for item in bundle["paths"].as_object().unwrap().values() {
        for (method, op) in item.as_object().unwrap() {
            let id = op["operationId"].as_str().unwrap().to_string();
            out.push((id, method.to_ascii_uppercase(), op.clone()));
        }
    }
    out
}

#[test]
fn excluded_operations_are_absent() {
    let bundle = built();
    let ids: Vec<String> = bundle_operations(&bundle)
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    let excluded: Vec<&str> = mcp::policy::TABLE
        .iter()
        .filter(|(_, _, c)| *c == mcp::policy::Class::Excluded)
        .map(|(id, _, _)| *id)
        .collect();
    assert!(!excluded.is_empty());
    for id in excluded {
        assert!(!ids.iter().any(|i| i == id), "{id} is in the bundle");
        assert!(!mcp::spec::render(&bundle).contains(&format!("\"{id}\"")));
    }
    // A path whose every operation is excluded is gone too.
    for (path, item) in bundle["paths"].as_object().unwrap() {
        assert!(!item.as_object().unwrap().is_empty(), "{path} is empty");
    }
}

#[test]
fn every_operation_has_a_class() {
    for (id, method, op) in bundle_operations(&built()) {
        let class = mcp::policy::static_class(&id, &method).unwrap();
        assert_ne!(class, mcp::policy::Class::Excluded);
        assert_eq!(op["x-class"], class.as_str(), "{id}");
        match mcp::policy::rule_of(&id) {
            Some(rule) => assert_eq!(op["x-class-rule"], rule, "{id}"),
            None => assert!(op.get("x-class-rule").is_none(), "{id}"),
        }
    }
}

#[test]
fn console_tree_is_typed() {
    let bundle = built();
    let post = &bundle["paths"]["/v1/databases/{id}/branches/{branch_id}/console/query"]["post"];
    let tree = &post["requestBody"]["content"]["application/json"]["schema"]["properties"]["tree"];
    let text = tree.to_string();
    for op in ["select", "insert", "update", "delete", "call", "batch"] {
        assert!(
            text.contains(&format!("\"{op}\"")),
            "tree schema lacks {op}"
        );
    }
}
