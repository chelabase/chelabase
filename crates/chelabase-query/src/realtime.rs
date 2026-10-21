//! Realtime's checks (RT1 T6, reused by T8): which primary keys of a relation a session may
//! see, read in that session (the console's, P6/W3), by the relation's OID with the keys bound
//! as `text[]` parameters (W27), and whether a decoded transaction is visible to the same
//! statement's snapshot (P7/W6).
//!
//! Every identifier in the SQL comes from the catalog ([`load_relation`]) and is quoted; the
//! request supplies only the OID, the key texts and the xid, all bound as parameters.

use std::fmt::Write as _;

use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

/// Keys one check carries at most (a window with more is a keyless `changed`).
pub const MAX_KEYS: usize = 1000;

/// A primary-key column, as [`load_relation`] reads it from the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyColumn {
    /// `pg_attribute.attname`.
    pub name: String,
    /// The column's type (the cast target): its schema and `typname`.
    pub type_schema: String,
    pub type_name: String,
    /// What the type resolves to through domains (over domains): its schema, `typname` and
    /// `typtype` (`'b'` base, `'e'` enum, `'c'` composite, `'r'` range, …); the type itself
    /// when it isn't a domain.
    pub base_schema: String,
    pub base_name: String,
    pub base_typtype: char,
}

/// A relation and its primary key, in key order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub key: Vec<KeyColumn>,
}

/// The privilege probe's answer: whether row security applies to the session for the relation
/// (`bypass` = it doesn't).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub bypass: bool,
}

/// A check's answer: whether the transaction was visible to the statement's snapshot, and the
/// key strings ([`key_string`]) of the rows the session sees (empty when not visible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub visible: bool,
    pub keys: Vec<String>,
}

/// P6's built-in base types (`pg_catalog`): their text input and output don't depend on the
/// session's settings in the forms the decoder emits (ISO dates and times with `+00`
/// offsets, ISO 8601 intervals, hex `bytea`, shortest-exact floats).
const BUILTIN_KEY_TYPES: &[&str] = &[
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
];

/// P6: whether a key column's type is on the allowlist. Decided by what the type resolves to
/// through domains: a built-in base type of `pg_catalog` on the list, `citext` (an extension
/// type, wherever it was installed), or any enum.
pub fn key_type_allowed(col: &KeyColumn) -> bool {
    match col.base_typtype {
        'e' => true,
        'b' => {
            (col.base_schema == "pg_catalog" && BUILTIN_KEY_TYPES.contains(&col.base_name.as_str()))
                || col.base_name == "citext"
        }
        _ => false,
    }
}

/// P6: whether a decoded key text may be cast for this column (a float `NaN` may not: it never
/// equals itself under SQL's `=` on the client side, so a NaN key can't be matched reliably).
///
/// Every text Postgres's float input reads as NaN counts: `float8in` skips white space and
/// hands the text to `strtod`, which takes an optional sign and `nan` in any case, optionally
/// followed by `(chars)` (`-NaN`, `+nan`, `nan(1)`).
pub fn key_text_allowed(col: &KeyColumn, text: &str) -> bool {
    let float = col.base_typtype == 'b'
        && col.base_schema == "pg_catalog"
        && matches!(col.base_name.as_str(), "float4" | "float8");
    if !float {
        return true;
    }
    let unsigned = text.trim();
    let unsigned = unsigned.strip_prefix(['+', '-']).unwrap_or(unsigned);
    !unsigned.get(..3).is_some_and(|head| head.eq_ignore_ascii_case("nan"))
}

/// Whether the relation's keys can be checked at all: a primary key, every column allowed.
pub fn keys_supported(rel: &Relation) -> bool {
    !rel.key.is_empty() && rel.key.iter().all(key_type_allowed)
}

/// `"name"` with embedded quotes doubled.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn qualified(rel: &Relation) -> String {
    format!("{}.{}", ident(&rel.schema), ident(&rel.name))
}

/// `t."a"::pg_catalog.text, …` in key order.
fn key_texts(rel: &Relation) -> String {
    rel.key
        .iter()
        .map(|c| format!("t.{}::pg_catalog.text", ident(&c.name)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The schema of the `=` a key column is compared with: `pg_catalog` for the built-ins, enums
/// and domains over them; citext's own (case-insensitive) one in the schema it was installed in.
/// Named, never found through the session's `search_path`, which the viewer's role controls.
fn equality_schema(c: &KeyColumn) -> String {
    if c.base_typtype == 'b' && c.base_schema != "pg_catalog" && c.base_name == "citext" {
        ident(&c.base_schema)
    } else {
        "pg_catalog".to_string()
    }
}

/// The probe: a real read of the key columns as text `LIMIT 0` (schema `USAGE`, table and
/// column privileges), plus `row_security_active`. One statement; its one row is
/// `row_security_active` (the read is joined in, so its privileges are checked even though it
/// returns nothing).
pub fn probe_sql(rel: &Relation) -> String {
    let read = if rel.key.is_empty() {
        "1".to_string()
    } else {
        key_texts(rel)
    };
    format!(
        "SELECT pg_catalog.row_security_active({oid}::pg_catalog.oid) AS rls \
         FROM (SELECT 1) AS one \
         LEFT JOIN (SELECT {read} FROM {table} AS t LIMIT 0) AS probe ON true",
        oid = rel.oid,
        table = qualified(rel),
    )
}

/// The check: the key columns as text of the rows matching the keys bound as `$1..$n` (one
/// `text[]` per key column, through `unnest`, each cast to its schema-qualified type); with
/// `with_xid`, `$n+1` is the transaction id and the statement also answers
/// `pg_visible_in_snapshot($n+1::xid8, pg_current_snapshot())`.
///
/// The answer's first column is `visible` (`true` without an xid); the rest are the key
/// texts, `NULL` on the single row a check that matched nothing (or wasn't visible) answers
/// with an xid.
pub fn check_sql(rel: &Relation, with_xid: bool) -> String {
    let n = rel.key.len();
    // `ROWS FROM (unnest(a), unnest(b))` is what a multi-argument `unnest(a, b)` means; that
    // form exists only unqualified (the parser rewrites the bare name), and a qualified
    // `pg_catalog.unnest(a, b)` doesn't exist. Each array zips by position, shorter ones
    // padded with NULLs.
    let arrays = (1..=n)
        .map(|i| format!("pg_catalog.unnest(${i}::pg_catalog.text[])"))
        .collect::<Vec<_>>()
        .join(", ");
    let aliases = (1..=n)
        .map(|i| format!("k{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut matches = String::new();
    for (i, c) in rel.key.iter().enumerate() {
        if i > 0 {
            matches.push_str(" AND ");
        }
        let _ = write!(
            matches,
            "t.{col} OPERATOR({op}.=) k.k{n}::{schema}.{ty}",
            col = ident(&c.name),
            op = equality_schema(c),
            n = i + 1,
            schema = ident(&c.type_schema),
            ty = ident(&c.type_name),
        );
    }
    let rows = format!(
        "SELECT {texts} FROM {table} AS t \
         WHERE EXISTS (SELECT 1 FROM ROWS FROM ({arrays}) AS k({aliases}) WHERE {matches})",
        texts = key_texts(rel),
        table = qualified(rel),
    );
    if !with_xid {
        return format!("SELECT true AS visible, r.* FROM ({rows}) AS r");
    }
    let xid = n + 1;
    format!(
        "SELECT v.visible, r.* \
         FROM (SELECT pg_catalog.pg_visible_in_snapshot(${xid}::pg_catalog.text::pg_catalog.xid8, \
                      pg_catalog.pg_current_snapshot()) AS visible) AS v \
         LEFT JOIN LATERAL ({rows}) AS r ON v.visible"
    )
}

/// The conversion (RT1 T8, `/visible`'s `convert: true`): each key's texts as the caller's
/// session prints them, without reading the relation (deleted rows convert too). The keys are
/// bound as `$1..$n`, one `text[]` per key column zipped by `ROWS FROM` of qualified `unnest`s
/// `WITH ORDINALITY`; each element is cast to its column's schema-qualified type, then to
/// `pg_catalog.text`. One row per key, in the request's order (`ORDER BY` the ordinality),
/// with the key column texts in key order as each row's last n columns (`text`). A text the type can't read is an error (fails
/// closed). Every function, cast and operator is `pg_catalog`-qualified, like the check.
pub fn convert_sql(rel: &Relation) -> String {
    let n = rel.key.len();
    let arrays = (1..=n)
        .map(|i| format!("pg_catalog.unnest(${i}::pg_catalog.text[])"))
        .collect::<Vec<_>>()
        .join(", ");
    let aliases = (1..=n)
        .map(|i| format!("k{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let printed = rel
        .key
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "(k.k{n}::{schema}.{ty})::pg_catalog.text",
                n = i + 1,
                schema = ident(&c.type_schema),
                ty = ident(&c.type_name),
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT {printed} \
         FROM ROWS FROM ({arrays}) WITH ORDINALITY AS k({aliases}, ord) \
         ORDER BY k.ord"
    )
}

/// Runs [`convert_sql`] in the caller's session: each key row's key string ([`key_string`]) as
/// the session prints it, in order. A text the column's type can't read is an error.
pub async fn convert(
    client: &Client,
    rel: &Relation,
    keys: &[Vec<String>],
) -> Result<Vec<String>, tokio_postgres::Error> {
    let n = rel.key.len();
    let mut columns: Vec<Vec<Option<&str>>> = vec![Vec::with_capacity(keys.len()); n];
    for row in keys {
        for (i, column) in columns.iter_mut().enumerate() {
            column.push(row.get(i).map(String::as_str));
        }
    }
    let params: Vec<&(dyn ToSql + Sync)> = columns
        .iter()
        .map(|c| c as &(dyn ToSql + Sync))
        .collect();
    let rows = client.query(&convert_sql(rel), &params).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        // A missing text (NULL) prints as NULL: no key string for it.
        let texts: Option<Vec<String>> = (0..n).map(|i| row.get::<_, Option<String>>(i)).collect();
        match texts {
            Some(texts) => out.push(key_string(&texts)),
            None => out.push(String::new()),
        }
    }
    Ok(out)
}

/// S11: a row's key string, `serde_json::to_string` of its key columns' texts (byte-identical
/// to JavaScript's `JSON.stringify` of the same strings).
pub fn key_string(texts: &[String]) -> String {
    serde_json::to_string(texts).expect("a list of strings always serializes")
}

/// The relation with this OID and its primary key, read in the caller's session (`None` when
/// there is none). A relation without a primary key comes back with an empty `key`.
pub async fn load_relation(
    client: &Client,
    rel_oid: u32,
) -> Result<Option<Relation>, tokio_postgres::Error> {
    let Some(row) = client
        .query_opt(
            "SELECT n.nspname::pg_catalog.text, c.relname::pg_catalog.text \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
             WHERE c.oid OPERATOR(pg_catalog.=) $1",
            &[&rel_oid],
        )
        .await?
    else {
        return Ok(None);
    };
    let (schema, name): (String, String) = (row.get(0), row.get(1));
    // The key columns in index order (not column order), each type and the base type it
    // resolves to through domains (over domains).
    let rows = client
        .query(
            "WITH RECURSIVE keycols AS ( \
                 SELECT k.ord, a.attname, a.atttypid \
                 FROM pg_catalog.pg_index i \
                 CROSS JOIN LATERAL pg_catalog.unnest(i.indkey::pg_catalog.int2[]) \
                     WITH ORDINALITY AS k(attnum, ord) \
                 JOIN pg_catalog.pg_attribute a \
                   ON a.attrelid OPERATOR(pg_catalog.=) i.indrelid \
                  AND a.attnum OPERATOR(pg_catalog.=) k.attnum \
                 WHERE i.indrelid OPERATOR(pg_catalog.=) $1 AND i.indisprimary \
                   AND k.ord OPERATOR(pg_catalog.<=) i.indnkeyatts \
             ), d(ord, typ, depth) AS ( \
                 SELECT ord, atttypid, 0 FROM keycols \
                 UNION ALL \
                 SELECT d.ord, dt.typbasetype, d.depth OPERATOR(pg_catalog.+) 1 \
                 FROM d JOIN pg_catalog.pg_type dt ON dt.oid OPERATOR(pg_catalog.=) d.typ \
                 WHERE dt.typtype OPERATOR(pg_catalog.=) 'd'::pg_catalog.\"char\" \
                   AND d.depth OPERATOR(pg_catalog.<) 64 \
             ), base AS ( \
                 SELECT DISTINCT ON (ord) ord, typ FROM d ORDER BY ord, depth DESC \
             ) \
             SELECT kc.attname::pg_catalog.text, tn.nspname::pg_catalog.text, \
                    ty.typname::pg_catalog.text, bn.nspname::pg_catalog.text, \
                    b.typname::pg_catalog.text, b.typtype::pg_catalog.text \
             FROM keycols kc \
             JOIN base ON base.ord OPERATOR(pg_catalog.=) kc.ord \
             JOIN pg_catalog.pg_type ty ON ty.oid OPERATOR(pg_catalog.=) kc.atttypid \
             JOIN pg_catalog.pg_namespace tn ON tn.oid OPERATOR(pg_catalog.=) ty.typnamespace \
             JOIN pg_catalog.pg_type b ON b.oid OPERATOR(pg_catalog.=) base.typ \
             JOIN pg_catalog.pg_namespace bn ON bn.oid OPERATOR(pg_catalog.=) b.typnamespace \
             ORDER BY kc.ord",
            &[&rel_oid],
        )
        .await?;
    let key = rows
        .iter()
        .map(|r| {
            let typtype: String = r.get(5);
            KeyColumn {
                name: r.get(0),
                type_schema: r.get(1),
                type_name: r.get(2),
                base_schema: r.get(3),
                base_name: r.get(4),
                base_typtype: typtype.chars().next().unwrap_or('?'),
            }
        })
        .collect();
    Ok(Some(Relation { oid: rel_oid, schema, name, key }))
}

/// Runs [`probe_sql`] in the caller's session.
pub async fn probe(client: &Client, rel: &Relation) -> Result<Probe, tokio_postgres::Error> {
    let row = client.query_one(&probe_sql(rel), &[]).await?;
    let rls: bool = row.get(0);
    Ok(Probe { bypass: !rls })
}

/// Runs [`check_sql`] in the caller's session; `keys` are rows of key texts in key order.
///
/// Every row should have one text per key column (the orchestrator refuses others with 400); a
/// missing text is bound as `NULL`, which matches no row.
pub async fn check(
    client: &Client,
    rel: &Relation,
    xid: Option<u64>,
    keys: &[Vec<String>],
) -> Result<Checked, tokio_postgres::Error> {
    let n = rel.key.len();
    // Column-major: one array per key column.
    let mut columns: Vec<Vec<Option<&str>>> = vec![Vec::with_capacity(keys.len()); n];
    for row in keys {
        for (i, column) in columns.iter_mut().enumerate() {
            column.push(row.get(i).map(String::as_str));
        }
    }
    let xid_text = xid.map(|x| x.to_string());
    let mut params: Vec<&(dyn ToSql + Sync)> = columns
        .iter()
        .map(|c| c as &(dyn ToSql + Sync))
        .collect();
    if let Some(x) = &xid_text {
        params.push(x);
    }
    let rows = client.query(&check_sql(rel, xid.is_some()), &params).await?;
    let mut visible = true;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        visible = row.get(0);
        if !visible {
            break;
        }
        let texts: Option<Vec<String>> = (1..=n).map(|i| row.get::<_, Option<String>>(i)).collect();
        // The single all-`NULL` row of a visible check that matched nothing.
        if let Some(texts) = texts {
            out.push(key_string(&texts));
        }
    }
    if !visible {
        out.clear();
    }
    Ok(Checked { visible, keys: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn float(ty: &str) -> KeyColumn {
        KeyColumn {
            name: "k".into(),
            type_schema: "pg_catalog".into(),
            type_name: ty.into(),
            base_schema: "pg_catalog".into(),
            base_name: ty.into(),
            base_typtype: 'b',
        }
    }

    // Every spelling `float8in` reads as NaN is refused, not only `nan` itself.
    #[test]
    fn every_nan_spelling_is_refused() {
        for ty in ["float4", "float8"] {
            let c = float(ty);
            for nan in ["-NaN", "+nan", " nan ", "\tNAN\n", "nan(1)", "-nan(0x7ff)"] {
                assert!(!key_text_allowed(&c, nan), "{ty} {nan:?}");
            }
            for ok in ["-Infinity", "+inf", "-0", "1e-5", "", "-", "na", "ñan"] {
                assert!(key_text_allowed(&c, ok), "{ty} {ok:?}");
            }
        }
    }
}
