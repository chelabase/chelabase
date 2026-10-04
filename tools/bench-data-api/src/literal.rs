//! SQL string literal quoting for the simple-query protocol.

/// `'…'` with `'` doubled. Backslashes pass through (standard_conforming_strings is on).
/// Errors on NUL, which Postgres text cannot hold.
pub fn quote_literal(s: &str) -> anyhow::Result<String> {
    anyhow::ensure!(!s.contains('\0'), "NUL byte in SQL literal");
    Ok(format!("'{}'", s.replace('\'', "''")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_literal_doubles_quotes() {
        assert_eq!(quote_literal("it's").unwrap(), "'it''s'");
    }

    #[test]
    fn quote_literal_keeps_backslashes_and_unicode() {
        assert_eq!(
            quote_literal(r#"a\b ü {"x":"'"}"#).unwrap(),
            r#"'a\b ü {"x":"''"}'"#
        );
    }

    #[test]
    fn quote_literal_refuses_nul() {
        assert!(quote_literal("a\0b").is_err());
    }
}
