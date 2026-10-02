//! The SDK's fixtures (`packages/client/fixtures/*.json`, each `{ "call", "tree" }`) are
//! the contract between the TypeScript builders and this parser: every tree must parse.

use std::path::Path;

#[test]
fn every_client_fixture_tree_parses() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/client/fixtures");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let fixture: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(fixture["call"].is_string(), "{}: no `call`", path.display());
        let tree = serde_json::to_vec(&fixture["tree"]).unwrap();
        if let Err(e) = cheladb_query::query::parse(&tree) {
            panic!("{}: {e}", path.display());
        }
        count += 1;
    }
    assert!(count >= 1, "no fixtures in {}", dir.display());
}
