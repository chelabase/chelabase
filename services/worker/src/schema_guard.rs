//! The DDL guard of protected branches (TE-A spec §5, rulings P5–P7).
//!
//! On a protected branch the member-role reconciler grants the marker role
//! `chelabase_schema_locked` ([`SCHEMA_LOCKED_ROLE`]) to every member role whose member isn't
//! an Admin, and runs [`schema_guard_sql`] in every database of the branch. Two event triggers
//! then refuse any schema change made by a session logged in as a marked role, whatever role
//! it `SET ROLE`s to:
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
//! of every role, so the functions rule one out before `pg_has_role`. Nobody holds the marker
//! on an unprotected branch, so triggers left there do nothing.
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
//! The functions' comments carry a stamp of the script's text ([`schema_guard_check_sql`]
//! compares it), so the reconciler reinstalls only where the guard isn't the current one.

#[cfg(doc)]
use chelabase_types::table_api::{BRANCH_PROTECTED_HINT, GUARD_SCHEMA, SCHEMA_LOCKED_ROLE};

/// The marker of [`schema_guard_check_sql`] (the fakes' needle).
pub const GUARD_CHECK_MARK: &str = "-- chelabase: schema guard check";

/// The guard, as one script for `admin_sql` (`cloud_admin`) in one database. Its names are
/// [`GUARD_SCHEMA`], [`SCHEMA_LOCKED_ROLE`] and [`BRANCH_PROTECTED_HINT`] (a test holds them
/// equal).
pub fn schema_guard_sql() -> String {
    let stamp = guard_stamp();
    format!(
        "{GUARD_SQL}COMMENT ON FUNCTION chelabase_guard.refuse_ddl() IS '{stamp}';
COMMENT ON FUNCTION chelabase_guard.refuse_drop() IS '{stamp}';
COMMIT;
"
    )
}

/// A read-only query, for `admin_sql` in one database, answering `t` when the installed guard
/// is the current [`schema_guard_sql`]: the schema is a superuser's with no grants and holds
/// exactly the two functions, both a superuser's, stamped with this version, with exactly this
/// version's bodies (a `CREATE OR REPLACE` keeps the comment, so the stamp alone can't tell an
/// older worker's bodies apart), no `SECURITY DEFINER`, the pinned `search_path`, executable
/// by nobody else, and exactly two event triggers point at them, one on each function's event,
/// `ENABLE ALWAYS`, unfiltered and a superuser's. Anything else answers `f`.
pub fn schema_guard_check_sql() -> String {
    let stamp = guard_stamp();
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
        THEN $b${REFUSE_DDL_BODY}$b$ ELSE $b${REFUSE_DROP_BODY}$b$ END)
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

/// `chelabase schema guard <hash of the script>`: any change to the guard changes it.
fn guard_stamp() -> String {
    // FNV-1a, 64 bits: stable across builds and platforms.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in GUARD_SQL.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("chelabase schema guard {hash:016x}")
}

/// How both functions start: no marker role, a superuser session or an unmarked session
/// return at once. Separate statements, so `pg_has_role` (which fails for a missing role) only
/// runs once the role is known to exist, and never decides for a superuser.
macro_rules! locked_or_return {
    () => {
        "IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_schema_locked') THEN RETURN; END IF;
    IF EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) session_user AND rolsuper) THEN RETURN; END IF;
    IF NOT pg_catalog.pg_has_role(session_user, 'chelabase_schema_locked', 'MEMBER') THEN RETURN; END IF;"
    };
}

macro_rules! refusal {
    () => {
        "RAISE EXCEPTION 'branch is protected: schema changes need Admin' USING ERRCODE = '42501', HINT = 'chelabase:branch_protected';"
    };
}

/// Data maintenance (T7 review): rebuilding a materialized view's rows or an index changes no
/// schema, so both functions let it through.
macro_rules! maintenance_returns {
    () => {
        "
    IF TG_TAG OPERATOR(pg_catalog.=) 'REFRESH MATERIALIZED VIEW' OR TG_TAG OPERATOR(pg_catalog.=) 'REINDEX' THEN RETURN; END IF;"
    };
}

/// `refuse_ddl`'s body (its `prosrc`, which [`schema_guard_check_sql`] compares).
macro_rules! refuse_ddl_body {
    () => {
        concat!(
            "BEGIN
    ",
            locked_or_return!(),
            maintenance_returns!(),
            "
    IF EXISTS (SELECT FROM pg_catalog.pg_event_trigger_ddl_commands() c
        WHERE c.schema_name IS NULL OR NOT pg_catalog.starts_with(c.schema_name, 'pg_temp'))
      OR EXISTS (SELECT FROM pg_catalog.pg_event_trigger_ddl_commands() c
          JOIN pg_catalog.pg_inherits i ON i.inhrelid OPERATOR(pg_catalog.=) c.objid
          JOIN pg_catalog.pg_class p ON p.oid OPERATOR(pg_catalog.=) i.inhparent
        WHERE c.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
          AND p.relpersistence OPERATOR(pg_catalog.<>) 't') THEN
      ",
            refusal!(),
            "
    END IF;
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_event_trigger_ddl_commands())
        AND (NOT pg_catalog.starts_with(TG_TAG, 'DROP ') OR TG_TAG OPERATOR(pg_catalog.=) 'DROP OWNED') THEN
      ",
            refusal!(),
            "
    END IF;
  END"
        )
    };
}

/// `refuse_drop`'s body (its `prosrc`).
macro_rules! refuse_drop_body {
    () => {
        concat!(
            "BEGIN
    ",
            locked_or_return!(),
            maintenance_returns!(),
            "
    IF EXISTS (SELECT FROM pg_catalog.pg_event_trigger_dropped_objects() o WHERE NOT o.is_temporary) THEN
      ",
            refusal!(),
            "
    END IF;
  END"
        )
    };
}

const REFUSE_DDL_BODY: &str = refuse_ddl_body!();
const REFUSE_DROP_BODY: &str = refuse_drop_body!();

/// The script up to its stamp and `COMMIT` ([`schema_guard_sql`] adds them).
const GUARD_SQL: &str = concat!(
    "BEGIN;
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
  $f$",
    refuse_ddl_body!(),
    "$f$;
CREATE OR REPLACE FUNCTION chelabase_guard.refuse_drop() RETURNS event_trigger
  LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS
  $f$",
    refuse_drop_body!(),
    "$f$;
ALTER FUNCTION chelabase_guard.refuse_ddl() OWNER TO CURRENT_USER;
ALTER FUNCTION chelabase_guard.refuse_drop() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION chelabase_guard.refuse_ddl(), chelabase_guard.refuse_drop() FROM PUBLIC;
CREATE EVENT TRIGGER chelabase_schema_guard_ddl ON ddl_command_end EXECUTE FUNCTION chelabase_guard.refuse_ddl();
ALTER EVENT TRIGGER chelabase_schema_guard_ddl ENABLE ALWAYS;
CREATE EVENT TRIGGER chelabase_schema_guard_drop ON sql_drop EXECUTE FUNCTION chelabase_guard.refuse_drop();
ALTER EVENT TRIGGER chelabase_schema_guard_drop ENABLE ALWAYS;
"
);

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
        let sql = schema_guard_sql();
        assert!(sql.contains(&format!("CREATE SCHEMA IF NOT EXISTS {GUARD_SCHEMA};")));
        assert!(sql.contains(&format!("CREATE ROLE {SCHEMA_LOCKED_ROLE} NOLOGIN")));
        assert!(sql.contains(&format!("HINT = '{BRANCH_PROTECTED_HINT}'")));
    }

    #[test]
    fn the_script_is_one_transaction_with_a_pinned_search_path() {
        let sql = schema_guard_sql();
        assert!(sql.starts_with("BEGIN;\nSET search_path = pg_catalog, pg_temp;\n"), "{sql}");
        assert!(sql.trim_end().ends_with("COMMIT;"), "{sql}");
        // Each trigger is dropped just before it is made again, inside the transaction.
        for t in ["chelabase_schema_guard_ddl", "chelabase_schema_guard_drop"] {
            let drop = sql.find(&format!("DROP EVENT TRIGGER IF EXISTS {t};")).unwrap();
            let create = sql.find(&format!("CREATE EVENT TRIGGER {t} ")).unwrap();
            assert!(drop < create);
        }
    }

    #[test]
    fn both_functions_rule_out_superusers_and_a_missing_role_before_pg_has_role() {
        let sql = schema_guard_sql();
        for f in ["chelabase_guard.refuse_ddl", "chelabase_guard.refuse_drop"] {
            let b = body(&sql, f);
            let exists = b.find("rolname OPERATOR(pg_catalog.=) 'chelabase_schema_locked') THEN RETURN;").unwrap();
            let superuser = b.find("session_user AND rolsuper) THEN RETURN;").unwrap();
            let member = b.find("IF NOT pg_catalog.pg_has_role(session_user, 'chelabase_schema_locked', 'MEMBER') THEN RETURN;").unwrap();
            assert!(exists < superuser && superuser < member, "{b}");
            assert!(b.find("RAISE EXCEPTION").unwrap() > member, "{b}");
            assert!(b.contains("LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS"), "{b}");
            assert!(!b.contains("SECURITY DEFINER"), "{b}");
        }
    }

    #[test]
    fn the_rules_allow_only_temporary_objects() {
        let sql = schema_guard_sql();
        let ddl = body(&sql, "chelabase_guard.refuse_ddl");
        assert!(ddl.contains("c.schema_name IS NULL OR NOT pg_catalog.starts_with(c.schema_name, 'pg_temp')"));
        assert!(ddl.contains("NOT pg_catalog.starts_with(TG_TAG, 'DROP ') OR TG_TAG OPERATOR(pg_catalog.=) 'DROP OWNED'"));
        assert_eq!(ddl.matches("RAISE EXCEPTION").count(), 2);
        let drop = body(&sql, "chelabase_guard.refuse_drop");
        assert!(drop.contains("pg_catalog.pg_event_trigger_dropped_objects() o WHERE NOT o.is_temporary"));
        assert_eq!(drop.matches("RAISE EXCEPTION").count(), 1);
    }

    #[test]
    fn every_function_and_operator_is_qualified() {
        let sql = schema_guard_sql();
        for f in ["pg_has_role(", "starts_with(", "pg_event_trigger_ddl_commands(", "pg_event_trigger_dropped_objects(", "aclexplode(", "format("] {
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

    #[test]
    fn nobody_else_keeps_rights_on_the_schema_or_functions() {
        let sql = schema_guard_sql();
        assert!(sql.contains("ALTER SCHEMA chelabase_guard OWNER TO CURRENT_USER;"));
        assert!(sql.contains("REVOKE ALL ON SCHEMA chelabase_guard FROM %I CASCADE"));
        assert!(sql.contains("ALTER FUNCTION chelabase_guard.refuse_ddl() OWNER TO CURRENT_USER;"));
        assert!(sql.contains("ALTER FUNCTION chelabase_guard.refuse_drop() OWNER TO CURRENT_USER;"));
        assert!(sql.contains("REVOKE ALL ON FUNCTION chelabase_guard.refuse_ddl(), chelabase_guard.refuse_drop() FROM PUBLIC;"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_sql_is_idempotent_and_complete() {
        let sql = schema_guard_sql();
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

    // T7 review, findings 2, 4, 5, 6 and 7, in the SQL text (the live test proves them).
    #[test]
    fn the_guard_sql_covers_the_review() {
        let sql = schema_guard_sql();
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
        let check = schema_guard_check_sql();
        assert!(check.contains("chelabase: schema guard check"), "{check}");
        assert!(!check.contains("CREATE "), "the check changes nothing: {check}");
    }
}

#[cfg(test)]
mod stamp {
    use super::*;

    #[test]
    fn the_check_looks_for_the_stamp_the_install_writes() {
        let stamp = guard_stamp();
        assert!(stamp.starts_with("chelabase schema guard "), "{stamp}");
        let sql = schema_guard_sql();
        assert!(sql.contains(&format!("COMMENT ON FUNCTION chelabase_guard.refuse_ddl() IS '{stamp}';")));
        assert!(sql.contains(&format!("COMMENT ON FUNCTION chelabase_guard.refuse_drop() IS '{stamp}';")));
        // The stamp comes before COMMIT, in the same transaction.
        assert!(sql.find(&stamp).unwrap() < sql.rfind("COMMIT;").unwrap());
        let check = schema_guard_check_sql();
        assert!(check.starts_with(GUARD_CHECK_MARK), "{check}");
        assert!(check.contains(&format!("OPERATOR(pg_catalog.=) '{stamp}'")), "{check}");
        // Read-only: one SELECT, nothing else (outside the bodies it compares as text).
        let outside = check.replace(REFUSE_DDL_BODY, "").replace(REFUSE_DROP_BODY, "");
        for word in ["INSERT", "UPDATE", "DELETE", "ALTER", "DROP", "COMMENT", "GRANT", "REVOKE"] {
            assert!(!outside.contains(word), "{word}: {outside}");
        }
    }

    // T7 fix round, live finding 4: a REFRESH was refused by an older `refuse_ddl` body (no
    // maintenance check) that had replaced ours under our comment (`CREATE OR REPLACE` keeps
    // it), and the stamp-only check still answered `t`. The check now compares the bodies,
    // and each installed body lets maintenance through before any refusal.
    #[test]
    fn the_check_compares_the_bodies_and_they_allow_maintenance_first() {
        let sql = schema_guard_sql();
        let check = schema_guard_check_sql();
        for body in [REFUSE_DDL_BODY, REFUSE_DROP_BODY] {
            assert!(sql.contains(&format!("$f${body}$f$;")), "installed as is: {body}");
            assert!(check.contains(&format!("$b${body}$b$")), "compared as is: {body}");
            assert!(!body.contains("$b$") && !body.contains("$f$"));
            let maintenance = body
                .find("IF TG_TAG OPERATOR(pg_catalog.=) 'REFRESH MATERIALIZED VIEW' OR TG_TAG OPERATOR(pg_catalog.=) 'REINDEX' THEN RETURN; END IF;")
                .expect("maintenance allowed");
            assert!(maintenance < body.find("RAISE EXCEPTION").unwrap(), "{body}");
            assert!(maintenance > body.find("pg_has_role").unwrap(), "{body}");
        }
        assert!(check.contains("AND NOT p.prosecdef"));
        assert!(check.contains("p.proconfig OPERATOR(pg_catalog.=) ARRAY['search_path=pg_catalog, pg_temp']"));
    }

    #[test]
    fn planted_objects_go_before_the_functions_are_made() {
        let sql = schema_guard_sql();
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
