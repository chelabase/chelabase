//! Builds the MCP spec bundle (`openapi/mcp.json`) from the management and data documents.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fmt;

/// Why a bundle could not be built.
#[derive(Debug, PartialEq, Eq)]
pub enum SpecError {
    UnknownRef(String),
    Shape(String),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::UnknownRef(r) => write!(f, "unknown $ref {r}"),
            SpecError::Shape(m) => write!(f, "unexpected document shape: {m}"),
        }
    }
}

impl std::error::Error for SpecError {}

const SCHEMA_PREFIX: &str = "#/components/schemas/";
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Builds the bundle from the management and data OpenAPI documents.
///
/// Every local `$ref` is resolved inline; a schema that reaches itself again ends in
/// `{"x-circular": "<Name>"}`. Error responses fold into `x-errors` and the shared envelope is
/// described once under `x-error-envelope`. `consoleQuery`'s `tree` takes `runQuery`'s request
/// schema from the data document.
pub fn build(management: &Value, data: &Value) -> Result<Value, SpecError> {
    let schemas = schemas_of(management)?;
    let mut paths = Map::new();
    for (path, item) in object(management.get("paths"), "paths")? {
        let mut out = Map::new();
        for (method, operation) in object(Some(item), "path item")? {
            if METHODS.contains(&method.as_str()) {
                out.insert(method.clone(), operation_of(operation, schemas)?);
            }
        }
        paths.insert(path.clone(), Value::Object(out));
    }
    type_console_tree(&mut paths, data)?;

    let mut info = Map::new();
    info.insert("title".into(), "Chelabase MCP spec".into());
    info.insert("version".into(), management["info"]["version"].clone());
    let mut sources = Map::new();
    sources.insert("data".into(), hash(&canonical(data)).into());
    sources.insert("management".into(), hash(&canonical(management)).into());
    info.insert("x-sources".into(), Value::Object(sources));

    let mut bundle = Map::new();
    bundle.insert("openapi".into(), management["openapi"].clone());
    bundle.insert("info".into(), Value::Object(info));
    bundle.insert("paths".into(), Value::Object(paths));
    if schemas.contains_key("ErrorEnvelope") {
        let envelope = json!({"$ref": format!("{SCHEMA_PREFIX}ErrorEnvelope")});
        bundle.insert(
            "x-error-envelope".into(),
            resolve(&envelope, schemas, &mut vec![])?,
        );
    }
    let mut bundle = Value::Object(bundle);
    let spec_hash = hash(&canonical(&bundle));
    bundle["info"]["x-spec-hash"] = spec_hash.into();
    Ok(sorted(&bundle))
}

/// The bundle as committed: keys sorted, pretty, one trailing newline.
pub fn render(bundle: &Value) -> String {
    let mut text = serde_json::to_string_pretty(&sorted(bundle)).expect("a Value serializes");
    text.push('\n');
    text
}

fn schemas_of(doc: &Value) -> Result<&Map<String, Value>, SpecError> {
    object(doc.pointer("/components/schemas"), "components.schemas")
}

fn object<'a>(v: Option<&'a Value>, what: &str) -> Result<&'a Map<String, Value>, SpecError> {
    v.and_then(Value::as_object)
        .ok_or_else(|| SpecError::Shape(format!("{what} is not an object")))
}

fn operation_of(op: &Value, schemas: &Map<String, Value>) -> Result<Value, SpecError> {
    let mut out = Map::new();
    for key in ["operationId", "tags", "summary", "description"] {
        if let Some(v) = op.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    for key in ["parameters", "requestBody"] {
        if let Some(v) = op.get(key) {
            out.insert(key.into(), resolve(v, schemas, &mut vec![])?);
        }
    }
    let mut ok = Map::new();
    let mut errors = Map::new();
    for (status, response) in object(op.get("responses"), "responses")? {
        if status.starts_with('2') {
            ok.insert(status.clone(), resolve(response, schemas, &mut vec![])?);
        } else if status.starts_with('4') || status.starts_with('5') || status == "default" {
            let description = response
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            errors.insert(status.clone(), error_codes(description).into());
        }
    }
    out.insert("responses".into(), Value::Object(ok));
    if !errors.is_empty() {
        out.insert("x-errors".into(), Value::Object(errors));
    }
    Ok(Value::Object(out))
}

/// The error codes a response description names: its backticked snake_case words, in order.
fn error_codes(description: &str) -> Vec<String> {
    let mut codes: Vec<String> = Vec::new();
    for (i, part) in description.split('`').enumerate() {
        let is_code = i % 2 == 1
            && part.starts_with(|c: char| c.is_ascii_lowercase())
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if is_code && !codes.iter().any(|c| c == part) {
            codes.push(part.to_string());
        }
    }
    codes
}

/// Resolves local schema refs inline; `stack` holds the schemas being expanded.
fn resolve(
    v: &Value,
    schemas: &Map<String, Value>,
    stack: &mut Vec<String>,
) -> Result<Value, SpecError> {
    match v {
        Value::Object(map) => {
            if let Some(target) = map.get("$ref").and_then(Value::as_str) {
                let name = target
                    .strip_prefix(SCHEMA_PREFIX)
                    .filter(|n| schemas.contains_key(*n))
                    .ok_or_else(|| SpecError::UnknownRef(target.to_string()))?;
                if stack.iter().any(|s| s == name) {
                    return Ok(json!({"x-circular": name}));
                }
                stack.push(name.to_string());
                let resolved = resolve(&schemas[name], schemas, stack);
                stack.pop();
                let mut resolved = resolved?;
                // OpenAPI 3.1 allows siblings of `$ref` (a description); they win.
                if let Some(obj) = resolved.as_object_mut() {
                    for (k, sibling) in map.iter().filter(|(k, _)| *k != "$ref") {
                        if !is_example(k) {
                            obj.insert(k.clone(), resolve(sibling, schemas, stack)?);
                        }
                    }
                }
                return Ok(resolved);
            }
            let mut out = Map::new();
            for (k, child) in map.iter().filter(|(k, _)| !is_example(k)) {
                out.insert(k.clone(), resolve(child, schemas, stack)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .iter()
            .map(|i| resolve(i, schemas, stack))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

fn is_example(key: &str) -> bool {
    key == "example" || key == "examples"
}

/// Replaces `consoleQuery`'s `tree` with the data API's `runQuery` request schema.
fn type_console_tree(paths: &mut Map<String, Value>, data: &Value) -> Result<(), SpecError> {
    let Some(tree) = paths
        .values_mut()
        .filter_map(Value::as_object_mut)
        .flat_map(|item| item.values_mut())
        .find(|op| op["operationId"] == "consoleQuery")
        .and_then(|op| {
            op.pointer_mut("/requestBody/content/application~1json/schema/properties/tree")
        })
    else {
        return Ok(());
    };
    let request = data
        .pointer("/paths/~1data~1v1~1query/post/requestBody/content/application~1json/schema")
        .ok_or_else(|| SpecError::Shape("data.json has no runQuery request schema".into()))?;
    *tree = resolve(request, schemas_of(data)?, &mut vec![])?;
    Ok(())
}

/// A copy with every object's keys in sorted order.
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&m[k])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

fn canonical(v: &Value) -> String {
    sorted(v).to_string()
}

fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(schemas: Value, paths: Value) -> Value {
        json!({"openapi": "3.1.0", "info": {"title": "t", "version": "1"},
               "paths": paths, "components": {"schemas": schemas}})
    }

    fn empty_data() -> Value {
        doc(json!({}), json!({}))
    }

    fn op(responses: Value) -> Value {
        json!({"/x": {"get": {"operationId": "getX", "tags": ["t"], "summary": "s",
            "security": [{"bearer": []}], "responses": responses}}})
    }

    #[test]
    fn resolves_nested_refs() {
        let m = doc(
            json!({
                "A": {"type": "object", "properties": {"b": {"$ref": "#/components/schemas/B"}}},
                "B": {"type": "object", "properties": {"c": {"$ref": "#/components/schemas/C"}}},
                "C": {"type": "string", "example": "x"}
            }),
            op(
                json!({"200": {"description": "ok", "content": {"application/json":
                {"schema": {"$ref": "#/components/schemas/A"}}}}}),
            ),
        );
        let b = build(&m, &empty_data()).unwrap();
        let schema =
            &b["paths"]["/x"]["get"]["responses"]["200"]["content"]["application/json"]["schema"];
        assert_eq!(
            schema["properties"]["b"]["properties"]["c"],
            json!({"type": "string"})
        );
        assert!(!render(&b).contains("$ref"));
        assert!(b["paths"]["/x"]["get"].get("security").is_none());
        assert!(b.get("components").is_none());
    }

    #[test]
    fn marks_a_cycle() {
        let m = doc(
            json!({"Node": {"type": "object", "properties": {
                "next": {"$ref": "#/components/schemas/Node"}}}}),
            op(
                json!({"200": {"description": "ok", "content": {"application/json":
                {"schema": {"$ref": "#/components/schemas/Node"}}}}}),
            ),
        );
        let b = build(&m, &empty_data()).unwrap();
        let schema =
            &b["paths"]["/x"]["get"]["responses"]["200"]["content"]["application/json"]["schema"];
        assert_eq!(schema["properties"]["next"], json!({"x-circular": "Node"}));
    }

    #[test]
    fn refuses_an_unknown_ref() {
        let m = doc(
            json!({}),
            op(
                json!({"200": {"description": "ok", "content": {"application/json":
                {"schema": {"$ref": "#/components/schemas/Missing"}}}}}),
            ),
        );
        let err = build(&m, &empty_data()).unwrap_err();
        assert_eq!(
            err,
            SpecError::UnknownRef("#/components/schemas/Missing".into())
        );
    }

    #[test]
    fn folds_error_responses() {
        let m = doc(
            json!({"ErrorEnvelope": {"type": "object", "properties": {"error": {"type": "string"}}}}),
            op(json!({
                "200": {"description": "ok"},
                "400": {"description": "`invalid_request`: bad id; `invalid_value`: bad value, not `a b`",
                    "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
                "404": {"description": "`not_found`: no such thing"}
            })),
        );
        let b = build(&m, &empty_data()).unwrap();
        let o = &b["paths"]["/x"]["get"];
        assert_eq!(
            o["x-errors"],
            json!({"400": ["invalid_request", "invalid_value"], "404": ["not_found"]})
        );
        assert!(o["responses"].get("400").is_none());
        assert!(o["responses"].get("200").is_some());
        assert_eq!(b["x-error-envelope"]["type"], "object");
    }

    #[test]
    fn bundle_is_deterministic() {
        let m = doc(
            json!({"A": {"type": "string"}}),
            op(json!({"200": {"description": "ok"}})),
        );
        let a = build(&m, &empty_data()).unwrap();
        let b = build(&m, &empty_data()).unwrap();
        assert_eq!(render(&a), render(&b));
        let hash = a["info"]["x-spec-hash"].as_str().unwrap();
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, b["info"]["x-spec-hash"].as_str().unwrap());
        let other = doc(
            json!({"A": {"type": "integer"}}),
            op(json!({"200": {"description": "ok"}})),
        );
        let c = build(&other, &empty_data()).unwrap();
        assert_ne!(hash, c["info"]["x-spec-hash"].as_str().unwrap());
        assert_eq!(a["info"]["x-sources"].as_object().unwrap().len(), 2);
    }
}
