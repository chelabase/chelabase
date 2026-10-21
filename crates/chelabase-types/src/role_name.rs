//! Rules for the names of login roles (spec 2026-10-01-login-roles, Part 1).

/// Names a login role can't take: the platform's own roles and Postgres' keywords.
/// Any name starting with `pg_` (in any case) is reserved too. `owner` stands for the database
/// owner in member grants, so no role may take it.
pub const RESERVED_ROLE_NAMES: [&str; 11] = [
    "owner",
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

/// Prefix of the hidden per-member Postgres login roles (`cb_m_<member>_<db hash>`). Reserved
/// in any case, like `chelabase_`.
pub const MEMBER_ROLE_PREFIX: &str = "cb_m_";

const CHELABASE_PREFIX: &str = "chelabase_";

/// A hidden member login role (prefix `cb_m_`, any case).
pub fn is_member_role(name: &str) -> bool {
    has_prefix_ci(name, MEMBER_ROLE_PREFIX)
}

/// The member role of `member` on any branch (R17): `cb_m_<member uuid simple hex>`, 37
/// characters. The worker creates it; the orchestrator logs in as it.
pub fn member_role_name(member: uuid::Uuid) -> String {
    format!("{MEMBER_ROLE_PREFIX}{}", member.simple())
}

/// The hand-over after an act-as run (members and roles, R5), run as a superuser in the run's
/// database once the run's session is over. It holds the **member-role invariant**: after any
/// act-as run ends (success, error, cancel, timeout or disconnect), the member role owns no
/// object of any class, appears in no ACL, default ACL or policy, and can `SET ROLE` only into
/// its reconciled safe set (the reconciler grants it nothing else, and a member role can't
/// grant itself memberships: it holds no ADMIN and isn't CREATEROLE).
///
/// In one transaction (one DO, bounded: 2 s for each lock, 6 s for the whole statement, so the
/// worker answers within its 10 s RPC deadline):
/// 1. the run's backend (`backend`: its pid and `backend_start`, so a recycled pid is never
///    touched) is ended, waiting up to 2 s; a backend that is still there after that is an
///    error (nothing moves while the session could still create);
/// 2. the target: `acted` only when the member role can still `SET ROLE` to it and neither it
///    nor anything it reaches (its `SET`/ADMIN chain) is owner-level, LOGIN, ADMIN-holding or
///    reserved, the reconciler's own rule (`safe_roles`); giving objects to anything else would
///    let the member plant code (a `SECURITY DEFINER` function, a view) that runs as a role it
///    may not keep. With no target (`acted` refused or `None`), nothing is reassigned, and the
///    rest below still runs;
/// 3. `REASSIGN OWNED BY member TO acted`: what the script made as the member role itself
///    (after `RESET ROLE`), of every class, goes to the role it acted as;
/// 4. the member role's own settings (`ALTER ROLE CURRENT_USER [IN DATABASE …] SET …`) are
///    reset;
/// 5. `REASSIGN` once more, then `DROP OWNED BY member`: every privilege, default privilege
///    and policy naming the member role in this database (and on shared objects) goes. The
///    second `REASSIGN` keeps the window short in which another run of the same member could
///    commit an object between the two: such an object is dropped. Objects of another run
///    that are still uncommitted are invisible here and stay until that run's own hand-over
///    (or the next reconcile's cleanup, R31). A run holding a lock on one of its objects makes
///    this wait at most 2 s, then fail; the next reconcile's cleanup does it instead.
///
/// Nothing runs in the script's own session, so nothing the script set (search path, temp
/// objects shadowing catalogs, read-only defaults, role settings) can change what this does.
pub fn member_hand_over_sql(
    member_role: &str,
    acted: Option<&str>,
    backend: Option<(i32, &str)>,
) -> String {
    let m = quote_literal(member_role);
    let terminate = backend
        .map(|(pid, started)| {
            let started = quote_literal(started);
            let this = format!(
                "s.pid = {pid} AND s.backend_start = {started}::pg_catalog.timestamptz AND s.usename = {m}"
            );
            format!(
                "  SELECT pg_catalog.pg_terminate_backend(s.pid, 2000) INTO ended\n    FROM pg_catalog.pg_stat_activity s WHERE {this};\n\
                 \x20 IF ended IS FALSE AND EXISTS (SELECT 1 FROM pg_catalog.pg_stat_activity s WHERE {this}) THEN\n\
                 \x20   RAISE EXCEPTION 'the run''s backend % is still there', {pid};\n  END IF;\n"
            )
        })
        .unwrap_or_default();
    let reassign = reassign_if_allowed(member_role, acted);
    let drop = quote_literal(&format!("DROP OWNED BY {}", quote_ident(member_role)));
    let reset = quote_literal(&format!("ALTER ROLE {} RESET ALL", quote_ident(member_role)));
    let body = format!(
        "DECLARE d record; ended boolean;
BEGIN
{terminate}{reassign}  EXECUTE {reset};
  FOR d IN SELECT db.datname FROM pg_catalog.pg_db_role_setting s
             JOIN pg_catalog.pg_database db ON db.oid = s.setdatabase
            WHERE s.setrole = (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = {m}) LOOP
    EXECUTE pg_catalog.format('ALTER ROLE %I IN DATABASE %I RESET ALL', {m}, d.datname);
  END LOOP;
{reassign}  EXECUTE {drop};
END"
    );
    bounded_do(&body)
}

/// The reconciler's cleanup of a member role in one database (R31, I-B): what it still owns
/// goes to `target` while it can still `SET ROLE` to it (and `target`'s chain is still safe,
/// as in [`member_hand_over_sql`]), then `DROP OWNED` removes the rest and every privilege
/// granted to it. Run before the reconcile's revokes, so a role being revoked still counts.
pub fn member_cleanup_sql(member_role: &str, target: Option<&str>) -> String {
    let reassign = reassign_if_allowed(member_role, target);
    let drop = quote_literal(&format!("DROP OWNED BY {}", quote_ident(member_role)));
    bounded_do(&format!("BEGIN\n{reassign}  EXECUTE {drop};\nEND"))
}

/// `body` as one DO with the hand-over's bounds.
fn bounded_do(body: &str) -> String {
    format!(
        "SET standard_conforming_strings = on;\nSET search_path = pg_catalog, pg_temp;\n\
         SET lock_timeout = '2s';\nSET statement_timeout = '6s';\nDO {};",
        quote_literal(body)
    )
}

/// Inside a DO body: `REASSIGN OWNED BY member TO target` when the member role can `SET ROLE`
/// to `target` and `target`'s chain (itself included, through `SET` and ADMIN memberships)
/// holds nothing owner-level (SUPERUSER, CREATEROLE, CREATEDB, REPLICATION, BYPASSRLS, a member
/// of the database's owner, `neon_superuser` or `cloud_admin`), no LOGIN role, no role holding
/// ADMIN on any role, and no reserved name (`pg_*`, `cb_m_*`, `chelabase_*`, the platform's
/// own; `anon` and `authenticated` may be). Empty without a target.
fn reassign_if_allowed(member_role: &str, target: Option<&str>) -> String {
    let Some(target) = target else {
        return String::new();
    };
    let m = quote_literal(member_role);
    let t = quote_literal(target);
    let reserved: Vec<String> = RESERVED_ROLE_NAMES
        .iter()
        .filter(|n| !matches!(**n, "anon" | "authenticated"))
        .map(|n| quote_literal(n))
        .collect();
    let reserved = reserved.join(", ");
    let reassign = quote_literal(&format!(
        "REASSIGN OWNED BY {} TO {}",
        quote_ident(member_role),
        quote_ident(target)
    ));
    // Nested: `pg_has_role` errors on a role that's gone, and SQL doesn't promise an order.
    format!(
        "  IF EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = {t}) THEN
  IF pg_catalog.pg_has_role({m}, {t}, 'SET') AND NOT EXISTS (
       WITH RECURSIVE chain(oid) AS (
         SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = {t}
         UNION SELECT a.roleid FROM pg_catalog.pg_auth_members a JOIN chain c ON a.member = c.oid
          WHERE a.set_option OR a.admin_option)
       SELECT 1 FROM chain c JOIN pg_catalog.pg_roles r ON r.oid = c.oid
        WHERE r.rolsuper OR r.rolreplication OR r.rolcreaterole OR r.rolbypassrls OR r.rolcreatedb
           OR r.rolcanlogin
           OR pg_catalog.pg_has_role(r.oid, (SELECT cdb.datdba FROM pg_catalog.pg_database cdb
                                              WHERE cdb.datname = pg_catalog.current_database()), 'MEMBER')
           OR EXISTS (SELECT 1 FROM pg_catalog.pg_roles o
                       WHERE o.rolname IN ('neon_superuser', 'cloud_admin')
                         AND pg_catalog.pg_has_role(r.oid, o.oid, 'MEMBER'))
           OR r.rolname ~* '^(pg_|cb_m_|chelabase_)' OR r.rolname = ANY (ARRAY[{reserved}])
           OR EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members x WHERE x.member = r.oid AND x.admin_option))
  THEN
    EXECUTE {reassign};
  END IF;
  END IF;
"
    )
}

fn has_prefix_ci(name: &str, prefix: &str) -> bool {
    name.len() >= prefix.len()
        && name.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// `cb_m_…` or `chelabase_…`, any case: names only Chelabase itself may create.
pub fn has_reserved_prefix(name: &str) -> bool {
    is_member_role(name) || has_prefix_ci(name, CHELABASE_PREFIX)
}

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
    if lower.starts_with("pg_")
        || has_reserved_prefix(name)
        || RESERVED_ROLE_NAMES.contains(&name)
    {
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

/// `RESERVED_ROLE_NAMES`, except `owner` itself; a `pg_`, `cb_m_` or `chelabase_` prefix (any
/// case) is reserved even for the owner.
pub fn is_reserved_for(name: &str, owner: &str) -> bool {
    if name.to_ascii_lowercase().starts_with("pg_") || has_reserved_prefix(name) {
        return true;
    }
    RESERVED_ROLE_NAMES.contains(&name) && name != owner
}

/// A role the API never changes (PATCH): [`is_reserved_for`], plus any `chelabase_…` or
/// `cb_m_…` role (any case), listed in [`RESERVED_ROLE_NAMES`] or not.
pub fn is_reserved_target(name: &str, owner: &str) -> bool {
    is_reserved_for(name, owner) || has_reserved_prefix(name)
}

/// A role no API-made role may be made a member of (U2): the owner, every reserved name
/// (`authenticator`, `anon`, `authenticated`, `neon_superuser`, `cloud_admin`, …), any
/// `chelabase_…` role, any `cb_m_…` member role and any `pg_…` role, in any case. Owner-level is the only way into
/// `neon_superuser`.
pub fn is_membership_reserved(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("pg_")
        || has_reserved_prefix(name)
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

/// The role setting (`ALTER ROLE … SET`) that marks a personal login: the compute's guard
/// refuses every change to a marked role from anyone but a superuser.
pub const PERSONAL_LOGIN_MARKER: &str = "chelabase.managed_role";

/// `ALTER ROLE <role> SET chelabase.managed_role = 'personal';`
pub fn mark_personal_login_sql(role: &str) -> String {
    format!(
        "ALTER ROLE {} SET {PERSONAL_LOGIN_MARKER} = 'personal';",
        quote_ident(role)
    )
}

/// One boolean row: whether `role` carries the marker, i.e. a `pg_db_role_setting` row with
/// `setdatabase = 0` for the role whose `setconfig` has an entry starting
/// `chelabase.managed_role=` (the one predicate K3's `/grants`, K6's guard and C3 share).
pub fn is_marked_sql(role: &str) -> String {
    format!(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_db_role_setting s \
         JOIN pg_catalog.pg_roles r ON r.oid = s.setrole \
         WHERE s.setdatabase = 0 AND r.rolname = {} \
         AND EXISTS (SELECT 1 FROM pg_catalog.unnest(s.setconfig) AS c \
         WHERE pg_catalog.starts_with(c, '{PERSONAL_LOGIN_MARKER}=')));",
        quote_literal(role)
    )
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
        // `owner` stands for the database owner in member grants, so no role may take it.
        assert_eq!(check_login_role_name("owner"), Err(RoleNameError::Reserved));
        assert!(is_reserved_for("owner", "postgres"));
        assert!(is_membership_reserved("owner"));
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
    fn reserved_prefixes_exhaustive() {
        let cases = [
            ("cb_m_x", true),
            ("CB_M_x", true),
            ("chelabase_x", true),
            ("Chelabase_X", true),
            ("cbm_x", false),
            ("chelabasex", false),
            ("billing", false),
        ];
        for (name, refused) in cases {
            assert_eq!(is_reserved_for(name, "postgres"), refused, "for {name}");
            assert_eq!(
                is_reserved_target(name, "postgres"),
                refused,
                "target {name}"
            );
            assert_eq!(is_membership_reserved(name), refused, "membership {name}");
            assert_eq!(
                check_login_role_name(&name.to_ascii_lowercase()) == Err(RoleNameError::Reserved),
                refused,
                "login {name}"
            );
        }
        assert!(is_member_role("cb_m_x") && is_member_role("CB_M_x"));
        assert!(!is_member_role("cbm_x") && !is_member_role("chelabase_x"));
        assert_eq!(MEMBER_ROLE_PREFIX, "cb_m_");
    }

    #[test]
    fn member_role_name_is_the_prefix_and_the_simple_uuid() {
        let member = uuid::Uuid::parse_str("0b5e7c1a-2f3d-4e5f-8a9b-0c1d2e3f4a5b").unwrap();
        let name = member_role_name(member);
        assert_eq!(name, "cb_m_0b5e7c1a2f3d4e5f8a9b0c1d2e3f4a5b");
        assert_eq!(name.len(), 37);
        assert!(is_member_role(&name) && has_reserved_prefix(&name));
    }

    /// `parts` appear in `sql` in this order.
    fn in_order(sql: &str, parts: &[&str]) {
        let mut at = 0;
        for part in parts {
            let found = sql[at..].find(part).unwrap_or_else(|| panic!("{part} after {at} in {sql}"));
            at += found + part.len();
        }
    }

    #[test]
    fn hand_over_sql_ends_the_session_checks_the_role_then_reassigns_and_drops() {
        let sql = member_hand_over_sql(
            "cb_m_ab",
            Some("Bil'l \"ing\""),
            Some((4242, "2026-10-09 10:00:00.5+00")),
        );
        assert!(sql.starts_with(
            "SET standard_conforming_strings = on;\nSET search_path = pg_catalog, pg_temp;\n\
             SET lock_timeout = '2s';\nSET statement_timeout = '6s';\n"
        ));
        // The DO body is one literal: quotes in names are doubled twice, never closing it.
        let body = sql.split_once("DO '").unwrap().1;
        assert!(body.ends_with("';"), "{sql}");
        in_order(
            &sql,
            &[
                // M-a: this run's backend only (pid and start), and an end that didn't happen
                // is an error.
                "pg_terminate_backend(s.pid, 2000)",
                "s.pid = 4242 AND s.backend_start = ''2026-10-09 10:00:00.5+00''::pg_catalog.timestamptz AND s.usename = ''cb_m_ab''",
                "IF ended IS FALSE AND EXISTS",
                "RAISE EXCEPTION",
                // M-f: the target must be one the member role can become and, like the
                // reconciler's rule, reach nothing owner-level, LOGIN, ADMIN-holding or reserved.
                "pg_catalog.pg_has_role(''cb_m_ab'', ''Bil''''l \"ing\"'', ''SET'')",
                "WITH RECURSIVE chain",
                "r.rolcanlogin",
                "x.admin_option",
                "''REASSIGN OWNED BY \"cb_m_ab\" TO \"Bil''''l \"\"ing\"\"\"''",
                "''ALTER ROLE \"cb_m_ab\" RESET ALL''",
                "IN DATABASE %I RESET ALL",
                // M-d: once more right before the drop.
                "''REASSIGN OWNED BY \"cb_m_ab\" TO \"Bil''''l \"\"ing\"\"\"''",
                "''DROP OWNED BY \"cb_m_ab\"''",
            ],
        );
        // Without a backend there's nothing to end.
        assert!(!member_hand_over_sql("cb_m_ab", Some("g"), None).contains("pg_terminate_backend"));
    }

    /// The qualifiers (`x` of `x.col`) an SQL text uses.
    fn qualifiers(sql: &str) -> std::collections::BTreeSet<String> {
        let b = sql.as_bytes();
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let mut out = std::collections::BTreeSet::new();
        for (i, c) in b.iter().enumerate() {
            if *c != b'.' || i == 0 || !ident(b[i - 1]) {
                continue;
            }
            let start = (0..i).rev().take_while(|&j| ident(b[j])).last().unwrap();
            if start > 0 && b[start - 1] == b'.' {
                continue;
            }
            out.insert(sql[start..i].to_string());
        }
        out
    }

    // Live finding: PL/pgSQL resolves `d.datdba` to its record variable `d` (not yet assigned)
    // before the SQL alias `d`, so every hand-over with a target failed. The reassign check runs
    // inside the hand-over's DO body: none of its qualifiers may be a variable that body declares.
    #[test]
    fn the_reassign_check_qualifies_nothing_with_a_hand_over_variable() {
        let sql = member_hand_over_sql("cb_m_ab", Some("g"), Some((7, "t")));
        let declare = sql.split_once("DECLARE ").unwrap().1.split_once("BEGIN").unwrap().0;
        let vars: Vec<&str> = declare
            .split(';')
            .filter_map(|v| v.split_whitespace().next())
            .collect();
        assert!(vars.contains(&"d") && vars.contains(&"ended"), "{vars:?}");
        let used = qualifiers(&reassign_if_allowed("cb_m_ab", Some("g")));
        assert!(used.contains("r") && used.contains("a"), "{used:?}");
        for v in vars {
            assert!(!used.contains(v), "the reassign check qualifies with `{v}`: {used:?}");
        }
    }

    // M-b: with no role to hand to (a name no member role may hold), nothing is reassigned, but
    // the rest still goes and the settings are still reset.
    #[test]
    fn a_refused_hand_over_still_drops_and_resets() {
        let sql = member_hand_over_sql("cb_m_ab", None, Some((7, "t")));
        assert!(!sql.contains("REASSIGN"), "{sql}");
        in_order(
            &sql,
            &["pg_terminate_backend", "RESET ALL", "''DROP OWNED BY \"cb_m_ab\"''"],
        );
    }

    // I-B: the reconciler's per-(role, database) cleanup: reassign only while the member role
    // can still become the target, then drop the rest; one DO, bounded.
    #[test]
    fn cleanup_sql_reassigns_only_while_settable_then_drops() {
        let sql = member_cleanup_sql("cb_m_ab", Some("sales"));
        assert!(sql.contains("SET lock_timeout = '2s';\nSET statement_timeout = '6s';"), "{sql}");
        in_order(
            &sql,
            &[
                "IF pg_catalog.pg_has_role(''cb_m_ab'', ''sales'', ''SET'')",
                "WITH RECURSIVE chain",
                "''REASSIGN OWNED BY \"cb_m_ab\" TO \"sales\"''",
                "''DROP OWNED BY \"cb_m_ab\"''",
            ],
        );
        assert!(!sql.contains("pg_terminate_backend") && !sql.contains("RESET ALL"), "{sql}");
        let none = member_cleanup_sql("cb_m_ab", None);
        assert!(!none.contains("REASSIGN") && none.contains("''DROP OWNED BY \"cb_m_ab\"''"), "{none}");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal("a\\b"), "'a\\b'");
    }

    // c3 (RT4-4, SP22): the role marker's name and the two statements built on it.
    #[test]
    fn the_personal_login_marker_is_the_pinned_guc() {
        assert_eq!(PERSONAL_LOGIN_MARKER, "chelabase.managed_role");
    }

    #[test]
    fn marking_a_personal_login_is_one_alter_role_set() {
        assert_eq!(
            mark_personal_login_sql("alice"),
            "ALTER ROLE \"alice\" SET chelabase.managed_role = 'personal';"
        );
    }

    #[test]
    fn marking_quotes_the_name_exactly() {
        // A double quote is doubled and the case is kept (the role is `Al"ice`, not `al"ice`).
        assert_eq!(
            mark_personal_login_sql("Al\"ice"),
            "ALTER ROLE \"Al\"\"ice\" SET chelabase.managed_role = 'personal';"
        );
        assert_eq!(
            mark_personal_login_sql("Alice"),
            "ALTER ROLE \"Alice\" SET chelabase.managed_role = 'personal';"
        );
        // A name that tries to end the identifier and add a statement stays one identifier.
        let evil = mark_personal_login_sql("x\"; DROP ROLE y; --");
        assert_eq!(
            evil,
            "ALTER ROLE \"x\"\"; DROP ROLE y; --\" SET chelabase.managed_role = 'personal';"
        );
        // One statement: the only `;` outside the identifier is the last character.
        assert_eq!(mark_personal_login_sql("a;b").matches(';').count(), 2);
    }

    #[test]
    fn the_marked_predicate_is_the_sp22_one() {
        let sql = is_marked_sql("alice");
        // A `pg_db_role_setting` row with `setdatabase = 0` for the role (resolved from its
        // name as a literal) whose `setconfig` has an entry starting `chelabase.managed_role=`.
        for needle in [
            "pg_db_role_setting",
            "setdatabase = 0",
            "setrole",
            "setconfig",
            "chelabase.managed_role=",
            "'alice'",
        ] {
            assert!(sql.contains(needle), "{needle} missing from {sql}");
        }
        assert!(
            sql.trim_start().to_ascii_uppercase().starts_with("SELECT"),
            "one boolean row, so a SELECT: {sql}"
        );
        // The marker named in the predicate is the constant's.
        assert!(sql.contains(&format!("{PERSONAL_LOGIN_MARKER}=")), "{sql}");
    }

    #[test]
    fn the_marked_predicate_quotes_the_name_as_a_literal() {
        let sql = is_marked_sql("x' OR true --");
        assert!(sql.contains(&quote_literal("x' OR true --")), "{sql}");
        assert!(sql.contains("'x'' OR true --'"), "{sql}");
        // Case is kept: `Alice` is not `alice`.
        assert!(is_marked_sql("Alice").contains("'Alice'"));
        assert!(!is_marked_sql("Alice").contains("'alice'"));
        // A database-scoped row doesn't mark: the predicate must not accept `setdatabase <> 0`.
        assert!(!sql.contains("setdatabase <> 0") && !sql.contains("setdatabase > 0"), "{sql}");
    }
}
