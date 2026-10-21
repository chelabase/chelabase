//! RT1 T6: realtime's check and probe SQL (`chelabase_query::realtime`), built from a relation
//! the catalog described (never from a request's names), its key-type allowlist (P6) and its
//! key strings (S11). The SQL's behaviour against Postgres is `realtime_live.rs`'s.

use chelabase_query::realtime::{
    KeyColumn, MAX_KEYS, Relation, check_sql, convert_sql, key_string, key_text_allowed,
    key_type_allowed, keys_supported, probe_sql,
};
use serde::Deserialize;

/// A column whose type is `schema.name` (not a domain).
fn col(name: &str, schema: &str, ty: &str, typtype: char) -> KeyColumn {
    KeyColumn {
        name: name.into(),
        type_schema: schema.into(),
        type_name: ty.into(),
        base_schema: schema.into(),
        base_name: ty.into(),
        base_typtype: typtype,
    }
}

fn builtin(name: &str, ty: &str) -> KeyColumn {
    col(name, "pg_catalog", ty, 'b')
}

/// A domain `schema.name` over `base` (in `pg_catalog`).
fn domain(name: &str, schema: &str, ty: &str, base: &str, base_typtype: char) -> KeyColumn {
    KeyColumn {
        name: name.into(),
        type_schema: schema.into(),
        type_name: ty.into(),
        base_schema: "pg_catalog".into(),
        base_name: base.into(),
        base_typtype,
    }
}

fn rel(schema: &str, name: &str, key: Vec<KeyColumn>) -> Relation {
    Relation {
        oid: 16_384,
        schema: schema.into(),
        name: name.into(),
        key,
    }
}

/// `(bigint, text)` on `public.notes`.
fn notes() -> Relation {
    rel(
        "public",
        "notes",
        vec![builtin("id", "int8"), builtin("tag", "text")],
    )
}

/// The `$n` placeholders the SQL uses, sorted and deduplicated.
fn params(sql: &str) -> Vec<usize> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start {
                out.push(sql[start..end].parse().unwrap());
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Spaces collapsed, so a test doesn't depend on the builder's layout.
fn squash(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every space removed, for calls whose argument spacing is the builder's choice.
fn compact(sql: &str) -> String {
    sql.split_whitespace().collect()
}

/// The SQL's shape with its `pg_catalog` qualifications read past (`pg_catalog.text`,
/// `"pg_catalog"."text"` and `text` all read `text`): the shape tests don't care how a name is
/// qualified; the qualification tests below do.
fn unqualified(sql: &str) -> String {
    sql.replace("\"pg_catalog\".", "")
        .replace("pg_catalog.", "")
        .replace("::\"text\"", "::text")
        .replace("::\"xid8\"", "::xid8")
}

/// [`squash`] after [`unqualified`].
fn shape(sql: &str) -> String {
    squash(&unqualified(sql))
}

/// [`compact`] after [`unqualified`].
fn shape_compact(sql: &str) -> String {
    compact(&unqualified(sql))
}

/// The end of the quoted identifier starting at `start` (a `"`), past its closing quote.
fn quoted_end(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b'"' {
            if b.get(i + 1) == Some(&b'"') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Grammar that looks like a call (`EXISTS (`, `JOIN (`, `OPERATOR(`…), not a function.
const GRAMMAR: &[&str] = &[
    "select", "from", "where", "exists", "in", "lateral", "operator", "values", "any", "all",
    "array", "row", "coalesce", "cast", "on", "and", "or", "not", "join", "as", "using", "limit",
];

/// Every function the SQL calls without `pg_catalog.` (quoted or not) before its name. Quoted
/// identifiers are skipped (a table named `x(` is no call); `AS k(…)` is an alias list.
fn unqualified_calls(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let name = if b[i] == b'"' {
            i = quoted_end(b, i);
            sql[start + 1..i - 1].replace("\"\"", "\"")
        } else if (b[i].is_ascii_alphabetic() || b[i] == b'_')
            && (i == 0 || !(is_ident(b[i - 1]) || b[i - 1] == b'$'))
        {
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            sql[start..i].to_string()
        } else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) != Some(&b'(') {
            continue;
        }
        let before = &sql[..start];
        let qualified = before.ends_with("pg_catalog.") || before.ends_with("\"pg_catalog\".");
        let alias = before.trim_end().to_ascii_lowercase().ends_with(" as");
        let grammar = b[start] != b'"' && GRAMMAR.contains(&name.to_ascii_lowercase().as_str());
        if !(qualified || alias || grammar) {
            out.push(name);
        }
    }
    out
}

/// Every cast (`::type`) whose type isn't schema-qualified (`::text`, `::oid`, `::xid8[]`…).
fn unqualified_casts(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'"' {
            i = quoted_end(b, i);
            continue;
        }
        if !(b[i] == b':' && b[i + 1] == b':') {
            i += 1;
            continue;
        }
        let mut j = i + 2;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let start = j;
        if b.get(j) == Some(&b'"') {
            j = quoted_end(b, j);
        } else {
            while j < b.len() && is_ident(b[j]) {
                j += 1;
            }
        }
        if b.get(j) != Some(&b'.') {
            out.push(sql[start..j].to_string());
        }
        i = j.max(i + 2);
    }
    out
}

/// The SQL with its quoted identifiers blanked, so an `=` inside a name doesn't count.
fn without_quoted(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            i = quoted_end(b, i);
            out.push_str("\"\"");
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// The `=` operators the SQL uses outside `OPERATOR(<schema>.=)` (the schema quoted or not),
/// and how many qualified ones it has.
fn equalities(sql: &str, schema: &str) -> (usize, usize) {
    let mut rest = compact(sql);
    let mut qualified = 0;
    for form in [
        format!("OPERATOR({schema}.=)"),
        format!("OPERATOR(\"{schema}\".=)"),
    ] {
        qualified += rest.matches(&form).count();
        rest = rest.replace(&form, "");
    }
    let rest = without_quoted(&rest);
    let bare = rest
        .char_indices()
        .filter(|&(k, ch)| {
            ch == '=' && !matches!(rest[..k].chars().last(), Some('<' | '>' | '!' | ':'))
        })
        .count();
    (bare, qualified)
}

/// A relation whose key exercises every type path: a built-in, an enum, a domain over text.
fn mixed() -> Relation {
    rel(
        "public",
        "mixed",
        vec![
            builtin("id", "int8"),
            col("mood", "app", "mood", 'e'),
            domain("email", "app", "email", "text", 'b'),
        ],
    )
}

// Every catalog function the check and the probe call is `pg_catalog.`-qualified: they run as
// the viewer's role, whose `search_path` may put its own schema first and shadow
// `row_security_active` (a bypass classification, so keys RLS hides), `unnest`,
// `pg_visible_in_snapshot` or `pg_current_snapshot`.
#[test]
fn every_function_the_sql_calls_is_pg_catalog_qualified() {
    for rel in [notes(), mixed()] {
        for (what, sql) in [
            ("check", check_sql(&rel, false)),
            ("check with an xid", check_sql(&rel, true)),
            ("probe", probe_sql(&rel)),
        ] {
            assert!(!sql.is_empty(), "{what}: no SQL built");
            let calls = unqualified_calls(&sql);
            assert!(
                calls.is_empty(),
                "{what} of {}: unqualified {calls:?} in {sql}",
                rel.name
            );
        }
    }
    // The scanner itself: it finds a bare call and passes qualified ones and aliases.
    assert_eq!(
        unqualified_calls(
            "SELECT unnest($1) AS k(a), pg_catalog.now(), \"pg_catalog\".\"lower\"(x)"
        ),
        ["unnest"]
    );
    assert_eq!(
        unqualified_calls("SELECT 1 FROM \"f(\" AS t"),
        Vec::<String>::new()
    );
}

// The same for types: `::text`, `::oid` or `::xid8` resolve through the `search_path` too.
#[test]
fn every_cast_names_its_types_schema() {
    for rel in [notes(), mixed()] {
        for (what, sql) in [
            ("check", check_sql(&rel, false)),
            ("check with an xid", check_sql(&rel, true)),
            ("probe", probe_sql(&rel)),
        ] {
            let casts = unqualified_casts(&sql);
            assert!(
                casts.is_empty(),
                "{what} of {}: unqualified casts {casts:?} in {sql}",
                rel.name
            );
        }
    }
    assert_eq!(
        unqualified_casts("x::text, y::pg_catalog.text, z::\"s\".\"t\", w::\"text\"[]"),
        ["text", "\"text\""]
    );
}

// Operators resolve through the `search_path` as well: a viewer's `evil.=` on the key type
// could match every row. Each key comparison names its operator's schema: `pg_catalog` for the
// built-ins, enums and domains over them.
#[test]
fn every_key_comparison_uses_a_qualified_operator() {
    for rel in [notes(), mixed()] {
        for with_xid in [false, true] {
            let sql = check_sql(&rel, with_xid);
            let (bare, qualified) = equalities(&sql, "pg_catalog");
            assert_eq!(bare, 0, "an unqualified = in {sql}");
            assert!(
                qualified >= rel.key.len(),
                "one OPERATOR(pg_catalog.=) per key column: {sql}"
            );
        }
    }
    // citext's equality is its own (case-insensitive), in the schema it was installed in.
    let ci = rel("public", "users", vec![col("email", "ext", "citext", 'b')]);
    let sql = check_sql(&ci, false);
    let (bare, qualified) = equalities(&sql, "ext");
    assert_eq!(
        (bare, qualified),
        (0, 1),
        "OPERATOR(\"ext\".=) for citext: {sql}"
    );
    // The probe compares nothing.
    assert_eq!(equalities(&probe_sql(&notes()), "pg_catalog").0, 0);
}

#[test]
fn the_check_unnests_one_text_array_per_key_column() {
    let sql = check_sql(&notes(), false);
    assert!(!sql.is_empty(), "no SQL built");
    let s = squash(&sql);
    assert!(
        rows_from_unnest(&sql).contains(&rows_from(2)),
        "one qualified unnest per key column, in order, zipped by ROWS FROM: {s}"
    );
    assert_eq!(
        params(&sql),
        vec![1, 2],
        "one parameter per key column, nothing else: {s}"
    );
    // Each key text is cast to its column's schema-qualified type (W27), never compared as text.
    assert!(s.contains(r#"::"pg_catalog"."int8""#), "{s}");
    assert!(s.contains(r#"::"pg_catalog"."text""#), "{s}");
    // The answer is the key columns as text, read from the qualified relation.
    assert!(s.contains(r#""public"."notes""#), "{s}");
    let u = shape(&sql);
    assert!(
        u.contains(r#""id"::text"#) && u.contains(r#""tag"::text"#),
        "{s}"
    );
    // One statement, so one snapshot.
    assert!(!sql.trim_end().trim_end_matches(';').contains(';'), "{s}");
}

#[test]
fn a_single_column_key_has_one_parameter() {
    let sql = check_sql(&rel("public", "t", vec![builtin("id", "uuid")]), false);
    let s = squash(&sql);
    assert!(rows_from_unnest(&sql).contains(&rows_from(1)), "{s}");
    assert!(s.contains(r#"::"pg_catalog"."uuid""#), "{s}");
    assert_eq!(params(&sql), vec![1]);
}

/// The SQL compacted and lower-cased with `"pg_catalog"` read as `pg_catalog`, for the
/// `ROWS FROM` check: qualified names stay qualified.
fn rows_from_unnest(sql: &str) -> String {
    compact(sql)
        .replace("\"pg_catalog\"", "pg_catalog")
        .replace("::pg_catalog.\"text\"", "::pg_catalog.text")
        .to_lowercase()
}

/// `ROWS FROM (pg_catalog.unnest($1::pg_catalog.text[]), …, pg_catalog.unnest($n::…))`,
/// compacted and lower-cased. A multi-argument `unnest(a, b)` exists only under its bare
/// name (the parser rewrites it into this form), and `pg_catalog.unnest(a, b)` doesn't exist
/// (42883), so the qualified check spells the rewrite out.
fn rows_from(n: usize) -> String {
    let calls: Vec<String> = (1..=n)
        .map(|i| format!("pg_catalog.unnest(${i}::pg_catalog.text[])"))
        .collect();
    format!("rowsfrom({})", calls.join(","))
}

// P7/W6: visibility is decided by the same statement's snapshot, the xid compared as xid8.
#[test]
fn with_an_xid_the_check_answers_visibility_in_the_same_statement() {
    let sql = check_sql(&notes(), true);
    let s = squash(&sql);
    assert_eq!(
        params(&sql),
        vec![1, 2, 3],
        "the xid is the parameter after the keys: {s}"
    );
    // Read past the qualification (`pg_catalog.pg_visible_in_snapshot($3::pg_catalog.xid8,
    // pg_catalog.pg_current_snapshot())`), which `every_function_the_sql_calls_…` requires.
    let c = shape_compact(&sql);
    let plain = "pg_visible_in_snapshot($3::xid8,pg_current_snapshot())";
    let via_text = "pg_visible_in_snapshot($3::text::xid8,pg_current_snapshot())";
    assert!(c.contains(plain) || c.contains(via_text), "{s}");
    assert!(
        !s.contains("pg_xact_status"),
        "W6: pg_xact_status says committed too early: {s}"
    );
    assert!(!sql.trim_end().trim_end_matches(';').contains(';'), "{s}");
    // Without one, no visibility clause and no third parameter.
    let without = check_sql(&notes(), false);
    assert!(!without.contains("pg_visible_in_snapshot"), "{without}");
}

// S11: keys are built in Rust; the SQL returns texts, never a Postgres JSON value (whose
// spacing differs from `JSON.stringify`).
#[test]
fn the_check_returns_texts_not_json() {
    let s = squash(&check_sql(&notes(), true)).to_lowercase();
    assert!(!s.is_empty());
    for json in [
        "to_jsonb",
        "to_json(",
        "json_build_array",
        "jsonb_build_array",
        "row_to_json",
        "json_agg",
    ] {
        assert!(!s.contains(json), "{json} in {s}");
    }
}

// Names come from the catalog and are always quoted (W27): a name with quotes, spaces, upper
// case or SQL in it stays one identifier.
#[test]
fn every_name_is_quoted() {
    let weird = Relation {
        oid: 1,
        schema: r#"we"ird"#.into(),
        name: "Ta ble; DROP TABLE x --".into(),
        key: vec![
            col("Id", r#"my"types"#, "Mood", 'e'),
            builtin(r#"a"b"#, "int4"),
        ],
    };
    let s = squash(&check_sql(&weird, true));
    assert!(s.contains(r#""we""ird"."Ta ble; DROP TABLE x --""#), "{s}");
    let u = shape(&check_sql(&weird, true));
    assert!(u.contains(r#""Id"::text"#), "{s}");
    assert!(u.contains(r#""a""b"::text"#), "{s}");
    assert!(s.contains(r#"::"my""types"."Mood""#), "{s}");
    let p = squash(&probe_sql(&weird));
    assert!(p.contains(r#""we""ird"."Ta ble; DROP TABLE x --""#), "{p}");
}

// W2: the probe is a real read of the key columns (schema USAGE, table and column SELECT),
// plus row security.
#[test]
fn the_probe_is_a_real_limit_0_read() {
    let sql = probe_sql(&notes());
    let s = shape(&sql);
    assert!(s.contains("LIMIT 0"), "{s}");
    assert!(s.contains(r#""public"."notes""#), "{s}");
    assert!(
        s.contains(r#""id"::text"#) && s.contains(r#""tag"::text"#),
        "{s}"
    );
    assert!(s.contains("row_security_active"), "{s}");
    assert!(
        !s.contains("has_table_privilege"),
        "W2: a privilege function misses schema USAGE: {s}"
    );
}

#[test]
fn the_allowlist_takes_exactly_p6s_types() {
    for ty in [
        "bool",
        "int2",
        "int4",
        "int8",
        "numeric",
        "float4",
        "float8",
        "text",
        "varchar",
        "bpchar",
        "name",
        "uuid",
        "date",
        "timestamp",
        "timestamptz",
        "time",
        "interval",
        "bytea",
        "oid",
    ] {
        assert!(key_type_allowed(&builtin("k", ty)), "{ty} is on P6's list");
    }
    // citext is an extension type: wherever it was installed.
    assert!(key_type_allowed(&col("k", "public", "citext", 'b')));
    assert!(key_type_allowed(&col("k", "extensions", "citext", 'b')));
    // Any enum.
    assert!(key_type_allowed(&col("k", "app", "mood", 'e')));
    // Domains over these (domains over domains are resolved by the catalog read).
    assert!(key_type_allowed(&domain("k", "app", "email", "text", 'b')));
    assert!(key_type_allowed(&domain(
        "k", "app", "positive", "int8", 'b'
    )));
    assert!(key_type_allowed(&KeyColumn {
        base_schema: "app".into(),
        base_name: "mood".into(),
        ..domain("k", "app", "mood_d", "mood", 'e')
    }));
}

// W3: types whose input or output depends on settings, or that aren't plain scalars, make the
// relation `changed`.
#[test]
fn the_allowlist_refuses_everything_else() {
    for ty in [
        "money",
        "timetz",
        "json",
        "jsonb",
        "xml",
        "inet",
        "cidr",
        "macaddr",
        "point",
        "tsvector",
        "bit",
        "varbit",
        "_int4",
        "_text",
        "int4range",
        "char",
        "regclass",
        "xid",
        "pg_lsn",
    ] {
        assert!(
            !key_type_allowed(&builtin("k", ty)),
            "{ty} is not on P6's list"
        );
    }
    // A type of another schema that only shares a built-in's name.
    assert!(!key_type_allowed(&col("k", "public", "int8", 'b')));
    assert!(!key_type_allowed(&col("k", "public", "text", 'b')));
    // A composite or range named like an allowed type.
    assert!(!key_type_allowed(&col("k", "public", "citext", 'c')));
    assert!(!key_type_allowed(&col("k", "pg_catalog", "text", 'c')));
    // A domain over a refused type, even when the domain's own name looks allowed.
    assert!(!key_type_allowed(&domain("k", "app", "text", "money", 'b')));
    assert!(!key_type_allowed(&domain(
        "k", "app", "citext", "jsonb", 'b'
    )));
}

#[test]
fn a_relation_is_supported_only_with_a_key_of_allowed_types() {
    assert!(keys_supported(&notes()));
    assert!(
        !keys_supported(&rel("public", "nokey", vec![])),
        "no primary key"
    );
    assert!(!keys_supported(&rel(
        "public",
        "m",
        vec![builtin("id", "int8"), builtin("amount", "money")]
    )));
}

#[test]
fn a_nan_float_key_is_refused() {
    for ty in ["float4", "float8"] {
        let c = builtin("k", ty);
        for nan in ["NaN", "nan", "NAN"] {
            assert!(!key_text_allowed(&c, nan), "{ty} {nan}");
        }
        for ok in ["1.5", "-0", "Infinity", "-Infinity", "1e+300"] {
            assert!(key_text_allowed(&c, ok), "{ty} {ok}");
        }
    }
    // NaN is an ordinary text (or numeric) key.
    assert!(key_text_allowed(&builtin("k", "text"), "NaN"));
    assert!(key_text_allowed(&builtin("k", "int8"), "9007199254740993"));
}

#[test]
fn the_key_cap_is_1000() {
    assert_eq!(MAX_KEYS, 1000);
}

#[derive(Deserialize)]
struct Case {
    texts: Vec<String>,
    key: String,
}

// S11: the shared fixture (JavaScript's `JSON.stringify` of the texts).
#[test]
fn key_strings_match_the_shared_fixture() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text_keys.json");
    let cases: Vec<Case> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(cases.len() >= 15);
    for c in &cases {
        assert_eq!(key_string(&c.texts), c.key, "texts {:?}", c.texts);
    }
}

// --- RT1 T8: the conversion (`/visible`'s `convert: true`, owner deletes) ---

#[test]
fn the_conversion_is_qualified_like_the_check() {
    for rel in [notes(), mixed()] {
        let sql = convert_sql(&rel);
        assert!(!sql.is_empty(), "{}: no SQL built", rel.name);
        let calls = unqualified_calls(&sql);
        assert!(calls.is_empty(), "{}: unqualified {calls:?} in {sql}", rel.name);
        let casts = unqualified_casts(&sql);
        assert!(casts.is_empty(), "{}: unqualified casts {casts:?} in {sql}", rel.name);
        let (bare, _) = equalities(&sql, "pg_catalog");
        assert_eq!(bare, 0, "an unqualified = in {sql}");
        assert!(!sql.trim_end().trim_end_matches(';').contains(';'), "{sql}");
    }
}

#[test]
fn the_conversion_casts_each_text_through_its_columns_type_without_reading_the_table() {
    let sql = convert_sql(&notes());
    assert!(!sql.is_empty(), "no SQL built");
    let s = squash(&sql);
    assert!(
        rows_from_unnest(&sql).contains(&rows_from(2)),
        "one qualified unnest per key column, zipped by ROWS FROM: {s}"
    );
    assert_eq!(params(&sql), vec![1, 2], "one parameter per key column: {s}");
    assert!(
        s.to_uppercase().contains("WITH ORDINALITY") && s.to_uppercase().contains("ORDER BY"),
        "answers in the request's order: {s}"
    );
    // Cast to the column's schema-qualified type, then printed by the session.
    assert!(s.contains(r#"::"pg_catalog"."int8""#), "{s}");
    assert!(s.contains(r#"::"pg_catalog"."text""#), "{s}");
    let printed = s.replace("::pg_catalog.text[]", "").matches("::pg_catalog.text").count();
    assert!(printed >= 2, "each key column printed as pg_catalog.text: {s}");
    assert!(
        !s.contains(r#""notes""#),
        "deleted rows convert too: the table is never read: {s}"
    );
    let m = squash(&convert_sql(&mixed()));
    assert!(m.contains(r#"::"app"."mood""#) && m.contains(r#"::"app"."email""#), "{m}");
}
