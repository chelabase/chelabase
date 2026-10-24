//! T6 fix round C1 (spec Part 6, MH-18 amended): the coder's edge cases for
//! [`super::personal_cleanup_script`] beyond the RED set. The SQL itself was probed on
//! PostgreSQL 16 and 17 (a planted `SECURITY DEFINER` function, a procedure, a view and a table
//! quarantined; run twice; a tampered quarantine role made powerless again; a concurrent
//! `CREATE ROLE` of the same name).

use super::{CLEANUP_MARK, QUARANTINE_ROLE, personal_cleanup_script};

fn at(script: &str, needle: &str) -> usize {
    script
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} in {script}"))
}

/// Two scripts racing on the same cluster: the existence check comes first, and a concurrent
/// `CREATE ROLE` that wins (`duplicate_object`, or `unique_violation` once its insert commits)
/// is caught in its own block, so the reassign still runs.
#[test]
fn a_concurrent_create_of_the_quarantine_role_is_caught() {
    let s = personal_cleanup_script("alice");
    let check = at(
        &s,
        &format!("WHERE r.rolname = ''{QUARANTINE_ROLE}'') THEN"),
    );
    let create = at(&s, &format!("CREATE ROLE {QUARANTINE_ROLE} NOLOGIN;"));
    let caught = at(
        &s,
        "EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;",
    );
    assert!(check < create && create < caught, "{s}");
    assert_eq!(s.matches("CREATE ROLE").count(), 1, "{s}");
}

/// A quarantine role someone changed (LOGIN, a password, an attribute, a membership either
/// way, an open session) is made powerless again before anything is moved to it; the
/// attributes are read from `pg_authid`, since `pg_roles` masks every password.
#[test]
fn a_tampered_quarantine_role_is_made_powerless_again_before_the_reassign() {
    let s = personal_cleanup_script("alice");
    let reassign = at(&s, "REASSIGN OWNED BY alice");
    let alter = at(
        &s,
        &format!(
            "ALTER ROLE {QUARANTINE_ROLE} NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION \
             NOBYPASSRLS\n      PASSWORD NULL;"
        ),
    );
    assert!(at(&s, "FROM pg_catalog.pg_authid r") < alter, "{s}");
    assert!(s.contains("r.rolpassword IS NOT NULL"), "{s}");
    let members = at(
        &s,
        &format!("WHERE a.rolname = ''{QUARANTINE_ROLE}'' OR b.rolname = ''{QUARANTINE_ROLE}''"),
    );
    assert!(
        s.contains("''REVOKE %I FROM %I GRANTED BY %I CASCADE''"),
        "{s}"
    );
    assert!(
        s.contains("''REVOKE %I FROM %I CASCADE''"),
        "a grantor that's gone: {s}"
    );
    let sessions = at(&s, &format!("WHERE s.usename = ''{QUARANTINE_ROLE}''"));
    assert!(
        alter < reassign && members < reassign && sessions < reassign,
        "{s}"
    );
}

/// The revoke covers procedures and aggregates too (`ROUTINE`), runs after the reassign (it
/// reaches what was just moved) and before `DROP OWNED`.
#[test]
fn execute_is_revoked_on_every_quarantined_routine_after_the_reassign() {
    let s = personal_cleanup_script("alice");
    let reassign = at(&s, "REASSIGN OWNED BY alice TO chelabase_orphaned;");
    let revoke = at(&s, "''REVOKE EXECUTE ON ROUTINE %s FROM PUBLIC''");
    let drop = at(&s, "DROP OWNED BY alice;");
    assert!(reassign < revoke && revoke < drop, "{s}");
}

/// The login's name appears only in the two statements that name it, quoted as an identifier,
/// never inside a DO body (a name with quotes can't break out of the literals), and the
/// literals are read with `standard_conforming_strings` on.
#[test]
fn an_odd_login_name_appears_only_as_an_identifier() {
    let odd = "Al'i\"ce \\x";
    let s = personal_cleanup_script(odd);
    assert!(s.starts_with(CLEANUP_MARK), "{s}");
    let quoted = "\"Al'i\"\"ce \\x\"";
    assert_eq!(s.matches(quoted).count(), 2, "{s}");
    assert!(
        s.contains(&format!(
            "REASSIGN OWNED BY {quoted} TO chelabase_orphaned;"
        )),
        "{s}"
    );
    assert!(s.ends_with(&format!("DROP OWNED BY {quoted};")), "{s}");
    assert!(
        !s.contains("Al'i\"ce") && !s.contains("Al''i"),
        "never unquoted: {s}"
    );
    assert!(
        at(&s, "SET standard_conforming_strings = on;") < at(&s, "DO '"),
        "{s}"
    );
    assert!(!s.contains("DO E'"), "no escape strings: {s}");
}
