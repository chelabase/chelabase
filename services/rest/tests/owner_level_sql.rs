//! RT1 T4: `OWNER_LEVEL_SQL` has one definition, `chelabase_query::roles` (the realtime check
//! session needs the same question rest asks). rest imports it and keeps no copy.

const EXEC: &str = include_str!("../src/exec.rs");

#[test]
fn rest_keeps_no_copy_of_owner_level_sql() {
    assert!(
        !EXEC.contains("const OWNER_LEVEL_SQL"),
        "services/rest/src/exec.rs still defines OWNER_LEVEL_SQL"
    );
    assert!(
        !EXEC.contains("r.rolreplication"),
        "services/rest/src/exec.rs still holds the query text"
    );
}

#[test]
fn rest_imports_it_from_chelabase_query_roles() {
    let imported = EXEC.lines().any(|l| {
        let l = l.trim_start();
        (l.starts_with("use ") || l.starts_with("pub use "))
            && l.contains("chelabase_query")
            && l.contains("roles")
            && l.contains("OWNER_LEVEL_SQL")
    }) || EXEC.contains("chelabase_query::roles::OWNER_LEVEL_SQL");
    assert!(
        imported,
        "exec.rs doesn't take OWNER_LEVEL_SQL from chelabase_query::roles"
    );
}
