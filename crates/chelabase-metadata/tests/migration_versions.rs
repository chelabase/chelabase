//! Invariant: every metadata migration has its own version. Two files with one version (an
//! epic's migration and one that landed on `main` the same day) make the migrator apply one
//! and then refuse to start on any database that applied the other ("previously applied but
//! has been modified"). A branch's new migration takes a version after `main`'s latest.

use std::collections::BTreeMap;

#[test]
fn every_migration_has_its_own_version() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if !name.ends_with(".sql") {
            continue;
        }
        let version = name.split('_').next().unwrap().to_owned();
        assert!(
            version.len() == 14 && version.bytes().all(|b| b.is_ascii_digit()),
            "{name}: a migration's name starts with a 14-digit version"
        );
        seen.entry(version).or_default().push(name);
    }
    let shared: Vec<_> = seen.values().filter(|names| names.len() > 1).collect();
    assert!(
        shared.is_empty(),
        "migrations sharing a version: {shared:?}"
    );
}
