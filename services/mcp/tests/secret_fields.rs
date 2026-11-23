//! The credential and secret-input tables against the MCP spec bundle (`openapi/mcp.json`,
//! every `$ref` resolved; `spec_bundle_is_current` keeps it in step with the API).

use mcp::secret_fields::{
    ALLOWED, CREDENTIAL_FIELDS, Carries, Kind, Pointer, SECRET_INPUTS, UNTYPED,
    answers_with_credential, credential_fields, secret_inputs,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const JSON: &str = "application/json";
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

fn bundle() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../openapi/mcp.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Every operation of `spec`: (operationId, operation).
fn operations(spec: &Value) -> Vec<(String, &Value)> {
    let mut out = vec![];
    for item in spec["paths"].as_object().unwrap().values() {
        for (method, op) in item.as_object().unwrap() {
            if METHODS.contains(&method.as_str()) {
                out.push((op["operationId"].as_str().unwrap().to_owned(), op));
            }
        }
    }
    out
}

/// Each success answer's (media type, schema); a missing schema is `{}`.
fn success_answers(op: &Value) -> Vec<(String, Value)> {
    let mut out = vec![];
    for (code, response) in op["responses"].as_object().unwrap() {
        if !code.starts_with('2') {
            continue;
        }
        if let Some(content) = response.get("content").and_then(Value::as_object) {
            for (media, body) in content {
                out.push((
                    media.clone(),
                    body.get("schema").cloned().unwrap_or(json!({})),
                ));
            }
        }
    }
    out
}

/// A field name or description that suggests a credential.
fn looks_like_credential(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "password",
        "secret",
        "token",
        "key",
        "uri",
        "url",
        "connection",
        "signed",
        "invite",
    ]
    .iter()
    .any(|w| text.contains(w))
}

fn types(schema: &Map<String, Value>) -> BTreeSet<String> {
    match schema.get("type") {
        Some(Value::String(t)) => [t.clone()].into(),
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// A node that may hold any JSON: no schema to say what is in it.
fn opaque(schema: &Map<String, Value>) -> bool {
    if ["allOf", "anyOf", "oneOf", "properties", "items"]
        .iter()
        .any(|k| schema.contains_key(*k))
        || schema
            .get("additionalProperties")
            .is_some_and(Value::is_object)
    {
        return false;
    }
    let mut ts = types(schema);
    if ts.len() == 1 && ts.contains("null") {
        return false;
    }
    ts.remove("null");
    ts.is_empty() || ts.contains("object") || ts.contains("array")
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Finding {
    /// A string field whose name or description suggests a credential.
    Looks(String),
    /// A node with no schema (or a cycle, which this walk can't follow).
    Opaque(String),
}

/// Walks one schema: `*` is an array's items, `{}` any key of a map.
fn walk(schema: &Value, at: &str, name: &str, desc: &str, out: &mut Vec<Finding>) {
    let Some(map) = schema.as_object() else {
        out.push(Finding::Opaque(at.to_owned()));
        return;
    };
    if map.contains_key("x-circular") || opaque(map) {
        out.push(Finding::Opaque(at.to_owned()));
        return;
    }
    let desc = map
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or(desc);
    for k in ["allOf", "anyOf", "oneOf"] {
        for branch in map.get(k).and_then(Value::as_array).into_iter().flatten() {
            walk(branch, at, name, desc, out);
        }
    }
    for (field, sub) in map
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let sub_desc = sub.get("description").and_then(Value::as_str).unwrap_or("");
        walk(sub, &format!("{at}/{field}"), field, sub_desc, out);
    }
    if let Some(items) = map.get("items") {
        walk(items, &format!("{at}/*"), name, desc, out);
    }
    // `true` is as untyped as a schema-less value: any extra key could hold anything.
    if let Some(values) = map
        .get("additionalProperties")
        .filter(|v| v.is_object() || **v == Value::Bool(true))
    {
        if values.is_object() {
            walk(values, &format!("{at}/{{}}"), name, desc, out);
        } else {
            out.push(Finding::Opaque(format!("{at}/{{}}")));
        }
    }
    if types(map).contains("string") && (looks_like_credential(name) || looks_like_credential(desc))
    {
        out.push(Finding::Looks(at.to_owned()));
    }
}

/// Every operation's findings, by (operation, media type).
fn findings(spec: &Value) -> BTreeMap<(String, String), BTreeSet<Finding>> {
    let mut out: BTreeMap<(String, String), BTreeSet<Finding>> = BTreeMap::new();
    for (id, op) in operations(spec) {
        for (media, schema) in success_answers(op) {
            let entry = out.entry((id.clone(), media.clone())).or_default();
            if media != JSON {
                // Not JSON: nothing in it can be pointed at, so the whole answer is opaque.
                entry.insert(Finding::Opaque(String::new()));
                continue;
            }
            let mut found = vec![];
            walk(&schema, "", "", "", &mut found);
            entry.extend(found);
        }
    }
    out
}

fn under(at: &str, entry: &str) -> bool {
    entry.is_empty() || at == entry || at.starts_with(&format!("{entry}/"))
}

fn untyped_covering(
    id: &str,
    media: &str,
    at: &str,
) -> Option<&'static mcp::secret_fields::Untyped> {
    UNTYPED
        .iter()
        .find(|u| u.operation == id && u.media == media && under(at, u.at))
}

fn is_credential(id: &str, at: &str) -> bool {
    credential_fields(id).iter().any(|(p, _)| *p == at)
}

fn is_allowed(id: &str, at: &str) -> bool {
    ALLOWED.iter().any(|a| a.at.contains(&(id, at)))
}

/// Findings no table accounts for, as `operation media pointer`.
fn unlisted(spec: &Value) -> Vec<String> {
    let mut out = vec![];
    for ((id, media), found) in findings(spec) {
        for f in found {
            let ok = match &f {
                Finding::Looks(at) => {
                    is_credential(&id, at)
                        || is_allowed(&id, at)
                        || untyped_covering(&id, &media, at).is_some()
                }
                Finding::Opaque(at) => {
                    untyped_covering(&id, &media, at).is_some()
                        || (media == JSON && is_credential(&id, at))
                }
            };
            if !ok {
                out.push(format!("{id} {media} {f:?}"));
            }
        }
    }
    out
}

/// Table entries the spec no longer gives a reason for.
fn stale(spec: &Value) -> Vec<String> {
    let found = findings(spec);
    let looks = |id: &str, at: &str| {
        found
            .iter()
            .any(|((i, _), fs)| i == id && fs.contains(&Finding::Looks(at.to_owned())))
    };
    let mut out = vec![];
    for a in ALLOWED {
        for (id, at) in a.at {
            if !looks(id, at) {
                out.push(format!(
                    "allow-listed {id} {at}: not a credential-looking field"
                ));
            }
        }
    }
    for u in UNTYPED {
        let hit = found
            .get(&(u.operation.to_owned(), u.media.to_owned()))
            .is_some_and(|fs| fs.contains(&Finding::Opaque(u.at.to_owned())));
        if !hit {
            out.push(format!(
                "untyped {} {} {:?}: not an untyped answer",
                u.operation, u.media, u.at
            ));
        }
    }
    for (id, fields) in CREDENTIAL_FIELDS {
        for (at, _) in *fields {
            let listed_untyped =
                untyped_covering(id, JSON, at).is_some_and(|u| u.carries == Carries::Listed);
            if !looks(id, at) && !listed_untyped {
                out.push(format!("credential {id} {at}: in no answer"));
            }
        }
    }
    out
}

/// A spec with one operation answering `schema`.
fn fixture(id: &str, schema: Value) -> Value {
    json!({"paths": {"/fixture": {"get": {
        "operationId": id,
        "responses": {"200": {"content": {JSON: {"schema": schema}}}, "404": {}},
    }}}})
}

#[test]
fn every_credential_looking_field_is_listed_or_allow_listed() {
    let spec = bundle();
    assert_eq!(unlisted(&spec), Vec::<String>::new());
    assert_eq!(stale(&spec), Vec::<String>::new());
    // A fixture operation with an unlisted `password` fails, named.
    let schema = json!({"type": "object", "properties": {
        "id": {"type": "string", "format": "uuid"},
        "nested": {"type": "array", "items": {"type": "object", "properties": {
            "password": {"type": ["string", "null"]},
        }}},
        "note": {"type": "string", "description": "The session token, once."},
        "count": {"type": "integer", "description": "How many keys."},
    }});
    assert_eq!(
        unlisted(&fixture("fixtureUnlistedPassword", schema)),
        [
            "fixtureUnlistedPassword application/json Looks(\"/nested/*/password\")",
            "fixtureUnlistedPassword application/json Looks(\"/note\")",
        ]
    );
}

#[test]
fn untyped_answers_are_hand_classified() {
    let spec = bundle();
    assert_eq!(unlisted(&spec), Vec::<String>::new());
    // Every classification names its handler; `Listed` only where the table lists something.
    for u in UNTYPED {
        assert!(
            u.handler.contains(".rs"),
            "{} names no handler",
            u.operation
        );
        let listed = answers_with_credential(u.operation);
        assert_eq!(
            u.carries == Carries::Listed,
            listed,
            "{} {:?}",
            u.operation,
            u.carries
        );
    }
    // A fixture operation answering untyped, wholly or in part, fails, named.
    for (schema, at) in [
        (json!({}), ""),
        (json!({"type": "object"}), ""),
        (
            json!({"type": "object", "properties": {"extra": {"type": ["object", "null"]}}}),
            "/extra",
        ),
        (json!({"type": "array", "items": {}}), "/*"),
        (
            json!({"type": "object", "properties": {"loop": {"x-circular": "Node"}}}),
            "/loop",
        ),
        // Named properties next to free-form extra keys: the extra keys are untyped.
        (
            json!({"type": "object", "properties": {"id": {"type": "string"}},
                "additionalProperties": true}),
            "/{}",
        ),
        (
            json!({"type": "object", "properties": {"id": {"type": "string"}},
                "additionalProperties": {}}),
            "/{}",
        ),
    ] {
        assert_eq!(
            unlisted(&fixture("fixtureUntyped", schema)),
            [format!("fixtureUntyped application/json Opaque({at:?})")]
        );
    }
    // `additionalProperties: false` or a typed schema leaves nothing unchecked.
    for extra in [json!(false), json!({"type": "string", "format": "uuid"})] {
        let schema = json!({"type": "object", "properties": {"id": {"type": "string"}},
            "additionalProperties": extra});
        assert_eq!(
            unlisted(&fixture("fixtureClosed", schema)),
            Vec::<String>::new()
        );
    }
    // A success answer that isn't JSON is classified as a whole.
    let text = json!({"paths": {"/fixture": {"get": {
        "operationId": "fixtureText",
        "responses": {"200": {"content": {"text/plain": {"schema": {"type": "string"}}}}},
    }}}});
    assert_eq!(unlisted(&text), ["fixtureText text/plain Opaque(\"\")"]);
}

#[test]
fn every_untyped_answer_is_hand_classified() {
    // Whole answers with no schema in the document, by name.
    let expected: BTreeSet<(&str, &str)> = [
        ("cancelSqlRun", JSON),
        ("consoleQuery", JSON),
        ("exportMergePlan", "text/plain"),
        ("getDataApiTypes", JSON),
        ("getDataApiTypes", "text/typescript"),
        ("getServiceLogs", "text/plain"),
        ("getSqlSchema", JSON),
        ("runSql", "application/x-ndjson"),
        ("storageAbortUpload", JSON),
        ("storageCopyObject", JSON),
        ("storageCreateBucket", JSON),
        ("storageDeleteBucket", JSON),
        ("storageDeleteObject", JSON),
        ("storageDeleteObjects", JSON),
        ("storageDownloadObject", "application/octet-stream"),
        ("storageListBuckets", JSON),
        ("storageListObjects", JSON),
        ("storageListParts", JSON),
        ("storageMoveObject", JSON),
        ("storageResolveObjects", JSON),
        ("storageSignObject", JSON),
        ("storageUpdateBucket", JSON),
    ]
    .into();
    let spec = bundle();
    let whole: BTreeSet<(&str, &str)> = findings(&spec)
        .iter()
        .filter(|(_, fs)| fs.contains(&Finding::Opaque(String::new())))
        .map(|((id, media), _)| {
            let u = UNTYPED
                .iter()
                .find(|u| u.operation == id && u.media == media && u.at.is_empty())
                .unwrap_or_else(|| panic!("{id} {media} is not classified"));
            (u.operation, u.media)
        })
        .collect();
    assert_eq!(whole, expected);
    let classified: BTreeSet<(&str, &str)> = UNTYPED
        .iter()
        .filter(|u| u.at.is_empty())
        .map(|u| (u.operation, u.media))
        .collect();
    assert_eq!(classified, expected);
    // The two that answer with credentials say so.
    for id in ["storageSignObject", "storageResolveObjects"] {
        let u = UNTYPED.iter().find(|u| u.operation == id).unwrap();
        assert_eq!(u.carries, Carries::Listed);
    }
    for id in [
        "consoleQuery",
        "runSql",
        "storageDownloadObject",
        "getServiceLogs",
    ] {
        let u = UNTYPED.iter().find(|u| u.operation == id).unwrap();
        assert_eq!(u.carries, Carries::MemberData, "{id}");
    }
}

/// The credential table, written once.
const NAMED: &[(&str, &[(Pointer, Kind)])] = &[
    ("createInvite", &[("/invite_url", Kind::InviteLink)]),
    ("createKey", &[("/key", Kind::DataKey)]),
    (
        "createPersonalLogin",
        &[
            ("/password", Kind::Password),
            ("/connection_string", Kind::ConnectionString),
        ],
    ),
    ("createProjectInvite", &[("/invite_url", Kind::InviteLink)]),
    ("createStorageWebhook", &[("/secret", Kind::WebhookSecret)]),
    (
        "getBranchConnection",
        &[
            ("/password", Kind::Password),
            ("/uri", Kind::ConnectionString),
        ],
    ),
    (
        "getDatabaseConnection",
        &[
            ("/password", Kind::Password),
            ("/uri", Kind::ConnectionString),
        ],
    ),
    ("putFunction", &[("/signing_secret", Kind::SigningSecret)]),
    ("resendInvite", &[("/invite_url", Kind::InviteLink)]),
    (
        "resetPersonalLoginPassword",
        &[
            ("/password", Kind::Password),
            ("/connection_string", Kind::ConnectionString),
        ],
    ),
    ("rotateKey", &[("/key", Kind::DataKey)]),
    (
        "rotateStorageWebhookSecret",
        &[("/secret", Kind::WebhookSecret)],
    ),
    ("storageResolveObjects", &[("/urls/*", Kind::SignedUrl)]),
    ("storageSignObject", &[("/signed_url", Kind::SignedUrl)]),
    (
        "updateFunction",
        &[("/signing_secret", Kind::SigningSecret)],
    ),
];

#[test]
fn named_operations_have_their_fields() {
    for (id, fields) in NAMED {
        assert_eq!(credential_fields(id), *fields, "{id}");
        assert!(answers_with_credential(id), "{id}");
    }
    let named: BTreeSet<&str> = NAMED.iter().map(|(id, _)| *id).collect();
    let table: BTreeSet<&str> = CREDENTIAL_FIELDS.iter().map(|(id, _)| *id).collect();
    assert_eq!(table, named, "the table holds exactly the named operations");
    assert_eq!(
        CREDENTIAL_FIELDS.len(),
        named.len(),
        "one row per operation"
    );
    // These answer 204: nothing to seal.
    let spec = bundle();
    for id in [
        "resetBranchPassword",
        "resetDatabasePassword",
        "resetRolePassword",
    ] {
        assert!(!answers_with_credential(id), "{id}");
        let (_, op) = operations(&spec)
            .into_iter()
            .find(|(i, _)| i == id)
            .unwrap();
        assert!(success_answers(op).is_empty(), "{id} answers a body");
    }
    // Not credentials, though named like them.
    for id in [
        "listRoles",
        "createRole",
        "updateRole",
        "listKeys",
        "updateKey",
        "listFunctions",
    ] {
        assert!(!answers_with_credential(id), "{id}");
    }
    assert!(credential_fields("noSuchOperation").is_empty());
    for (_, fields) in CREDENTIAL_FIELDS {
        for (at, _) in *fields {
            assert!(at.starts_with('/') && !at.contains("{}"), "{at}");
        }
    }
}

/// (secret input's operation, its pointer, the operations that read that resource back).
const READ_BACK: &[(&str, &str, &[&str])] = &[
    (
        "createAppUser",
        "/password",
        &[
            "createAppUser",
            "getAppUser",
            "listAppUsers",
            "updateAppUser",
            "deactivateAppUser",
            "reactivateAppUser",
        ],
    ),
    (
        "putFunctionSecret",
        "/value",
        &["putFunctionSecret", "listFunctionSecrets"],
    ),
];

/// Every property name anywhere in a schema.
fn property_names(schema: &Value, out: &mut BTreeSet<String>) {
    match schema {
        Value::Object(map) => {
            if let Some(props) = map.get("properties").and_then(Value::as_object) {
                out.extend(props.keys().cloned());
            }
            map.values().for_each(|v| property_names(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| property_names(v, out)),
        _ => {}
    }
}

#[test]
fn no_secret_input_is_readable_back() {
    let spec = bundle();
    let ops: BTreeMap<String, &Value> = operations(&spec).into_iter().collect();
    let mut covered = BTreeSet::new();
    for (id, at, readers) in READ_BACK {
        assert!(
            secret_inputs(id).contains(at),
            "{id} {at} is not a secret input"
        );
        covered.insert((*id, *at));
        let field = at.rsplit('/').next().unwrap();
        for reader in *readers {
            let op = ops
                .get(*reader)
                .unwrap_or_else(|| panic!("no operation {reader}"));
            for (media, schema) in success_answers(op) {
                assert_eq!(media, JSON, "{reader}");
                let mut names = BTreeSet::new();
                property_names(&schema, &mut names);
                assert!(
                    !names.contains(field),
                    "{reader} answers `{field}`, the secret input of {id}"
                );
            }
        }
    }
    let table: BTreeSet<(&str, &str)> = SECRET_INPUTS
        .iter()
        .flat_map(|(id, ps)| ps.iter().map(move |p| (*id, *p)))
        .collect();
    assert_eq!(
        table, covered,
        "each secret input has its read-back operations here"
    );
}

/// Path and query parameters named like credentials, with why they are none.
const PARAMS_NOT_SECRET: &[(&str, &str, &str)] = &[
    (
        "deleteFunctionSecret",
        "secret",
        "the secret's name; its value is never in a URL",
    ),
    (
        "putFunctionSecret",
        "secret",
        "the secret's name; the value is the body's `value`",
    ),
    ("revokeKey", "key_id", "the key's id"),
    ("updateKey", "key_id", "the key's id"),
    ("rotateKey", "key_id", "the key's id"),
    (
        "revokeInvite",
        "invite_id",
        "the invite's id; accepting needs the code",
    ),
    (
        "resendInvite",
        "invite_id",
        "the invite's id; accepting needs the code",
    ),
    (
        "listAppUsers",
        "page_token",
        "a page cursor (app_user_admin.rs), no access",
    ),
];

/// Resolves a table pointer (`*` for array items) inside a request schema.
fn request_field<'a>(schema: &'a Value, at: &str) -> Option<&'a Value> {
    let mut node = schema;
    for seg in at.strip_prefix('/')?.split('/') {
        node = if seg == "*" {
            node.get("items")?
        } else {
            node.get("properties")?.get(seg)?
        };
    }
    Some(node)
}

#[test]
fn secret_inputs_are_body_fields_only() {
    let spec = bundle();
    let ops: BTreeMap<String, &Value> = operations(&spec).into_iter().collect();
    for (id, pointers) in SECRET_INPUTS {
        let op = ops.get(*id).unwrap_or_else(|| panic!("no operation {id}"));
        let schema = &op["requestBody"]["content"][JSON]["schema"];
        for at in *pointers {
            let field = request_field(schema, at)
                .unwrap_or_else(|| panic!("{id} {at} is not a body field"));
            assert!(
                types(field.as_object().unwrap()).contains("string"),
                "{id} {at}"
            );
        }
    }
    let mut seen = BTreeSet::new();
    for (id, op) in operations(&spec) {
        for param in op
            .get("parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = param["name"].as_str().unwrap();
            let place = param["in"].as_str().unwrap();
            if matches!(place, "path" | "query") && looks_like_credential(name) {
                let known = PARAMS_NOT_SECRET
                    .iter()
                    .any(|(i, n, _)| *i == id && *n == name);
                assert!(
                    known,
                    "{id} takes `{name}` in its {place}: a credential in a URL?"
                );
                seen.insert((id.clone(), name.to_owned()));
            }
        }
    }
    for (id, name, _) in PARAMS_NOT_SECRET {
        assert!(
            seen.contains(&(id.to_string(), name.to_string())),
            "{id} {name}: no such parameter"
        );
    }
}

#[test]
fn allow_listed_credential_like_fields_are_pinned() {
    let pinned = [
        ("listRoles", "/*/password"),
        ("createRole", "/password"),
        ("updateRole", "/password"),
        ("listKeys", "/*/key"),
        ("updateKey", "/key"),
        ("listFunctions", "/*/signing_secret"),
        ("getDatabaseConnection", "/http_url"),
        ("getBranchConnection", "/http_url"),
        ("getDatabaseConnection", "/password_state"),
        ("listStorageWebhooks", "/*/url"),
        ("listAppUsers", "/next_page_token"),
    ];
    for (id, at) in pinned {
        assert!(is_allowed(id, at), "{id} {at} is not allow-listed");
    }
    for a in ALLOWED {
        assert!(a.reason.len() > 10, "{:?}: no reason", a.at);
        assert!(a.handler.contains(".rs"), "{:?}: no handler named", a.at);
        for (id, at) in a.at {
            assert!(
                !is_credential(id, at),
                "{id} {at} is both listed and allow-listed"
            );
        }
    }
    // A role's `password` is a state: an enum in the document, never a value.
    let spec = bundle();
    let ops: BTreeMap<String, &Value> = operations(&spec).into_iter().collect();
    for (id, at) in [
        ("listRoles", "/*/password"),
        ("createRole", "/password"),
        ("updateRole", "/password"),
    ] {
        for (_, schema) in success_answers(ops[id]) {
            let field = request_field(&schema, at).unwrap_or_else(|| panic!("{id} {at}"));
            assert!(
                field.get("enum").is_some(),
                "{id} {at} is not an enum: {field}"
            );
        }
    }
}
