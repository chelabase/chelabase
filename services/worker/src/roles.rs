//! The two roles inside every database.
//!
//! `cheladb_platform` is a true superuser used only by the platform. `postgres`
//! is the owner role users get: elevated (owns the database and `public`,
//! CREATEDB, CREATEROLE, BYPASSRLS) but not a superuser, so it can't alter or
//! drop the platform role (Postgres 16+ limits CREATEROLE to roles it created).

pub const PLATFORM_ROLE: &str = "cheladb_platform";
pub const OWNER_ROLE: &str = "postgres";

/// Statements run as the platform role while connected to database `postgres`.
///
/// Passwords come from `credentials::generate_password` (alphanumeric) and
/// database names from `names::sanitize_db_name` (`[A-Za-z0-9_]`), so plain
/// literals and quoted identifiers are safe here.
pub fn owner_setup_sql(database_name: &str, owner_password: &str, superuser: bool) -> Vec<String> {
    let mut create = format!(
        "CREATE ROLE {OWNER_ROLE} LOGIN PASSWORD '{owner_password}' CREATEDB CREATEROLE BYPASSRLS"
    );
    if superuser {
        create.push_str(" SUPERUSER");
    }
    vec![
        create,
        format!(
            "GRANT pg_read_all_data, pg_write_all_data, pg_monitor, pg_signal_backend TO {OWNER_ROLE}"
        ),
        format!(r#"ALTER DATABASE "{database_name}" OWNER TO {OWNER_ROLE}"#),
    ]
}

/// Replace the platform password Postgres was initialised with (visible via
/// `docker inspect`) with one that only the metadata store holds, encrypted.
pub fn rotate_platform_password_sql(password: &str) -> String {
    format!("ALTER ROLE {PLATFORM_ROLE} PASSWORD '{password}'")
}

/// Statements run as the platform role while connected to the new database.
pub fn owner_schema_sql() -> Vec<String> {
    vec![format!("ALTER SCHEMA public OWNER TO {OWNER_ROLE}")]
}

/// Reset the owner's password without ever waiting indefinitely: if the owner
/// holds a lock on their role (e.g. an open transaction that altered it), the
/// reset fails fast instead of stalling every other reset behind it.
pub fn reset_password_statements(password: &str) -> Vec<String> {
    vec![
        "SET lock_timeout = '5s'".to_string(),
        "SET statement_timeout = '10s'".to_string(),
        format!("ALTER ROLE {OWNER_ROLE} PASSWORD '{password}'"),
    ]
}

/// libpq-style connection string for a platform session. `search_path` and
/// `default_transaction_read_only` are fixed so database-level settings the
/// owner controls can't redirect or block platform statements.
pub fn platform_connection_config(port: u16, password: &str, database: &str) -> String {
    format!(
        "host=127.0.0.1 port={port} user={PLATFORM_ROLE} password={password} dbname={database} \
         connect_timeout=5 options='-c search_path=pg_catalog -c default_transaction_read_only=off'"
    )
}

/// `CHELADB_DB_OWNER_SUPERUSER`: only an explicit true value enables it.
pub fn owner_superuser_from_env(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("true" | "1" | "yes")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_is_elevated_but_not_superuser_by_default() {
        let sql = owner_setup_sql("web1", "pw123", false).join("\n");
        assert!(sql.contains(
            "CREATE ROLE postgres LOGIN PASSWORD 'pw123' CREATEDB CREATEROLE BYPASSRLS"
        ));
        assert!(!sql.contains("SUPERUSER"));
        assert!(sql.contains(
            "GRANT pg_read_all_data, pg_write_all_data, pg_monitor, pg_signal_backend TO postgres"
        ));
        assert!(sql.contains(r#"ALTER DATABASE "web1" OWNER TO postgres"#));
    }

    #[test]
    fn superuser_switch_makes_the_owner_superuser() {
        assert!(owner_setup_sql("web1", "pw", true)[0].ends_with(" SUPERUSER"));
    }

    // The password Postgres was initialised with is visible via `docker inspect`;
    // the platform replaces it right away and stores only the new one.
    #[test]
    fn platform_password_is_rotated_away_from_the_init_password() {
        assert_eq!(
            rotate_platform_password_sql("fresh"),
            "ALTER ROLE cheladb_platform PASSWORD 'fresh'"
        );
    }

    #[test]
    fn schema_is_handed_to_the_owner() {
        assert_eq!(
            owner_schema_sql(),
            vec!["ALTER SCHEMA public OWNER TO postgres".to_string()]
        );
    }

    #[test]
    fn reset_bounds_lock_and_statement_time_before_altering_the_owner() {
        assert_eq!(
            reset_password_statements("newpw"),
            vec![
                "SET lock_timeout = '5s'".to_string(),
                "SET statement_timeout = '10s'".to_string(),
                "ALTER ROLE postgres PASSWORD 'newpw'".to_string(),
            ]
        );
    }

    // The owner can't influence platform sessions through database settings.
    #[test]
    fn platform_connections_pin_search_path_read_write_and_a_connect_timeout() {
        let config = platform_connection_config(5432, "pw", "postgres");
        assert!(config.contains("host=127.0.0.1 port=5432"), "{config}");
        assert!(config.contains("user=cheladb_platform password=pw dbname=postgres"), "{config}");
        assert!(config.contains("connect_timeout=5"), "{config}");
        assert!(config.contains("search_path=pg_catalog"), "{config}");
        assert!(config.contains("default_transaction_read_only=off"), "{config}");
    }

    #[test]
    fn superuser_switch_parses_explicit_true_only() {
        for on in ["true", "1", "YES"] {
            assert!(owner_superuser_from_env(Some(on)));
        }
        for off in ["false", "", "nope"] {
            assert!(!owner_superuser_from_env(Some(off)));
        }
        assert!(!owner_superuser_from_env(None));
    }
}
