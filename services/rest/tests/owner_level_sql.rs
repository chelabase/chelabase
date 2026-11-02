//! RT1 T4: `OWNER_LEVEL_SQL` has one definition, `chelabase_query::roles` (the realtime check
//! session needs the same question rest asks). rest asks it through `chelabase_pgsession`'s
//! owner-level check, which imports it; neither keeps a copy.

const EXEC: &str = include_str!("../src/exec.rs");
const OWNER_LEVEL: &str = include_str!("../../../crates/chelabase-pgsession/src/owner_level.rs");

#[test]
fn rest_keeps_no_copy_of_owner_level_sql() {
    for (path, source) in [
        ("services/rest/src/exec.rs", EXEC),
        ("crates/chelabase-pgsession/src/owner_level.rs", OWNER_LEVEL),
    ] {
        assert!(
            !source.contains("const OWNER_LEVEL_SQL"),
            "{path} still defines OWNER_LEVEL_SQL"
        );
        assert!(
            !source.contains("r.rolreplication"),
            "{path} still holds the query text"
        );
    }
}

#[test]
fn rest_imports_it_from_chelabase_query_roles() {
    let imported = OWNER_LEVEL.lines().any(|l| {
        let l = l.trim_start();
        (l.starts_with("use ") || l.starts_with("pub use "))
            && l.contains("chelabase_query")
            && l.contains("roles")
            && l.contains("OWNER_LEVEL_SQL")
    }) || OWNER_LEVEL.contains("chelabase_query::roles::OWNER_LEVEL_SQL");
    assert!(
        imported,
        "the owner-level check doesn't take OWNER_LEVEL_SQL from chelabase_query::roles"
    );
    assert!(
        EXEC.contains("owner_level::fetch_owner_level"),
        "exec.rs doesn't ask through chelabase_pgsession's owner-level check"
    );
}
