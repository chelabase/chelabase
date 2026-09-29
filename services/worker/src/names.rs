//! The Postgres database name derived from a database's user-facing name.

/// A Postgres database name from the user's name (see
/// [`cheladb_metadata::postgres_database_name`], which also rejects reserved
/// and over-long names; the orchestrator refuses those before queueing).
pub fn sanitize_db_name(name: &str) -> Result<String, String> {
    cheladb_metadata::postgres_database_name(name).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_like_before() {
        assert_eq!(sanitize_db_name("web-1 prod").unwrap(), "web_1_prod");
        assert_eq!(sanitize_db_name("---").unwrap(), "___");
        assert_eq!(sanitize_db_name("").unwrap(), "cheladb");
        assert!(sanitize_db_name("template1").is_err());
    }
}
