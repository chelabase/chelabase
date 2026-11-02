//! AC 1: one client method per route in `services/identity/openapi.json`.

use chelabase_identity::ops::OPERATIONS;
use std::collections::BTreeSet;

/// m2's routes (#115), not yet in the spec of this checkout. Remove an entry once
/// `openapi.json` lists it; the test accepts either way.
const PENDING_M2: &[(&str, &str)] = &[
    ("POST", "/internal/v1/pools/{pool}/codes"),
    ("POST", "/internal/v1/pools/{pool}/codes/verify"),
    ("GET", "/internal/v1/email/status"),
];

fn spec_operations() -> BTreeSet<(String, String)> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../services/identity/openapi.json"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut ops = BTreeSet::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, _) in item.as_object().unwrap() {
            if method != "parameters" {
                ops.insert((method.to_uppercase(), path.clone()));
            }
        }
    }
    ops
}

#[test]
fn every_path_has_a_method() {
    let mut want = spec_operations();
    assert!(want.len() >= 16, "the spec has at least m1's 16 operations");
    want.extend(
        PENDING_M2
            .iter()
            .map(|(m, p)| (m.to_string(), p.to_string())),
    );
    let have: Vec<(String, String)> = OPERATIONS
        .iter()
        .map(|o| (o.method.to_string(), o.path.to_string()))
        .collect();
    let have_set: BTreeSet<_> = have.iter().cloned().collect();
    assert_eq!(have.len(), have_set.len(), "an operation is listed twice");
    assert_eq!(have_set, want);
    let names: BTreeSet<_> = OPERATIONS.iter().map(|o| o.name).collect();
    assert_eq!(names.len(), OPERATIONS.len(), "operation names are unique");
}

#[test]
fn operation_ids_match_the_spec() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../services/identity/openapi.json"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for o in OPERATIONS {
        if let Some(op) = doc["paths"][o.path][o.method.to_lowercase()].as_object() {
            assert_eq!(op["operationId"], o.operation_id, "{} {}", o.method, o.path);
        }
    }
}

/// The table is only a list unless a method uses each entry: exactly one call site apiece.
#[test]
fn every_operation_is_called_exactly_once() {
    let sources = [
        include_str!("../src/client.rs"),
        include_str!("../src/codes.rs"),
    ]
    .concat();
    for o in OPERATIONS {
        let needle = format!("&ops::{},", o.name);
        assert_eq!(sources.matches(&needle).count(), 1, "{needle}");
    }
}
