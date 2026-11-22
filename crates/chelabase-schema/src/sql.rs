//! Identifier and literal quoting for generated SQL, with Postgres's `quote_ident` and
//! `quote_literal` rules.

/// `quote_ident`: unquoted only when the name is `[a-z_][a-z0-9_$]*` and not a keyword other
/// than an unreserved one; otherwise double-quoted with inner `"` doubled.
pub fn ident(s: &str) -> String {
    if is_plain(s) && !is_keyword(s) {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `[a-z_][a-z0-9_$]*`.
fn is_plain(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'_'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$'))
}

/// A keyword `quote_ident` quotes: any kind but unreserved. Called only for plain names, which
/// scan as exactly one token.
fn is_keyword(s: &str) -> bool {
    use pg_query::protobuf::KeywordKind;
    let Ok(scan) = pg_query::scan(s) else {
        // A plain name always scans; quoting is the safe answer if it ever doesn't.
        return true;
    };
    scan.tokens.iter().any(|t| {
        !matches!(
            KeywordKind::try_from(t.keyword_kind),
            Ok(KeywordKind::NoKeyword | KeywordKind::UnreservedKeyword)
        )
    })
}

/// `schema.name`, each part through [`ident`].
pub fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", ident(schema), ident(name))
}

/// `quote_literal`: single-quoted with `'` doubled; a string with a backslash gets the `E`
/// prefix and its backslashes doubled, so it reads the same whatever
/// `standard_conforming_strings` is. The one implementation is
/// [`chelabase_types::role_name::quote_literal`].
pub fn literal(s: &str) -> String {
    chelabase_types::role_name::quote_literal(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ident_quotes_when_needed() {
        assert_eq!(ident("users"), "users");
        assert_eq!(ident("_t1$x"), "_t1$x");
        assert_eq!(ident("Order"), "\"Order\"");
        assert_eq!(ident("my table"), "\"my table\"");
        assert_eq!(ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(ident("select"), "\"select\"");
        // Column-name and type-function-name keywords are quoted, unreserved ones aren't
        // (as `quote_ident` does).
        assert_eq!(ident("between"), "\"between\"");
        assert_eq!(ident("left"), "\"left\"");
        assert_eq!(ident("name"), "name");
        assert_eq!(ident("comment"), "comment");
        assert_eq!(ident("1abc"), "\"1abc\"");
        assert_eq!(ident(""), "\"\"");
        assert_eq!(ident("é"), "\"é\"");
    }

    #[test]
    fn qualified_quotes_each_part() {
        assert_eq!(qualified("public", "users"), "public.users");
        assert_eq!(qualified("Billing", "my table"), "\"Billing\".\"my table\"");
        assert_eq!(qualified("a.b", "user"), "\"a.b\".\"user\"");
    }

    /// One literal quoter: `literal` and `role_name::quote_literal` return the same text, and
    /// that text holds the input under either `standard_conforming_strings` setting.
    #[test]
    fn literal_matches_role_name_quote_literal() {
        for input in [
            "",
            "plain",
            "it's",
            "''",
            "\\",
            "a\\b",
            "a\\'b",
            "\\'; x",
            "trailing\\",
            "\\\\double",
            "é\\ü'ß",
            "表\\示'能",
            "line\nbreak\\n",
        ] {
            let quoted = literal(input);
            assert_eq!(
                quoted,
                chelabase_types::role_name::quote_literal(input),
                "{input:?}"
            );
            assert_eq!(read_back(&quoted).as_deref(), Some(input), "{quoted}");
        }
    }

    /// What Postgres reads `quoted` as, when both `standard_conforming_strings` settings read
    /// it the same: `'…'` takes `''` as a quote and may hold no backslash (the one byte the
    /// two settings read differently); `E'…'` takes `''` as a quote and `\\` as a backslash,
    /// and no other escape appears. `None` for anything else.
    fn read_back(quoted: &str) -> Option<String> {
        let (escapes, body) = match quoted.strip_prefix('E') {
            Some(rest) => (true, rest),
            None => (false, quoted),
        };
        let body = body.strip_prefix('\'')?.strip_suffix('\'')?;
        let mut out = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            match c {
                '\'' => {
                    if chars.next() != Some('\'') {
                        return None;
                    }
                    out.push('\'');
                }
                '\\' => {
                    if !escapes || chars.next() != Some('\\') {
                        return None;
                    }
                    out.push('\\');
                }
                c => out.push(c),
            }
        }
        Some(out)
    }

    #[test]
    fn literal_escapes_quotes_and_backslashes() {
        assert_eq!(literal("plain"), "'plain'");
        assert_eq!(literal(""), "''");
        assert_eq!(literal("it's"), "'it''s'");
        assert_eq!(literal("a\\b"), "E'a\\\\b'");
        assert_eq!(literal("x'\\y"), "E'x''\\\\y'");
    }
}
