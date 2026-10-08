//! SQL text builders for the planner. Every generated identifier goes through `sql::ident` /
//! `sql::qualified` and every literal through `sql::literal`; definitions the catalog gives
//! (types, expressions, constraint and index parts) are Postgres's own normalized text and are
//! used as they are.

use crate::model::{
    Column, DefaultPrivilegeObject, Grant, Identity, IdentityGeneration, Index, Policy,
    PolicyCommand, RoleRef, SequenceParams,
};
use crate::sql::{ident, literal, qualified};

/// The owner role every merge script runs as, in its own session (E15: psql `-U`): objects
/// stay owner-owned and the owner's default privileges fire.
pub const OWNER_ROLE: &str = "postgres";

/// `expr` in parentheses, unless the whole text is already one parenthesized group.
pub(super) fn paren(expr: &str) -> String {
    let e = expr.trim();
    if e.starts_with('(') && closing_paren(e, 0) == Some(e.len() - 1) {
        e.to_string()
    } else {
        format!("({e})")
    }
}

/// The byte offset of the parenthesis closing the one at `open`, skipping quoted text.
pub(super) fn closing_paren(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == q {
                        if bytes.get(i + 1) == Some(&q) {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `text` (catalog definition text) with the identifiers in `renames` (old → new name)
/// replaced, so a definition the target prints before a column rename compares equal to the
/// one Postgres prints after it. Token-level (pg_query's scanner): string literals and other
/// tokens are left alone. Text that doesn't scan is returned unchanged.
pub(crate) fn rename_identifiers(
    text: &str,
    renames: &std::collections::HashMap<String, String>,
) -> String {
    use pg_query::protobuf::{KeywordKind, Token};
    if renames.is_empty() {
        return text.to_string();
    }
    let Ok(scan) = pg_query::scan(text) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for t in &scan.tokens {
        let (Ok(start), Ok(end)) = (usize::try_from(t.start), usize::try_from(t.end)) else {
            continue;
        };
        let Some(raw) = text.get(start..end) else {
            continue;
        };
        let is_ident = t.token == Token::Ident as i32
            || KeywordKind::try_from(t.keyword_kind).is_ok_and(|k| k != KeywordKind::NoKeyword);
        if !is_ident {
            continue;
        }
        let name = match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
            Some(q) => q.replace("\"\"", "\""),
            None => raw.to_string(),
        };
        if let Some(new) = renames.get(&name) {
            out.push_str(&text[last..start]);
            out.push_str(&ident(new));
            last = end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Whether `text` (catalog definition text) has one of `names` as an identifier token
/// (pg_query's scanner; string literals don't count). Conservative: a same-named identifier
/// of another table (a subquery) also counts. Text that doesn't scan counts as a mention.
pub(super) fn mentions_identifier(text: &str, names: &[&str]) -> bool {
    use pg_query::protobuf::{KeywordKind, Token};
    if names.is_empty() {
        return false;
    }
    let Ok(scan) = pg_query::scan(text) else {
        return true;
    };
    scan.tokens.iter().any(|t| {
        let is_ident = t.token == Token::Ident as i32
            || KeywordKind::try_from(t.keyword_kind).is_ok_and(|k| k != KeywordKind::NoKeyword);
        let (Ok(start), Ok(end)) = (usize::try_from(t.start), usize::try_from(t.end)) else {
            return false;
        };
        let Some(raw) = text.get(start..end).filter(|_| is_ident) else {
            return false;
        };
        let name = match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
            Some(q) => q.replace("\"\"", "\""),
            None => raw.to_lowercase(),
        };
        names.contains(&name.as_str())
    })
}

/// Functions a column default can call without `ADD COLUMN` rewriting the table: stable or
/// immutable ones Postgres evaluates once for the existing rows (T14 minor 15). Any other call
/// counts as volatile (conservative: the flag only warns). Only Postgres's own functions: the
/// catalog text names them unqualified or in `pg_catalog` (`search_path = pg_catalog`); a user
/// function of the same name elsewhere (`public.now()`) may be anything.
const NON_VOLATILE_FUNCTIONS: [&str; 25] = [
    "now",
    "timezone",
    "transaction_timestamp",
    "statement_timestamp",
    "lower",
    "upper",
    "length",
    "concat",
    "concat_ws",
    "btrim",
    "to_char",
    "to_jsonb",
    "to_json",
    "jsonb_build_object",
    "json_build_object",
    "jsonb_build_array",
    "json_build_array",
    "make_date",
    "make_interval",
    "date_trunc",
    "abs",
    "round",
    "floor",
    "ceil",
    "md5",
];

/// Whether a column default (catalog text) may be volatile, so `ADD COLUMN … DEFAULT` rewrites
/// the table: it calls a function outside [`NON_VOLATILE_FUNCTIONS`], or doesn't parse.
pub(super) fn volatile_default(expr: &str) -> bool {
    let Ok(parsed) = pg_query::parse(&format!("SELECT {expr}")) else {
        return true;
    };
    parsed.protobuf.nodes().into_iter().any(|(node, ..)| match node {
        pg_query::NodeRef::FuncCall(f) => {
            let parts: Vec<&str> = f
                .funcname
                .iter()
                .filter_map(|n| match &n.node {
                    Some(pg_query::NodeEnum::String(s)) => Some(s.sval.as_str()),
                    _ => None,
                })
                .collect();
            let builtin = match parts.as_slice() {
                [name] | ["pg_catalog", name] => NON_VOLATILE_FUNCTIONS.contains(name),
                _ => false,
            };
            !builtin
        }
        _ => false,
    })
}

/// A constraint definition without the ` NOT VALID` `pg_get_constraintdef` appends.
pub(super) fn strip_not_valid(def: &str) -> &str {
    def.trim().strip_suffix(" NOT VALID").unwrap_or(def.trim())
}

/// `PUBLIC` or the quoted role name.
pub(super) fn role_ref(r: &RoleRef) -> String {
    match r {
        RoleRef::Public => "PUBLIC".to_string(),
        RoleRef::Role { name, .. } => ident(name),
    }
}

/// A grantee's name for comparisons (`PUBLIC` can't be a role name: `public` is reserved).
pub(crate) fn grantee_name(r: &RoleRef) -> &str {
    match r {
        RoleRef::Public => "PUBLIC",
        RoleRef::Role { name, .. } => name,
    }
}

/// A privilege keyword from `aclexplode` (`SELECT`, `USAGE`, …): upper-case letters only.
pub(super) fn privilege(p: &str) -> Option<&str> {
    (!p.is_empty() && p.bytes().all(|b| b.is_ascii_uppercase())).then_some(p)
}

/// `k='v', …` for a `WITH (…)` / `SET (…)` list from `reloptions` text (`fillfactor=70`).
pub(super) fn reloptions(opts: &[String]) -> String {
    opts.iter()
        .map(|o| match o.split_once('=') {
            Some((k, v)) => format!("{}={}", option_key(k), literal(v)),
            None => option_key(o),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// An option name: plain when it's `[a-z0-9_.]`, quoted otherwise.
pub(super) fn option_key(k: &str) -> String {
    if !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
    {
        k.to_string()
    } else {
        ident(k)
    }
}

pub(super) fn option_name(o: &str) -> &str {
    o.split_once('=').map_or(o, |(k, _)| k)
}

/// `name type [COLLATE c] [GENERATED …] [DEFAULT …] [NOT NULL]`.
pub(super) fn column_def(c: &Column, schema: &str, with_not_null: bool) -> String {
    let mut s = format!("{} {}", ident(&c.name), c.data_type);
    if let Some(coll) = &c.collation {
        s.push_str(&format!(" COLLATE {coll}"));
    }
    if let Some(id) = &c.identity {
        s.push_str(&format!(" {}", identity_clause(id, schema)));
    }
    if let Some(expr) = &c.generated {
        s.push_str(&format!(" GENERATED ALWAYS AS {} STORED", paren(expr)));
    }
    if let Some(d) = &c.default {
        s.push_str(&format!(" DEFAULT {d}"));
    }
    if with_not_null && c.not_null {
        s.push_str(" NOT NULL");
    }
    s
}

pub(super) fn generation(g: IdentityGeneration) -> &'static str {
    match g {
        IdentityGeneration::Always => "ALWAYS",
        IdentityGeneration::ByDefault => "BY DEFAULT",
    }
}

/// `GENERATED … AS IDENTITY (SEQUENCE NAME s.n START WITH … NO CYCLE)`; the sequence's type
/// follows the column's.
pub(super) fn identity_clause(id: &Identity, schema: &str) -> String {
    let p = &id.params;
    format!(
        "GENERATED {} AS IDENTITY (SEQUENCE NAME {} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} CACHE {} {})",
        generation(id.generation),
        qualified(schema, &id.sequence_name),
        p.start,
        p.increment,
        p.min,
        p.max,
        p.cache,
        cycle(p.cycle),
    )
}

pub(super) fn cycle(c: bool) -> &'static str {
    if c { "CYCLE" } else { "NO CYCLE" }
}

/// `CREATE SEQUENCE`'s options.
pub(super) fn sequence_options(p: &SequenceParams) -> String {
    format!(
        "AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} CACHE {} {}",
        p.data_type,
        p.increment,
        p.min,
        p.max,
        p.start,
        p.cache,
        cycle(p.cycle)
    )
}

/// The options that differ, for `ALTER SEQUENCE` (`with_type`) or `ALTER COLUMN … SET …`.
pub(super) fn sequence_changes(
    t: &SequenceParams,
    m: &SequenceParams,
    with_type: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if with_type && t.data_type != m.data_type {
        out.push(format!("AS {}", m.data_type));
    }
    if t.increment != m.increment {
        out.push(format!("INCREMENT BY {}", m.increment));
    }
    if t.min != m.min {
        out.push(format!("MINVALUE {}", m.min));
    }
    if t.max != m.max {
        out.push(format!("MAXVALUE {}", m.max));
    }
    if t.start != m.start {
        out.push(format!("START WITH {}", m.start));
    }
    if t.cache != m.cache {
        out.push(format!("CACHE {}", m.cache));
    }
    if t.cycle != m.cycle {
        out.push(cycle(m.cycle).to_string());
    }
    out
}

/// `CREATE [UNIQUE] INDEX [CONCURRENTLY] name ON table USING m (keys) [INCLUDE (…)]
/// [NULLS NOT DISTINCT] [WITH (…)] [WHERE (…)]`.
pub(super) fn create_index(ix: &Index, name: &str, table_q: &str, concurrently: bool) -> String {
    let mut s = format!(
        "CREATE {}INDEX {}{} ON {} USING {} ({})",
        if ix.unique { "UNIQUE " } else { "" },
        if concurrently { "CONCURRENTLY " } else { "" },
        ident(name),
        table_q,
        ident(&ix.method),
        ix.keys.join(", ")
    );
    if !ix.include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", ix.include.join(", ")));
    }
    if ix.nulls_not_distinct {
        s.push_str(" NULLS NOT DISTINCT");
    }
    if !ix.options.is_empty() {
        s.push_str(&format!(" WITH ({})", reloptions(&ix.options)));
    }
    if let Some(p) = &ix.predicate {
        s.push_str(&format!(" WHERE {}", paren(p)));
    }
    s
}

/// The parts of a `UNIQUE …` constraint definition that its backing index needs:
/// `(nulls_not_distinct, "(cols)", Some("(include)"), Some("with options"))`.
pub(super) struct UniqueParts {
    pub nulls_not_distinct: bool,
    pub columns: String,
    pub include: Option<String>,
    pub with: Option<String>,
}

pub(super) fn unique_parts(def: &str) -> Option<UniqueParts> {
    let rest = strip_not_valid(def).strip_prefix("UNIQUE")?.trim_start();
    let (nulls_not_distinct, rest) = match rest.strip_prefix("NULLS NOT DISTINCT") {
        Some(r) => (true, r.trim_start()),
        None => (false, rest),
    };
    let close = closing_paren(rest, 0)?;
    let columns = rest[..=close].to_string();
    let mut rest = rest[close + 1..].trim_start();
    let mut include = None;
    if let Some(r) = rest.strip_prefix("INCLUDE") {
        let r = r.trim_start();
        let close = closing_paren(r, 0)?;
        include = Some(r[..=close].to_string());
        rest = r[close + 1..].trim_start();
    }
    let mut with = None;
    if let Some(r) = rest.strip_prefix("WITH") {
        let r = r.trim_start();
        let close = closing_paren(r, 0)?;
        with = Some(r[..=close].to_string());
    }
    Some(UniqueParts {
        nulls_not_distinct,
        columns,
        include,
        with,
    })
}

/// The concurrent unique index backing a new `UNIQUE` constraint (named as the constraint, so
/// `ADD CONSTRAINT … USING INDEX` keeps the name).
pub(super) fn unique_index_for(def: &str, name: &str, table_q: &str) -> Option<String> {
    let p = unique_parts(def)?;
    let mut s = format!(
        "CREATE UNIQUE INDEX CONCURRENTLY {} ON {table_q} USING btree {}",
        ident(name),
        p.columns
    );
    if let Some(i) = p.include {
        s.push_str(&format!(" INCLUDE {i}"));
    }
    if p.nulls_not_distinct {
        s.push_str(" NULLS NOT DISTINCT");
    }
    if let Some(w) = p.with {
        s.push_str(&format!(" WITH {w}"));
    }
    Some(s)
}

pub(super) fn command(c: PolicyCommand) -> &'static str {
    match c {
        PolicyCommand::All => "ALL",
        PolicyCommand::Select => "SELECT",
        PolicyCommand::Insert => "INSERT",
        PolicyCommand::Update => "UPDATE",
        PolicyCommand::Delete => "DELETE",
    }
}

pub(super) fn create_policy(p: &Policy, table_q: &str) -> String {
    let roles = if p.roles.is_empty() {
        "PUBLIC".to_string()
    } else {
        p.roles.iter().map(role_ref).collect::<Vec<_>>().join(", ")
    };
    let mut s = format!(
        "CREATE POLICY {} ON {table_q} AS {} FOR {} TO {roles}",
        ident(&p.name),
        if p.permissive {
            "PERMISSIVE"
        } else {
            "RESTRICTIVE"
        },
        command(p.command),
    );
    if let Some(u) = &p.using {
        s.push_str(&format!(" USING {}", paren(u)));
    }
    if let Some(w) = &p.with_check {
        s.push_str(&format!(" WITH CHECK {}", paren(w)));
    }
    s
}

/// `COMMENT ON <what> <target> IS '…' | NULL`.
pub(super) fn comment(what: &str, target: &str, text: &Option<String>) -> String {
    format!(
        "COMMENT ON {what} {target} IS {}",
        text.as_deref().map_or("NULL".to_string(), literal)
    )
}

pub(super) fn grant_sql(g: &Grant, on: &str, column: Option<&str>) -> Option<String> {
    let p = privilege(&g.privilege)?;
    Some(format!(
        "GRANT {p}{} ON {on} TO {}{}",
        column.map_or(String::new(), |c| format!(" ({})", ident(c))),
        role_ref(&g.grantee),
        if g.grantable {
            " WITH GRANT OPTION"
        } else {
            ""
        }
    ))
}

pub(super) fn revoke_sql(
    g: &Grant,
    on: &str,
    column: Option<&str>,
    option_only: bool,
) -> Option<String> {
    let p = privilege(&g.privilege)?;
    Some(format!(
        "REVOKE {}{p}{} ON {on} FROM {}",
        if option_only { "GRANT OPTION FOR " } else { "" },
        column.map_or(String::new(), |c| format!(" ({})", ident(c))),
        role_ref(&g.grantee),
    ))
}

pub(super) fn default_object(o: DefaultPrivilegeObject) -> &'static str {
    match o {
        DefaultPrivilegeObject::Tables => "TABLES",
        DefaultPrivilegeObject::Sequences => "SEQUENCES",
        DefaultPrivilegeObject::Functions => "FUNCTIONS",
        DefaultPrivilegeObject::Types => "TYPES",
        DefaultPrivilegeObject::Schemas => "SCHEMAS",
    }
}

/// `acldefault` for an object type: what a role gets on what it creates when no global
/// default-privilege row says otherwise.
pub(crate) fn hardwired_defaults(
    o: DefaultPrivilegeObject,
) -> (&'static [&'static str], &'static [&'static str]) {
    // (owner's privileges, PUBLIC's privileges)
    match o {
        DefaultPrivilegeObject::Tables => (
            &[
                "INSERT",
                "SELECT",
                "UPDATE",
                "DELETE",
                "TRUNCATE",
                "REFERENCES",
                "TRIGGER",
                "MAINTAIN",
            ],
            &[],
        ),
        DefaultPrivilegeObject::Sequences => (&["USAGE", "SELECT", "UPDATE"], &[]),
        DefaultPrivilegeObject::Functions => (&["EXECUTE"], &["EXECUTE"]),
        DefaultPrivilegeObject::Types => (&["USAGE"], &["USAGE"]),
        DefaultPrivilegeObject::Schemas => (&["USAGE", "CREATE"], &[]),
    }
}

/// `rolvaliduntil` as Postgres prints it (`2030-01-01 00:00:00+00`) in RFC 3339
/// (`2030-01-01T00:00:00+00:00`); `infinity` is no expiry. Text in another shape passes as is.
pub(super) fn rfc3339(pg: &str) -> Option<String> {
    let s = pg.trim();
    if s == "infinity" {
        return None;
    }
    let Some((date, rest)) = s.split_once(' ') else {
        return Some(s.to_string());
    };
    let Some(pos) = rest.rfind(['+', '-']) else {
        return Some(s.to_string());
    };
    let (time, offset) = rest.split_at(pos);
    let offset = match offset.len() {
        3 => format!("{offset}:00"),
        _ => offset.to_string(),
    };
    Some(format!("{date}T{time}{offset}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paren_wraps_only_when_needed() {
        assert_eq!(paren("(a = 1)"), "(a = 1)");
        assert_eq!(paren("a = 1"), "(a = 1)");
        assert_eq!(paren("(a) = (b)"), "((a) = (b))");
        assert_eq!(paren("f(')')"), "(f(')'))");
        assert_eq!(paren("(\")\" = 1)"), "(\")\" = 1)");
    }

    #[test]
    fn unique_parts_reads_postgres_definitions() {
        let p = unique_parts(
            "UNIQUE NULLS NOT DISTINCT (a, \"b)\") INCLUDE (c) WITH (fillfactor='70')",
        )
        .unwrap();
        assert!(p.nulls_not_distinct);
        assert_eq!(p.columns, "(a, \"b)\")");
        assert_eq!(p.include.as_deref(), Some("(c)"));
        assert_eq!(p.with.as_deref(), Some("(fillfactor='70')"));
        assert!(unique_parts("CHECK (a)").is_none());
    }

    #[test]
    fn rename_identifiers_replaces_names_not_strings() {
        let map: std::collections::HashMap<String, String> = [
            ("a".to_string(), "b".to_string()),
            ("Odd".to_string(), "my col".to_string()),
            ("name".to_string(), "label".to_string()),
        ]
        .into();
        assert_eq!(
            rename_identifiers("((a > 0) AND (\"Odd\" <> 'a') AND (name = aa))", &map),
            "((b > 0) AND (\"my col\" <> 'a') AND (label = aa))"
        );
        assert_eq!(
            rename_identifiers("lower(a)", &Default::default()),
            "lower(a)"
        );
    }

    #[test]
    fn valid_until_becomes_rfc3339() {
        assert_eq!(
            rfc3339("2030-01-01 00:00:00+00").as_deref(),
            Some("2030-01-01T00:00:00+00:00")
        );
        assert_eq!(
            rfc3339("2030-01-01 10:00:00.5-05:30").as_deref(),
            Some("2030-01-01T10:00:00.5-05:30")
        );
        assert_eq!(rfc3339("infinity"), None);
    }

    #[test]
    fn reloptions_quote_values() {
        assert_eq!(
            reloptions(&["fillfactor=70".into(), "Odd Key=x'y".into()]),
            "fillfactor='70', \"Odd Key\"='x''y'"
        );
    }
}
