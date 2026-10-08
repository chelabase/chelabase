//! The `.sql` export of a plan (T4).

use crate::plan::{PHASE1_STATEMENT_TIMEOUT, Phase, Plan, PlanOptions, Step};

/// The plan as a `.sql` file: phase headers, phase 1 in `BEGIN`/`COMMIT` with the merge's
/// `lock_timeout` and `statement_timeout` (E35, from `opts`), phase 2 statements outside a
/// transaction, roles listed only (commented out: they're created through the roles API, with a
/// password shown once); no passwords. Empty phases are left out. The stale guard and the
/// recovery marker are the merge job's, not part of the export (a comment says so). Every
/// commented line is split at `\n` and `\r` alike (Postgres ends a `--` comment at either).
pub fn export_sql(plan: &Plan, opts: &PlanOptions) -> String {
    let mut s = String::from("-- Chelabase merge plan\n");
    let of = |p: Phase| plan.steps.iter().filter(move |st| st.phase == p);
    let header = |s: &mut String, h: &str| {
        s.push('\n');
        s.push_str(h);
        s.push('\n');
    };
    if of(Phase::Roles).next().is_some() {
        header(&mut s, "-- Phase 0: roles (create through the roles API; listed only)");
        for st in of(Phase::Roles) {
            step_line(&mut s, st);
            for line in st.sql.split(['\n', '\r']).filter(|l| !l.is_empty()) {
                s.push_str("-- ");
                s.push_str(line);
                s.push('\n');
            }
        }
    }
    if of(Phase::EnumAdd).next().is_some() {
        header(
            &mut s,
            "-- Phase 1a: enum values used by the plan (each statement commits on its own)",
        );
        for st in of(Phase::EnumAdd) {
            statement(&mut s, st);
        }
    }
    if of(Phase::Tx).next().is_some() {
        header(&mut s, "-- Phase 1: one transaction");
        s.push_str("BEGIN;\n");
        s.push_str("-- The merge's stale guard and recovery marker are left out.\n");
        s.push_str(&format!(
            "SET LOCAL lock_timeout = '{}ms';\nSET LOCAL statement_timeout = '{PHASE1_STATEMENT_TIMEOUT}';\n",
            opts.lock_timeout_ms
        ));
        for st in of(Phase::Tx) {
            statement(&mut s, st);
        }
        s.push_str("COMMIT;\n");
    }
    if of(Phase::Online).next().is_some() {
        header(&mut s, "-- Phase 2: run each statement outside a transaction");
        for st in of(Phase::Online) {
            statement(&mut s, st);
        }
    }
    s
}

fn step_line(s: &mut String, st: &Step) {
    let mut flags = Vec::new();
    if st.data_loss {
        flags.push("data loss");
    }
    if st.rewrite {
        flags.push("rewrites the table");
    }
    if st.security {
        flags.push("security");
    }
    s.push_str("-- step ");
    s.push_str(&st.id.0.replace(['\n', '\r'], " "));
    if !flags.is_empty() {
        s.push_str(&format!(" ({})", flags.join(", ")));
    }
    s.push('\n');
}

fn statement(s: &mut String, st: &Step) {
    step_line(s, st);
    let sql = st.sql.trim_end();
    s.push_str(sql);
    if sql.lines().last().is_some_and(|l| l.contains("--")) {
        s.push('\n');
    }
    s.push_str(";\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::plan;
    use crate::testkit::*;

    #[test]
    fn export_has_phase_headers_and_no_passwords() {
        let mut target = model(vec![table(100, "public", "t", vec![col(1, "m", "mood")])]);
        target.enums = vec![enum_type(500, "public", "mood", &[(501, "sad")])];
        let mut m_col = col(1, "m", "mood");
        m_col.default = Some("'happy'::mood".into());
        let mut t = table(100, "public", "t", vec![m_col]);
        t.indexes = vec![index(0, "t_m_idx", &["m"])];
        let mut m = model(vec![t]);
        m.enums = vec![enum_type(500, "public", "mood", &[(501, "sad"), (502, "happy")])];
        let mut app = role(700, "app");
        app.login = true;
        m.roles = vec![app];
        let p = plan(&target, &merged(m), &no_analysis(), &[], &opts()).unwrap();

        let sql = export_sql(&p, &opts());
        assert_eq!(
            sql,
            "-- Chelabase merge plan\n\
             \n\
             -- Phase 0: roles (create through the roles API; listed only)\n\
             -- step role:create:app\n\
             -- CREATE ROLE app WITH LOGIN INHERIT NOBYPASSRLS CONNECTION LIMIT -1\n\
             \n\
             -- Phase 1a: enum values used by the plan (each statement commits on its own)\n\
             -- step add-enum-value:public.mood.happy\n\
             ALTER TYPE public.mood ADD VALUE IF NOT EXISTS 'happy' AFTER 'sad';\n\
             \n\
             -- Phase 1: one transaction\n\
             BEGIN;\n\
             -- The merge's stale guard and recovery marker are left out.\n\
             SET LOCAL lock_timeout = '5000ms';\n\
             SET LOCAL statement_timeout = '15min';\n\
             -- step set-default:public.t.m\n\
             ALTER TABLE public.t ALTER COLUMN m SET DEFAULT 'happy'::mood;\n\
             COMMIT;\n\
             \n\
             -- Phase 2: run each statement outside a transaction\n\
             -- step create-index:public.t.t_m_idx\n\
             CREATE INDEX CONCURRENTLY t_m_idx ON public.t USING btree (m);\n"
        );
        assert!(!sql.to_uppercase().contains("PASSWORD"));
    }

    #[test]
    fn export_marks_data_loss_and_comments_every_role_line() {
        let mut target = model(vec![table(100, "public", "t", vec![col(1, "a", "integer")])]);
        target.roles = vec![role(400, "r")];
        let mut m = model(vec![]);
        let mut r = role(400, "r");
        r.member_of = vec![membership(401, "readers")];
        m.roles = vec![r];
        let p = plan(&target, &merged(m), &no_analysis(), &[], &opts()).unwrap();
        let sql = export_sql(&p, &opts());
        assert!(
            sql.contains("-- step role:update:r\n-- GRANT readers TO r\n"),
            "{sql}"
        );
        assert!(sql.contains("-- step drop-table:public.t (data loss)\nDROP TABLE public.t;\n"), "{sql}");
        for line in sql.lines() {
            if line.contains("ROLE") || line.contains("GRANT readers") {
                assert!(line.starts_with("-- "), "{line}");
            }
        }
    }

    // E35: the phase 1 block waits for locks at most the merge's lock_timeout.
    #[test]
    fn export_phase1_uses_the_merges_timeouts() {
        let m = model(vec![table(300, "public", "t", vec![col(1, "a", "integer")])]);
        let p = plan(&model(vec![]), &merged(m), &no_analysis(), &[], &opts()).unwrap();
        let o = crate::plan::PlanOptions {
            lock_timeout_ms: 1234,
            ..opts()
        };
        let sql = export_sql(&p, &o);
        assert!(
            sql.contains(
                "BEGIN;\n-- The merge's stale guard and recovery marker are left out.\nSET LOCAL lock_timeout = '1234ms';\nSET LOCAL statement_timeout = '15min';\n"
            ),
            "{sql}"
        );
    }

    // T14 minor 19: a role name holding a bare carriage return can't end the comment early (a
    // `\r` ends a `--` comment for Postgres).
    #[test]
    fn export_role_lines_stay_comments_across_carriage_returns() {
        let mut m = model(vec![]);
        m.roles = vec![role(0, "x\rDROP TABLE victim;--")];
        let p = plan(&model(vec![]), &merged(m), &no_analysis(), &[], &opts()).unwrap();
        let sql = export_sql(&p, &opts());
        for line in sql.split(['\n', '\r']) {
            if line.contains("DROP TABLE victim") {
                assert!(line.starts_with("-- "), "{line:?} in {sql:?}");
            }
        }
        assert!(
            pg_query::parse(&sql).unwrap().protobuf.stmts.is_empty(),
            "{sql:?}"
        );
    }
}
