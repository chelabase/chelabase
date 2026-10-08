//! Rules for the names of login roles (spec 2026-10-01-login-roles, Part 1).

/// Names a login role can't take: the platform's own roles and Postgres' keywords.
/// Any name starting with `pg_` (in any case) is reserved too.
pub const RESERVED_ROLE_NAMES: [&str; 10] = [
    "postgres",
    "anon",
    "authenticated",
    "authenticator",
    "chelabase_admin",
    "cloud_admin",
    "neon_superuser",
    "public",
    "none",
    "chelabase_auth_sync",
];

/// Why a login role name was refused (422 `invalid_role_name` / `reserved_role_name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleNameError {
    Invalid,
    Reserved,
}

/// `^[a-z_][a-z0-9_]{0,62}$` and not reserved. Reserved is checked first, so `PG_x` is
/// `Reserved`, not `Invalid`.
pub fn check_login_role_name(name: &str) -> Result<(), RoleNameError> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("pg_") || RESERVED_ROLE_NAMES.contains(&name) {
        return Err(RoleNameError::Reserved);
    }
    let bytes = name.as_bytes();
    let shaped = !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_');
    if shaped {
        Ok(())
    } else {
        Err(RoleNameError::Invalid)
    }
}

/// Any name Postgres can hold: 1..=63 bytes, no NUL. (reset, delete, reveal)
pub fn check_any_role_name(name: &str) -> Result<(), RoleNameError> {
    if name.is_empty() || name.len() > 63 || name.contains('\0') {
        Err(RoleNameError::Invalid)
    } else {
        Ok(())
    }
}

/// `RESERVED_ROLE_NAMES` or a `pg_` prefix (any case), except `owner` itself
/// (a `pg_` name is reserved even for the owner).
pub fn is_reserved_for(name: &str, owner: &str) -> bool {
    if name.to_ascii_lowercase().starts_with("pg_") {
        return true;
    }
    RESERVED_ROLE_NAMES.contains(&name) && name != owner
}

/// A role the API never changes (PATCH): [`is_reserved_for`], plus any `chelabase_…` role
/// (any case), listed in [`RESERVED_ROLE_NAMES`] or not.
pub fn is_reserved_target(name: &str, owner: &str) -> bool {
    is_reserved_for(name, owner) || name.to_ascii_lowercase().starts_with("chelabase_")
}

/// A role no API-made role may be made a member of (U2): the owner, every reserved name
/// (`authenticator`, `anon`, `authenticated`, `neon_superuser`, `cloud_admin`, …), any
/// `chelabase_…` role and any `pg_…` role, in any case. Owner-level is the only way into
/// `neon_superuser`.
pub fn is_membership_reserved(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("pg_")
        || lower.starts_with("chelabase_")
        || RESERVED_ROLE_NAMES.contains(&lower.as_str())
}

/// A double-quoted SQL identifier (`"` doubled).
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A single-quoted SQL literal (`'` doubled; no backslash escapes, so valid with
/// `standard_conforming_strings = on`).
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        for name in ["a", "_x", "app_reader", &"a".repeat(63)] {
            assert_eq!(check_login_role_name(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn invalid_names() {
        for name in ["", &"a".repeat(64), "Admin", "a-b", "1a", "a$", "é"] {
            assert_eq!(
                check_login_role_name(name),
                Err(RoleNameError::Invalid),
                "{name}"
            );
        }
    }

    #[test]
    fn reserved_names() {
        for name in RESERVED_ROLE_NAMES {
            assert_eq!(
                check_login_role_name(name),
                Err(RoleNameError::Reserved),
                "{name}"
            );
        }
        assert!(RESERVED_ROLE_NAMES.contains(&"chelabase_auth_sync"));
        for name in ["pg_x", "PG_x", "Pg_monitor"] {
            assert_eq!(
                check_login_role_name(name),
                Err(RoleNameError::Reserved),
                "{name}"
            );
        }
    }

    #[test]
    fn any_name_accepts_quoted_shapes() {
        for name in ["My Role", "a\"b", "É", &"a".repeat(63)] {
            assert_eq!(check_any_role_name(name), Ok(()), "{name}");
        }
        for name in ["", &"a".repeat(64), "a\0b"] {
            assert_eq!(check_any_role_name(name), Err(RoleNameError::Invalid));
        }
    }

    #[test]
    fn reserved_for_the_owner() {
        assert!(is_reserved_for("postgres", "someone"));
        assert!(!is_reserved_for("postgres", "postgres"));
        assert!(is_reserved_for("PG_x", "postgres"));
        assert!(is_reserved_for("pg_x", "pg_x"));
        assert!(!is_reserved_for("app", "postgres"));
    }

    #[test]
    fn chelabase_roles_are_reserved_targets() {
        for name in ["chelabase_custom", "Chelabase_x", "chelabase_admin", "pg_x", "anon"] {
            assert!(is_reserved_target(name, "postgres"), "{name}");
        }
        for name in ["app", "chela", "postgres", "Anon"] {
            assert!(!is_reserved_target(name, "postgres"), "{name}");
        }
    }

    #[test]
    fn membership_in_platform_roles_is_refused() {
        for name in [
            "postgres",
            "authenticator",
            "anon",
            "authenticated",
            "neon_superuser",
            "cloud_admin",
            "chelabase_admin",
            "chelabase_auth_sync",
            "chelabase_anything",
            "Chelabase_x",
            "pg_read_all_data",
            "PG_monitor",
            "public",
            "none",
        ] {
            assert!(is_membership_reserved(name), "{name}");
        }
        for name in ["app", "readers", "My Group", "chela"] {
            assert!(!is_membership_reserved(name), "{name}");
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal("a\\b"), "'a\\b'");
    }
}
