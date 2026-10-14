//! The DDL guard of protected branches (TE-A spec §5, rulings P5–P7; PB spec §2, S1–S3).
//!
//! On a protected branch the member-role reconciler grants the marker role
//! `chelabase_schema_locked` ([`SCHEMA_LOCKED_ROLE`]) to every member role whose member isn't
//! an Admin, and runs [`schema_guard_sql`] in every database of the branch. Two event triggers
//! then refuse any schema change made by a *locked* session, whatever role it `SET ROLE`s to.
//! A session is locked when its `session_user` holds the marker or, while the branch's
//! "guard login roles" switch is on ([`GuardSettings::guard_logins`]), when its (non-superuser)
//! `session_user` is anyone but the owner ([`crate::OWNER_ROLE`]), a member login (`cb_m_*`,
//! which the marker rule covers) or one of the branch's exemptions ([`GuardSettings::exempt`]).
//! The rule asks who logged in, never whether the role may log in now: an `ALTER ROLE …
//! NOLOGIN` from another session doesn't un-guard an open one. So a login role made from SQL
//! is guarded from its first statement.
//!
//! - `chelabase_schema_guard_ddl` (`ddl_command_end`): refused unless every object the command
//!   touched is temporary (its schema starts `pg_temp`) and no temporary relation it touched
//!   inherits from a permanent one (`CREATE TEMP TABLE … INHERITS`, `ALTER TABLE … INHERIT`
//!   change the parent). A command that collected nothing is allowed only when it is a
//!   `DROP …` other than `DROP OWNED` (which also revokes privileges, and those reach no
//!   trigger): its drops are the next trigger's to judge.
//! - `chelabase_schema_guard_drop` (`sql_drop`): refused unless every dropped object is
//!   temporary.
//!
//! Data maintenance stays allowed: `REFRESH MATERIALIZED VIEW` and `REINDEX` pass both
//! triggers. Both triggers are `ENABLE ALWAYS`, so `session_replication_role = replica` doesn't
//! skip them.
//!
//! The refusal is `42501` with the hint [`BRANCH_PROTECTED_HINT`]. Superusers (`cloud_admin`:
//! the reconciler, migrations, merges) are never refused: Postgres counts a superuser a member
//! of every role, so the functions rule one out first, before `pg_has_role`.
//!
//! The settings are rendered into both function bodies (S1, P1): the functions are SECURITY
//! INVOKER on purpose, so they couldn't read a superuser-only table. Each body carries one
//! line `-- settings: guard_logins=<on|off> exempt=[<hex>, …]`, and the exemptions are
//! compared as the lowercase hex of their UTF-8 bytes, so no name can break out of the body or
//! the literal. The exemption list is rendered even when the switch is off (P2).
//!
//! An unprotected branch carries no guard (S2): the reconciler asks
//! [`schema_guard_present_sql`] in every database and runs [`schema_unguard_sql`] where it
//! answers `t`.
//!
//! The functions live in their own schema [`GUARD_SCHEMA`], owned by the superuser that runs
//! the script, with no rights for anyone else; not in `chelabase`, whose tamper check would
//! quarantine them. Every catalog function and operator is schema-qualified and the functions
//! pin `search_path`. The script is one transaction and safe to run again: the triggers and
//! functions are dropped (any signature) and made again each time, and an existing schema is
//! taken back (its owner and every grant). A schema holding anything else (planted while the
//! branch was unprotected: a table, a type, a function of the same name owned by someone
//! else, …) is dropped with all it holds first, so nothing planted can block or shadow the
//! guard.
//!
//! The functions' comments carry a stamp of the rendered script's text (settings included;
//! [`schema_guard_check_sql`] compares it and the bodies), so the reconciler reinstalls only
//! where the guard isn't the current one for the branch's settings.

use std::collections::BTreeSet;

use chelabase_schema::sql::literal;
use chelabase_types::role_name::MEMBER_ROLE_PREFIX;
#[cfg(doc)]
use chelabase_types::table_api::{BRANCH_PROTECTED_HINT, GUARD_SCHEMA, SCHEMA_LOCKED_ROLE};

use crate::OWNER_ROLE;

/// The marker of [`schema_guard_check_sql`] (the fakes' needle).
pub const GUARD_CHECK_MARK: &str = "-- chelabase: schema guard check";

/// The marker of [`schema_guard_present_sql`] (the fakes' needle).
pub const GUARD_PRESENT_MARK: &str = "-- chelabase: schema guard present";

/// The marker of [`schema_unguard_sql`] (the fakes' needle).
pub const UNGUARD_MARK: &str = "-- chelabase: schema unguard";

/// A protected branch's guard settings (PB spec §1): the "guard login roles" switch and the
/// login roles exempt from it, by name.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GuardSettings {
    pub guard_logins: bool,
    pub exempt: BTreeSet<String>,
}

impl GuardSettings {
    /// The exemptions as lowercase hex of their UTF-8 bytes, in the set's order.
    fn exempt_hex(&self) -> Vec<String> {
        self.exempt.iter().map(|name| hex::encode(name.as_bytes())).collect()
    }

    /// `-- settings: guard_logins=<on|off> exempt=[<hex>, <hex>]`, one per body.
    fn line(&self) -> String {
        format!(
            "-- settings: guard_logins={} exempt=[{}]",
            if self.guard_logins { "on" } else { "off" },
            self.exempt_hex().join(", ")
        )
    }
}

/// The guard, as one script for `admin_sql` (`cloud_admin`) in one database. Its names are
/// [`GUARD_SCHEMA`], [`SCHEMA_LOCKED_ROLE`] and [`BRANCH_PROTECTED_HINT`] (a test holds them
/// equal).
pub fn schema_guard_sql(settings: &GuardSettings) -> String {
    let script = guard_script(settings);
    let stamp = guard_stamp(settings);
    format!(
        "{script}COMMENT ON FUNCTION chelabase_guard.refuse_ddl() IS '{stamp}';
COMMENT ON FUNCTION chelabase_guard.refuse_drop() IS '{stamp}';
COMMIT;
"
    )
}

/// A read-only query, for `admin_sql` in one database, answering `t` when the installed guard
/// is the current [`schema_guard_sql`] for `settings`: the schema is a superuser's with no
/// grants and holds exactly the two functions, both a superuser's, stamped with this version,
/// with exactly this version's bodies (a `CREATE OR REPLACE` keeps the comment, so the stamp
/// alone can't tell an older worker's bodies apart; the bodies carry the settings), no
/// `SECURITY DEFINER`, the pinned `search_path`, executable by nobody else, and exactly two
/// event triggers point at them, one on each function's event, `ENABLE ALWAYS`, unfiltered
/// and a superuser's. Anything else answers `f`.
pub fn schema_guard_check_sql(settings: &GuardSettings) -> String {
    let stamp = guard_stamp(settings);
    let ddl = refuse_ddl_body(settings);
    let drop = refuse_drop_body(settings);
    format!(
        "{GUARD_CHECK_MARK}
SELECT
  (SELECT pg_catalog.count(*) FROM pg_catalog.pg_event_trigger e
      JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) e.evtfoid
      JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace
    WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase_guard') OPERATOR(pg_catalog.=) 2
  AND (SELECT pg_catalog.count(DISTINCT p.proname) FROM pg_catalog.pg_event_trigger e
      JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) e.evtfoid
      JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace
      JOIN pg_catalog.pg_roles po ON po.oid OPERATOR(pg_catalog.=) p.proowner
      JOIN pg_catalog.pg_roles eo ON eo.oid OPERATOR(pg_catalog.=) e.evtowner
    WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase_guard'
      AND pg_catalog.starts_with(e.evtname, 'chelabase_schema_guard_')
      AND ((e.evtevent OPERATOR(pg_catalog.=) 'ddl_command_end' AND p.proname OPERATOR(pg_catalog.=) 'refuse_ddl')
        OR (e.evtevent OPERATOR(pg_catalog.=) 'sql_drop' AND p.proname OPERATOR(pg_catalog.=) 'refuse_drop'))
      AND e.evtenabled OPERATOR(pg_catalog.=) 'A' AND e.evttags IS NULL AND eo.rolsuper
      AND p.pronargs OPERATOR(pg_catalog.=) 0 AND po.rolsuper
      AND pg_catalog.obj_description(p.oid, 'pg_proc') OPERATOR(pg_catalog.=) '{stamp}'
      AND p.prosrc OPERATOR(pg_catalog.=) (CASE WHEN p.proname OPERATOR(pg_catalog.=) 'refuse_ddl'
        THEN $b${ddl}$b$ ELSE $b${drop}$b$ END)
      AND NOT p.prosecdef
      AND p.proconfig OPERATOR(pg_catalog.=) ARRAY['search_path=pg_catalog, pg_temp']::pg_catalog.text[]
      AND p.proacl IS NOT NULL
      AND NOT EXISTS (SELECT FROM pg_catalog.aclexplode(p.proacl) a WHERE a.grantee OPERATOR(pg_catalog.<>) p.proowner)
  ) OPERATOR(pg_catalog.=) 2
  AND EXISTS (SELECT FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner
    WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase_guard' AND r.rolsuper
      AND NOT EXISTS (SELECT FROM pg_catalog.aclexplode(n.nspacl) a WHERE a.grantee OPERATOR(pg_catalog.<>) n.nspowner))
  AND (SELECT pg_catalog.count(*) FROM pg_catalog.pg_depend d
      JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) d.refobjid
    WHERE d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_namespace'::pg_catalog.regclass
      AND n.nspname OPERATOR(pg_catalog.=) 'chelabase_guard') OPERATOR(pg_catalog.=) 2;
"
    )
}

/// A read-only query, for `admin_sql` in one database, answering `t` when either event trigger
/// of the guard or its schema [`GUARD_SCHEMA`] exists (an unprotected branch's reconcile asks
/// it before removing anything, so a normal run makes no DDL).
pub fn schema_guard_present_sql() -> String {
    format!(
        "{GUARD_PRESENT_MARK}
SELECT EXISTS (SELECT FROM pg_catalog.pg_event_trigger e
    WHERE e.evtname OPERATOR(pg_catalog.=) 'chelabase_schema_guard_ddl'
      OR e.evtname OPERATOR(pg_catalog.=) 'chelabase_schema_guard_drop')
  OR EXISTS (SELECT FROM pg_catalog.pg_namespace n WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase_guard');
"
    )
}

/// Removes the guard, as one transaction for `admin_sql` (`cloud_admin`) in one database: both
/// event triggers, then schema [`GUARD_SCHEMA`] with all it holds (S2). The marker role stays
/// (a cluster-wide role, granted to nobody on an unprotected branch).
pub fn schema_unguard_sql() -> String {
    format!(
        "{UNGUARD_MARK}
BEGIN;
SET search_path = pg_catalog, pg_temp;
DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_ddl;
DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_drop;
DROP SCHEMA IF EXISTS chelabase_guard CASCADE;
COMMIT;
"
    )
}

/// `chelabase schema guard <hash of the rendered script>`: any change to the guard or its
/// settings changes it.
fn guard_stamp(settings: &GuardSettings) -> String {
    // FNV-1a, 64 bits: stable across builds and platforms.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in guard_script(settings).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("chelabase schema guard {hash:016x}")
}

/// How both functions start (PB spec §2): a superuser session returns at once, then data
/// maintenance; then the session is locked when it holds the marker (looked up only once the
/// marker role is known to exist: `pg_has_role` fails for a missing role) or, with the switch
/// on, when its `session_user` isn't the owner, a member login or an exemption. An
/// unlocked session returns.
fn locked_or_return(settings: &GuardSettings) -> String {
    let exempt = settings
        .exempt_hex()
        .iter()
        .map(|h| format!("'{h}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "DECLARE
    locked boolean := false;
  BEGIN
    {line}
    IF EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) session_user AND rolsuper) THEN RETURN; END IF;{MAINTENANCE_RETURNS}
    IF EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_schema_locked') THEN
      locked := pg_catalog.pg_has_role(session_user, 'chelabase_schema_locked', 'MEMBER');
    END IF;
    IF NOT locked AND {on} THEN
      locked := session_user OPERATOR(pg_catalog.<>) {owner}
        AND NOT pg_catalog.starts_with(session_user, {member})
        AND NOT (pg_catalog.encode(pg_catalog.convert_to(session_user, 'UTF8'), 'hex')
          OPERATOR(pg_catalog.=) ANY (ARRAY[{exempt}]::pg_catalog.text[]));
    END IF;
    IF NOT locked THEN RETURN; END IF;",
        line = settings.line(),
        on = if settings.guard_logins { "true" } else { "false" },
        owner = literal(OWNER_ROLE),
        member = literal(MEMBER_ROLE_PREFIX),
    )
}

const REFUSAL: &str = "RAISE EXCEPTION 'branch is protected: schema changes need Admin' USING ERRCODE = '42501', HINT = 'chelabase:branch_protected';";

/// Data maintenance (T7 review): rebuilding a materialized view's rows or an index changes no
/// schema, so both functions let it through.
const MAINTENANCE_RETURNS: &str = "
    IF TG_TAG OPERATOR(pg_catalog.=) 'REFRESH MATERIALIZED VIEW' OR TG_TAG OPERATOR(pg_catalog.=) 'REINDEX' THEN RETURN; END IF;";

/// `refuse_ddl`'s body (its `prosrc`, which [`schema_guard_check_sql`] compares).
fn refuse_ddl_body(settings: &GuardSettings) -> String {
    format!(
        "{start}
    IF EXISTS (SELECT FROM pg_catalog.pg_event_trigger_ddl_commands() c
        WHERE c.schema_name IS NULL OR NOT pg_catalog.starts_with(c.schema_name, 'pg_temp'))
      OR EXISTS (SELECT FROM pg_catalog.pg_event_trigger_ddl_commands() c
          JOIN pg_catalog.pg_inherits i ON i.inhrelid OPERATOR(pg_catalog.=) c.objid
          JOIN pg_catalog.pg_class p ON p.oid OPERATOR(pg_catalog.=) i.inhparent
        WHERE c.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
          AND p.relpersistence OPERATOR(pg_catalog.<>) 't') THEN
      {REFUSAL}
    END IF;
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_event_trigger_ddl_commands())
        AND (NOT pg_catalog.starts_with(TG_TAG, 'DROP ') OR TG_TAG OPERATOR(pg_catalog.=) 'DROP OWNED') THEN
      {REFUSAL}
    END IF;
  END",
        start = locked_or_return(settings),
    )
}

/// `refuse_drop`'s body (its `prosrc`).
fn refuse_drop_body(settings: &GuardSettings) -> String {
    format!(
        "{start}
    IF EXISTS (SELECT FROM pg_catalog.pg_event_trigger_dropped_objects() o WHERE NOT o.is_temporary) THEN
      {REFUSAL}
    END IF;
  END",
        start = locked_or_return(settings),
    )
}

/// The script up to its stamp and `COMMIT` ([`schema_guard_sql`] adds them).
fn guard_script(settings: &GuardSettings) -> String {
    [GUARD_SQL_HEAD, &refuse_ddl_body(settings), GUARD_SQL_MID, &refuse_drop_body(settings), GUARD_SQL_TAIL].concat()
}

const GUARD_SQL_HEAD: &str = "BEGIN;
SET search_path = pg_catalog, pg_temp;
DO $$BEGIN IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_schema_locked') THEN CREATE ROLE chelabase_schema_locked NOLOGIN; END IF; END$$;
DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_ddl;
DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_drop;
DO $$BEGIN
  IF EXISTS (SELECT FROM pg_catalog.pg_namespace n WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase_guard'
      AND (NOT EXISTS (SELECT FROM pg_catalog.pg_roles r WHERE r.oid OPERATOR(pg_catalog.=) n.nspowner AND r.rolsuper)
        OR EXISTS (SELECT FROM pg_catalog.pg_depend d
          WHERE d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_namespace'::pg_catalog.regclass
            AND d.refobjid OPERATOR(pg_catalog.=) n.oid
            AND NOT EXISTS (SELECT FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) p.proowner
              WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass
                AND p.oid OPERATOR(pg_catalog.=) d.objid AND r.rolsuper
                AND (p.proname OPERATOR(pg_catalog.=) 'refuse_ddl' OR p.proname OPERATOR(pg_catalog.=) 'refuse_drop'))))) THEN
    DROP SCHEMA chelabase_guard CASCADE;
  END IF;
END$$;
CREATE SCHEMA IF NOT EXISTS chelabase_guard;
ALTER SCHEMA chelabase_guard OWNER TO CURRENT_USER;
REVOKE ALL ON SCHEMA chelabase_guard FROM PUBLIC;
DO $$DECLARE g pg_catalog.name; BEGIN
  FOR g IN SELECT DISTINCT r.rolname FROM pg_catalog.pg_namespace n
      CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) a
      JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.grantee
    WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase_guard' AND a.grantee OPERATOR(pg_catalog.<>) n.nspowner
  LOOP
    EXECUTE pg_catalog.format('REVOKE ALL ON SCHEMA chelabase_guard FROM %I CASCADE', g);
  END LOOP;
END$$;
DO $$DECLARE f pg_catalog.regprocedure; BEGIN
  FOR f IN SELECT p.oid FROM pg_catalog.pg_proc p
      JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace
    WHERE n.nspname OPERATOR(pg_catalog.=) 'chelabase_guard'
      AND (p.proname OPERATOR(pg_catalog.=) 'refuse_ddl' OR p.proname OPERATOR(pg_catalog.=) 'refuse_drop')
  LOOP
    EXECUTE pg_catalog.format('DROP ROUTINE IF EXISTS %s CASCADE', f);
  END LOOP;
END$$;
CREATE OR REPLACE FUNCTION chelabase_guard.refuse_ddl() RETURNS event_trigger
  LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS
  $f$";

const GUARD_SQL_MID: &str = "$f$;
CREATE OR REPLACE FUNCTION chelabase_guard.refuse_drop() RETURNS event_trigger
  LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS
  $f$";

const GUARD_SQL_TAIL: &str = "$f$;
ALTER FUNCTION chelabase_guard.refuse_ddl() OWNER TO CURRENT_USER;
ALTER FUNCTION chelabase_guard.refuse_drop() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION chelabase_guard.refuse_ddl(), chelabase_guard.refuse_drop() FROM PUBLIC;
CREATE EVENT TRIGGER chelabase_schema_guard_ddl ON ddl_command_end EXECUTE FUNCTION chelabase_guard.refuse_ddl();
ALTER EVENT TRIGGER chelabase_schema_guard_ddl ENABLE ALWAYS;
CREATE EVENT TRIGGER chelabase_schema_guard_drop ON sql_drop EXECUTE FUNCTION chelabase_guard.refuse_drop();
ALTER EVENT TRIGGER chelabase_schema_guard_drop ENABLE ALWAYS;
";

/// The settings the TE-A text tests render the guard with: the switch on and exemptions, and
/// the switch off with none (each property holds for both).
#[cfg(test)]
fn test_settings() -> [GuardSettings; 2] {
    [
        GuardSettings {
            guard_logins: true,
            exempt: ["deploy".to_string(), "Ci Bot".to_string()].into_iter().collect(),
        },
        GuardSettings::default(),
    ]
}

#[cfg(test)]
mod guard_text {
    use super::*;
    use chelabase_types::table_api::{BRANCH_PROTECTED_HINT, GUARD_SCHEMA, SCHEMA_LOCKED_ROLE};

    fn body(sql: &str, function: &str) -> String {
        let start = sql
            .find(&format!("CREATE OR REPLACE FUNCTION {function}()"))
            .unwrap();
        let rest = &sql[start..];
        rest[..rest.find("END$f$;").unwrap()].to_string()
    }

    #[test]
    fn the_names_are_the_shared_constants() {
        for settings in test_settings() {
            let sql = schema_guard_sql(&settings);
            assert!(sql.contains(&format!("CREATE SCHEMA IF NOT EXISTS {GUARD_SCHEMA};")));
            assert!(sql.contains(&format!("CREATE ROLE {SCHEMA_LOCKED_ROLE} NOLOGIN")));
            assert!(sql.contains(&format!("HINT = '{BRANCH_PROTECTED_HINT}'")));
            assert!(sql.contains(&format!("pg_catalog.starts_with(session_user, '{}')", chelabase_types::role_name::MEMBER_ROLE_PREFIX)));
            assert!(sql.contains(&format!("session_user OPERATOR(pg_catalog.<>) '{}'", crate::OWNER_ROLE)));
        }
    }

    #[test]
    fn the_script_is_one_transaction_with_a_pinned_search_path() {
        for settings in test_settings() {
            let sql = schema_guard_sql(&settings);
            assert!(sql.starts_with("BEGIN;\nSET search_path = pg_catalog, pg_temp;\n"), "{sql}");
            assert!(sql.trim_end().ends_with("COMMIT;"), "{sql}");
            // Each trigger is dropped just before it is made again, inside the transaction.
            for t in ["chelabase_schema_guard_ddl", "chelabase_schema_guard_drop"] {
                let drop = sql.find(&format!("DROP EVENT TRIGGER IF EXISTS {t};")).unwrap();
                let create = sql.find(&format!("CREATE EVENT TRIGGER {t} ")).unwrap();
                assert!(drop < create);
            }
        }
    }

    // PB T3: the order is now superuser, maintenance, then "locked" (the marker, looked up only
    // once the marker role is known to exist, or the login rule); the superuser and missing-role
    // checks still come before `pg_has_role`, and every refusal after the "not locked" return.
    #[test]
    fn both_functions_rule_out_superusers_and_a_missing_role_before_pg_has_role() {
        for settings in test_settings() {
            let sql = schema_guard_sql(&settings);
            for f in ["chelabase_guard.refuse_ddl", "chelabase_guard.refuse_drop"] {
                let b = body(&sql, f);
                let superuser = b.find("session_user AND rolsuper) THEN RETURN;").unwrap();
                let exists = b.find("IF EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_schema_locked') THEN\n      locked := pg_catalog.pg_has_role(").unwrap();
                let member = b.find("pg_catalog.pg_has_role(session_user, 'chelabase_schema_locked', 'MEMBER');").unwrap();
                let login = b.find("locked := session_user OPERATOR(pg_catalog.<>)").unwrap();
                let unlocked = b.find("IF NOT locked THEN RETURN; END IF;").unwrap();
                assert!(superuser < exists && exists < member && member < login && login < unlocked, "{b}");
                assert!(b.find("RAISE EXCEPTION").unwrap() > unlocked, "{b}");
                assert!(b.contains("LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS"), "{b}");
                assert!(!b.contains("SECURITY DEFINER"), "{b}");
            }
        }
    }

    #[test]
    fn the_rules_allow_only_temporary_objects() {
        for settings in test_settings() {
            let sql = schema_guard_sql(&settings);
            let ddl = body(&sql, "chelabase_guard.refuse_ddl");
            assert!(ddl.contains("c.schema_name IS NULL OR NOT pg_catalog.starts_with(c.schema_name, 'pg_temp')"));
            assert!(ddl.contains("NOT pg_catalog.starts_with(TG_TAG, 'DROP ') OR TG_TAG OPERATOR(pg_catalog.=) 'DROP OWNED'"));
            assert_eq!(ddl.matches("RAISE EXCEPTION").count(), 2);
            let drop = body(&sql, "chelabase_guard.refuse_drop");
            assert!(drop.contains("pg_catalog.pg_event_trigger_dropped_objects() o WHERE NOT o.is_temporary"));
            assert_eq!(drop.matches("RAISE EXCEPTION").count(), 1);
        }
    }

    // PB T3: also over the presence and removal scripts, and `encode` / `convert_to` (the
    // login rule's exemption comparison).
    #[test]
    fn every_function_and_operator_is_qualified() {
        for settings in test_settings() {
            for sql in [schema_guard_sql(&settings), schema_guard_check_sql(&settings), schema_guard_present_sql(), schema_unguard_sql()] {
                for f in ["pg_has_role(", "starts_with(", "pg_event_trigger_ddl_commands(", "pg_event_trigger_dropped_objects(", "aclexplode(", "format(", "encode(", "convert_to(", "count(", "obj_description("] {
                    for (at, _) in sql.match_indices(f) {
                        assert!(sql[..at].ends_with("pg_catalog."), "{f} at {at}");
                    }
                }
                // Comparisons go through OPERATOR(pg_catalog.…).
                for op in [" = ", " <> "] {
                    for (at, _) in sql.match_indices(op) {
                        let line_start = sql[..at].rfind('\n').map_or(0, |i| i + 1);
                        let line = &sql[line_start..sql[at..].find('\n').map_or(sql.len(), |i| at + i)];
                        assert!(
                            line.starts_with("SET search_path = ") || line.contains("search_path = pg_catalog, pg_temp AS") || line.contains("ERRCODE = "),
                            "a bare {op:?}: {line}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn nobody_else_keeps_rights_on_the_schema_or_functions() {
        for settings in test_settings() {
            let sql = schema_guard_sql(&settings);
            assert!(sql.contains("ALTER SCHEMA chelabase_guard OWNER TO CURRENT_USER;"));
            assert!(sql.contains("REVOKE ALL ON SCHEMA chelabase_guard FROM %I CASCADE"));
            assert!(sql.contains("ALTER FUNCTION chelabase_guard.refuse_ddl() OWNER TO CURRENT_USER;"));
            assert!(sql.contains("ALTER FUNCTION chelabase_guard.refuse_drop() OWNER TO CURRENT_USER;"));
            assert!(sql.contains("REVOKE ALL ON FUNCTION chelabase_guard.refuse_ddl(), chelabase_guard.refuse_drop() FROM PUBLIC;"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_sql_is_idempotent_and_complete() {
        for settings in test_settings() {
            let sql = schema_guard_sql(&settings);
            for part in [
                "CREATE SCHEMA IF NOT EXISTS chelabase_guard",
                "REVOKE ALL ON SCHEMA chelabase_guard FROM PUBLIC",
                "CREATE OR REPLACE FUNCTION chelabase_guard.refuse_ddl()",
                "CREATE OR REPLACE FUNCTION chelabase_guard.refuse_drop()",
                "DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_ddl",
                "CREATE EVENT TRIGGER chelabase_schema_guard_ddl ON ddl_command_end",
                "DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_drop",
                "CREATE EVENT TRIGGER chelabase_schema_guard_drop ON sql_drop",
                "pg_has_role(session_user, 'chelabase_schema_locked', 'MEMBER')",
                "ERRCODE = '42501'",
                "HINT = 'chelabase:branch_protected'",
                "SET search_path = pg_catalog, pg_temp",
            ] {
                assert!(sql.contains(part), "{part}\n{sql}");
            }
        }
    }

    // T7 review, findings 2, 4, 5, 6 and 7, in the SQL text (the live test proves them).
    #[test]
    fn the_guard_sql_covers_the_review() {
        for settings in test_settings() {
            let sql = schema_guard_sql(&settings);
            for part in [
                // 6: replicas don't skip the guard.
                "ALTER EVENT TRIGGER chelabase_schema_guard_ddl ENABLE ALWAYS",
                "ALTER EVENT TRIGGER chelabase_schema_guard_drop ENABLE ALWAYS",
                // 4: data maintenance stays allowed.
                "'REFRESH MATERIALIZED VIEW'",
                "'REINDEX'",
                // 5: a temporary table inheriting from a permanent one is a schema change.
                "pg_catalog.pg_inherits",
            ] {
                assert!(sql.contains(part), "{part}\n{sql}");
            }
            // 7: the check is a cheap, separate query answering `t` when the guard is current.
            let check = schema_guard_check_sql(&settings);
            assert!(check.contains("chelabase: schema guard check"), "{check}");
            assert!(!check.contains("CREATE "), "the check changes nothing: {check}");
        }
    }
}

#[cfg(test)]
mod stamp {
    use super::*;

    #[test]
    fn the_check_looks_for_the_stamp_the_install_writes() {
        for settings in test_settings() {
            let stamp = guard_stamp(&settings);
            assert!(stamp.starts_with("chelabase schema guard "), "{stamp}");
            let sql = schema_guard_sql(&settings);
            assert!(sql.contains(&format!("COMMENT ON FUNCTION chelabase_guard.refuse_ddl() IS '{stamp}';")));
            assert!(sql.contains(&format!("COMMENT ON FUNCTION chelabase_guard.refuse_drop() IS '{stamp}';")));
            // The stamp comes before COMMIT, in the same transaction.
            assert!(sql.find(&stamp).unwrap() < sql.rfind("COMMIT;").unwrap());
            let check = schema_guard_check_sql(&settings);
            assert!(check.starts_with(GUARD_CHECK_MARK), "{check}");
            assert!(check.contains(&format!("OPERATOR(pg_catalog.=) '{stamp}'")), "{check}");
            // Read-only: one SELECT, nothing else (outside the bodies it compares as text).
            let outside = check
                .replace(&refuse_ddl_body(&settings), "")
                .replace(&refuse_drop_body(&settings), "");
            for word in ["INSERT", "UPDATE", "DELETE", "ALTER", "DROP", "COMMENT", "GRANT", "REVOKE"] {
                assert!(!outside.contains(word), "{word}: {outside}");
            }
        }
    }

    // T7 fix round, live finding 4: a REFRESH was refused by an older `refuse_ddl` body (no
    // maintenance check) that had replaced ours under our comment (`CREATE OR REPLACE` keeps
    // it), and the stamp-only check still answered `t`. The check now compares the bodies,
    // and each installed body lets maintenance through before any refusal. PB T3: maintenance
    // now comes right after the superuser return, so before `pg_has_role` too.
    #[test]
    fn the_check_compares_the_bodies_and_they_allow_maintenance_first() {
        for settings in test_settings() {
            let sql = schema_guard_sql(&settings);
            let check = schema_guard_check_sql(&settings);
            for body in [refuse_ddl_body(&settings), refuse_drop_body(&settings)] {
                assert!(sql.contains(&format!("$f${body}$f$;")), "installed as is: {body}");
                assert!(check.contains(&format!("$b${body}$b$")), "compared as is: {body}");
                assert!(!body.contains("$b$") && !body.contains("$f$"));
                let maintenance = body
                    .find("IF TG_TAG OPERATOR(pg_catalog.=) 'REFRESH MATERIALIZED VIEW' OR TG_TAG OPERATOR(pg_catalog.=) 'REINDEX' THEN RETURN; END IF;")
                    .expect("maintenance allowed");
                assert!(maintenance < body.find("RAISE EXCEPTION").unwrap(), "{body}");
                assert!(maintenance > body.find("rolsuper) THEN RETURN;").unwrap(), "{body}");
                assert!(maintenance < body.find("pg_has_role").unwrap(), "{body}");
            }
            assert!(check.contains("AND NOT p.prosecdef"));
            assert!(check.contains("p.proconfig OPERATOR(pg_catalog.=) ARRAY['search_path=pg_catalog, pg_temp']"));
        }
    }

    #[test]
    fn planted_objects_go_before_the_functions_are_made() {
        for settings in test_settings() {
            let sql = schema_guard_sql(&settings);
            let planted = sql.find("DROP SCHEMA chelabase_guard CASCADE;").unwrap();
            let routines = sql.find("DROP ROUTINE IF EXISTS %s CASCADE").unwrap();
            let create = sql.find("CREATE OR REPLACE FUNCTION chelabase_guard.refuse_ddl()").unwrap();
            let schema = sql.find("CREATE SCHEMA IF NOT EXISTS chelabase_guard;").unwrap();
            assert!(planted < schema && schema < routines && routines < create, "{sql}");
            // Both triggers are dropped before any function is.
            for t in ["chelabase_schema_guard_ddl", "chelabase_schema_guard_drop"] {
                assert!(sql.find(&format!("DROP EVENT TRIGGER IF EXISTS {t};")).unwrap() < planted);
            }
        }
    }
}

// PB, T3: the branch's settings in the bodies.
#[cfg(test)]
mod settings_text {
    use super::*;

    fn s(on: bool, exempt: &[&str]) -> GuardSettings {
        GuardSettings { guard_logins: on, exempt: exempt.iter().map(|e| e.to_string()).collect() }
    }

    #[test]
    fn the_settings_line_is_in_both_bodies() {
        let sql = schema_guard_sql(&s(true, &["deploy", "Ci Bot"]));
        // "Ci Bot" and "deploy", hex of their UTF-8 bytes, in the set's order.
        assert_eq!(sql.matches("-- settings: guard_logins=on exempt=[436920426f74, 6465706c6f79]").count(), 2, "{sql}");
        let off = schema_guard_sql(&s(false, &[]));
        assert_eq!(off.matches("-- settings: guard_logins=off exempt=[]").count(), 2, "{off}");
    }

    #[test]
    fn a_settings_change_changes_the_check_and_the_stamp() {
        let (a, b, c) = (s(true, &["deploy"]), s(true, &[]), s(false, &["deploy"]));
        assert_ne!(schema_guard_check_sql(&a), schema_guard_check_sql(&b));
        assert_ne!(schema_guard_check_sql(&a), schema_guard_check_sql(&c));
        assert_eq!(schema_guard_check_sql(&a), schema_guard_check_sql(&s(true, &["deploy"])));
        let stamp = |x: &GuardSettings| {
            let sql = schema_guard_sql(x);
            let at = sql.find("IS 'chelabase schema guard ").unwrap();
            sql[at..at + 44].to_string()
        };
        assert_ne!(stamp(&a), stamp(&b));
        assert_ne!(stamp(&a), stamp(&c));
    }

    #[test]
    fn the_login_rule_reads_the_catalog_and_stays_invoker() {
        let sql = schema_guard_sql(&s(true, &["deploy"]));
        for part in ["session_user", "'cb_m_'", "rolsuper", "pg_catalog.encode(pg_catalog.convert_to(session_user, 'UTF8'), 'hex')"] {
            assert!(sql.contains(part), "{part}: {sql}");
        }
        // T3 review: who logged in, not who may log in now (a NOLOGIN flip mid-session).
        assert!(!sql.contains("rolcanlogin"), "{sql}");
        assert!(!sql.contains("SECURITY DEFINER"));
    }

    #[test]
    fn hostile_names_never_reach_the_text() {
        let name = "x$f$y'; DROP SCHEMA public; --";
        let sql = schema_guard_sql(&s(true, &[name]));
        assert!(!sql.contains("x$f$y") && !sql.contains("DROP SCHEMA public"), "{sql}");
        assert!(sql.contains(&hex::encode(name.as_bytes())), "{sql}");
    }

    #[test]
    fn presence_and_removal_scripts() {
        let present = schema_guard_present_sql();
        assert!(present.starts_with(GUARD_PRESENT_MARK), "{present}");
        for word in ["DROP", "CREATE", "ALTER", "COMMENT"] {
            assert!(!present.contains(word), "{word}: {present}");
        }
        let unguard = schema_unguard_sql();
        assert!(unguard.starts_with(UNGUARD_MARK), "{unguard}");
        let ddl = unguard.find("DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_ddl;").unwrap();
        let drop = unguard.find("DROP EVENT TRIGGER IF EXISTS chelabase_schema_guard_drop;").unwrap();
        let schema = unguard.find("DROP SCHEMA IF EXISTS chelabase_guard CASCADE;").unwrap();
        assert!(ddl < schema && drop < schema, "{unguard}");
        assert!(unguard.contains("BEGIN;") && unguard.trim_end().ends_with("COMMIT;"), "{unguard}");
    }
}
