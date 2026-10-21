//! RT1 T4: `OWNER_LEVEL_SQL` lives in `chelabase_query::roles` (rest imports it; T6's check
//! session uses it too). It keeps every guarantee rest's own unit test pinned for it.

use chelabase_query::roles::OWNER_LEVEL_SQL;

#[test]
fn owner_level_sql_is_pinned() {
    for needle in [
        "r.rolsuper",
        "r.rolcreaterole",
        "r.rolcreatedb",
        "r.rolreplication",
        "pg_catalog.pg_has_role(r.oid, s.oid, 'MEMBER')",
        "FROM pg_catalog.pg_roles r WHERE r.rolname = $1",
    ] {
        assert!(
            OWNER_LEVEL_SQL.contains(needle),
            "{needle}: {OWNER_LEVEL_SQL}"
        );
    }
    // A key's role may have BYPASSRLS.
    assert!(
        !OWNER_LEVEL_SQL.contains("rolbypassrls"),
        "{OWNER_LEVEL_SQL}"
    );
    // A member, at any depth, of a superuser role or of each refused group.
    assert!(OWNER_LEVEL_SQL.contains("s.rolsuper"), "{OWNER_LEVEL_SQL}");
    for group in chelabase_types::data_key::KEY_ROLE_REFUSED_GROUPS {
        assert!(
            OWNER_LEVEL_SQL.contains(&format!("'{group}'")),
            "{group}: {OWNER_LEVEL_SQL}"
        );
    }
    // Only the schema-qualified function: a `search_path` can't shadow it.
    assert_eq!(
        OWNER_LEVEL_SQL.matches("pg_has_role(").count(),
        OWNER_LEVEL_SQL.matches("pg_catalog.pg_has_role(").count(),
        "{OWNER_LEVEL_SQL}"
    );
}
