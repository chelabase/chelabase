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
    // A key's role may have BYPASSRLS: the first column doesn't read it.
    let (owner_level, _) = OWNER_LEVEL_SQL
        .split_once(", EXISTS (SELECT FROM pg_catalog.pg_roles x")
        .expect("the elevated column follows the owner-level one");
    assert!(!owner_level.contains("rolbypassrls"), "{owner_level}");
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

/// The second column is the worker's elevated rule (`member_roles::is_elevated`): the role or
/// one it is a member of can log in, has an owner-level attribute (BYPASSRLS included), holds
/// ADMIN or has a reserved name. The third is the elevated login's direct membership.
#[test]
fn owner_level_sql_answers_the_holding_login() {
    let (_, rest) = OWNER_LEVEL_SQL
        .split_once(", EXISTS (SELECT FROM pg_catalog.pg_roles x")
        .unwrap();
    let (elevated, held) = rest
        .split_once(", EXISTS (SELECT FROM pg_catalog.pg_auth_members m")
        .expect("the holding login's column follows the elevated one");
    for needle in [
        "pg_catalog.pg_has_role(r.oid, x.oid, 'MEMBER')",
        "x.rolsuper",
        "x.rolcreaterole",
        "x.rolcreatedb",
        "x.rolreplication",
        "x.rolbypassrls",
        "x.rolcanlogin",
        "a.admin_option AND a.member OPERATOR(pg_catalog.=) x.oid",
        "pg_catalog.starts_with(l, 'pg_')",
        "pg_catalog.starts_with(l, 'chelabase_')",
        "pg_catalog.starts_with(l, 'cb_m_')",
        "OPERATOR(pg_catalog.<>) ALL (ARRAY['anon', 'authenticated'])",
    ] {
        assert!(elevated.contains(needle), "{needle}: {elevated}");
    }
    // Every reserved name, the member-grant `owner` and the worker's owner role.
    for name in chelabase_types::role_name::RESERVED_ROLE_NAMES.iter().chain(&["owner", "postgres"]) {
        assert!(elevated.contains(&format!("'{name}'")), "{name}: {elevated}");
    }
    assert!(chelabase_types::role_name::is_member_role("cb_m_x"));
    assert!(chelabase_types::role_name::has_reserved_prefix("chelabase_x"));
    assert!(
        held.contains("m.roleid OPERATOR(pg_catalog.=) r.oid")
            && held.contains(&format!("'{}'", chelabase_query::roles::ELEVATED_LOGIN)),
        "{held}"
    );
}
