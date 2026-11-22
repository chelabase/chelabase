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
    let management = read("management.json");
    let expected: usize = management["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|p| p.as_object().unwrap().len())
        .sum();
    assert_eq!(ops, expected);
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
