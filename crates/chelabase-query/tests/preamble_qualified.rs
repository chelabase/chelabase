//! RT1 final fix round (batch B, T6 hardening): the shared query preambles run before (or while)
//! the role is set, under the login role's `search_path`, which a database-level
//! `ALTER DATABASE … SET search_path` controls. Every function, cast and operator they use is
//! named by schema (`pg_catalog.` or the machinery's `chelabase.`), so nothing in a schema the
//! database's owner controls can shadow them.

use chelabase_query::engine::{
    GUARDED_PREAMBLE_SQL, OWNER_GUARDED_PREAMBLE_SQL, OWNER_PREAMBLE_SQL, PREAMBLE_SQL,
};

/// Words that are grammar, not functions, when an opening parenthesis follows them.
const GRAMMAR: &[&str] = &["OPERATOR", "ANY", "EXISTS", "IN", "COALESCE", "VALUES"];

/// Every unqualified function call, cast and operator in `sql`.
fn unqualified(sql: &str) -> Vec<String> {
    let mut found = Vec::new();
    let b = sql.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let qualified_before = |start: usize| {
        ["pg_catalog.", "chelabase."]
            .iter()
            .any(|q| sql[..start].ends_with(q))
    };
    let mut i = 0;
    while i < b.len() {
        // A call: `name(`.
        if b[i] == b'(' && i > 0 && word(b[i - 1]) {
            let mut start = i;
            while start > 0 && word(b[start - 1]) {
                start -= 1;
            }
            let name = &sql[start..i];
            if !GRAMMAR.iter().any(|g| g.eq_ignore_ascii_case(name)) && !qualified_before(start) {
                found.push(format!("call {name}("));
            }
        }
        // A cast: `::type`.
        if b[i] == b':' && b.get(i + 1) == Some(&b':') {
            let rest = &sql[i + 2..];
            if !rest.starts_with("pg_catalog.") {
                let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
                found.push(format!("cast ::{}", &rest[..end.unwrap_or(rest.len())]));
            }
            i += 2;
            continue;
        }
        // An operator: `=` outside `OPERATOR(pg_catalog.=)`.
        if b[i] == b'=' && !sql[..i].ends_with("OPERATOR(pg_catalog.") {
            found.push(format!("operator = at {i}"));
        }
        i += 1;
    }
    found
}

#[test]
fn the_scanner_finds_what_it_should() {
    let bare = "SELECT set_config('a', $1::int8::text, true) WHERE a = b";
    assert_eq!(
        unqualified(bare),
        vec![
            "call set_config(".to_owned(),
            "cast ::int8".to_owned(),
            "cast ::text".to_owned(),
            format!("operator = at {}", bare.find(" = ").unwrap() + 1),
        ]
    );
    assert!(
        unqualified(
            "SELECT pg_catalog.set_config('a', $1::pg_catalog.int8, true), \
             chelabase.assert_schema_version($2) WHERE a OPERATOR(pg_catalog.=) b \
             AND EXISTS (SELECT 1) AND x = ANY ($3)"
        )
        .iter()
        .all(|f| f.starts_with("operator")),
        "only the bare `=` before ANY is reported"
    );
}

#[test]
fn every_preamble_is_qualified() {
    for (name, sql) in [
        ("PREAMBLE_SQL", PREAMBLE_SQL),
        ("OWNER_PREAMBLE_SQL", OWNER_PREAMBLE_SQL),
        ("GUARDED_PREAMBLE_SQL", GUARDED_PREAMBLE_SQL),
        ("OWNER_GUARDED_PREAMBLE_SQL", OWNER_GUARDED_PREAMBLE_SQL),
    ] {
        let found = unqualified(sql);
        assert!(found.is_empty(), "{name}: {found:?}\n{sql}");
    }
}
