use super::*;
use crate::analysis::{Change, ObjKind, ObjSnapshot};
use crate::model::*;
use crate::steps::UserStep;
use crate::testkit::*;

fn plan_of(target: &SchemaModel, m: &SchemaModel) -> Plan {
    plan(target, &merged(m.clone()), &no_analysis(), &[], &opts()).unwrap()
}

fn user(id: &str, after: Option<&str>, sql: &str) -> UserStep {
    UserStep {
        id: StepId(id.to_string()),
        after: after.map(|a| StepId(a.to_string())),
        sql: sql.to_string(),
    }
}

#[test]
fn role_patch_valid_until_keeps_absent_null_and_set_apart() {
    let absent: RolePatchModel = serde_json::from_str("{}").unwrap();
    assert_eq!(absent.valid_until, None);
    let cleared: RolePatchModel = serde_json::from_str(r#"{"valid_until":null}"#).unwrap();
    assert_eq!(cleared.valid_until, Some(None));
    let set: RolePatchModel = serde_json::from_str(r#"{"valid_until":"2030-01-01"}"#).unwrap();
    assert_eq!(set.valid_until, Some(Some("2030-01-01".to_string())));
    for p in [absent, cleared, set] {
        let back: RolePatchModel =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}

// ---- order and quoting ----

#[test]
fn drops_before_creates_in_reverse_dependency_order() {
    // `b_child` references `a_parent`: alphabetical order would drop the parent first.
    let mut parent = table(
        100,
        "public",
        "a_parent",
        vec![not_null(col(1, "id", "integer"))],
    );
    parent.constraints = vec![constraint(
        110,
        "a_parent_pkey",
        ConstraintKind::PrimaryKey,
        "PRIMARY KEY (id)",
        &[1],
    )];
    let mut child = table(
        101,
        "public",
        "b_child",
        vec![col(1, "id", "integer"), col(2, "parent_id", "integer")],
    );
    child.constraints = vec![fk(
        111,
        "b_child_parent_id_fkey",
        "FOREIGN KEY (parent_id) REFERENCES public.a_parent(id)",
        &[2],
        ("public", "a_parent", &[1]),
    )];
    let mut keep = table(
        102,
        "public",
        "keep",
        vec![col(1, "a", "integer"), col(2, "b", "integer")],
    );
    keep.policies = vec![policy(112, "p", "(b = 1)")];
    let target = model(vec![parent, child, keep]);
    let merged_model = model(vec![
        table(102, "public", "keep", vec![col(1, "a", "integer")]),
        table(200, "public", "fresh", vec![col(1, "x", "integer")]),
    ]);

    let p = plan_of(&target, &merged_model);
    let drop_policy = at(&p, "DROP POLICY p ON public.keep");
    let drop_column = at(&p, "ALTER TABLE public.keep DROP COLUMN b");
    let drop_child = at(&p, "DROP TABLE public.b_child");
    let drop_parent = at(&p, "DROP TABLE public.a_parent");
    let create = at(&p, "CREATE TABLE public.fresh (x integer)");
    assert!(drop_policy < drop_column, "{:#?}", sqls(&p));
    assert!(drop_child < drop_parent, "{:#?}", sqls(&p));
    assert!(
        [drop_policy, drop_column, drop_child, drop_parent]
            .iter()
            .all(|d| *d < create),
        "{:#?}",
        sqls(&p)
    );
    assert_eq!(p.steps.len(), 5, "{:#?}", sqls(&p));
    for sql in [
        "ALTER TABLE public.keep DROP COLUMN b",
        "DROP TABLE public.b_child",
        "DROP TABLE public.a_parent",
    ] {
        assert!(step(&p, sql).data_loss, "{sql}");
        assert_eq!(phase_of(&p, sql), Phase::Tx);
    }
    assert!(!step(&p, "DROP POLICY p ON public.keep").data_loss);
}

#[test]
fn renames_come_first() {
    let old = table(100, "public", "old", vec![col(1, "a", "integer")]);
    let target = model(vec![
        old.clone(),
        table(101, "public", "gone", vec![col(1, "x", "integer")]),
    ]);
    let new = table(100, "public", "new", vec![col(1, "b", "integer")]);
    let m = model(vec![
        new.clone(),
        table(200, "public", "added", vec![col(1, "y", "integer")]),
    ]);
    let mut analysis = no_analysis();
    analysis.items = vec![
        renamed(
            "table:100",
            key(ObjKind::Table, Some("public"), None, "old"),
            ObjSnapshot::table(&old),
            ObjSnapshot::table(&new),
        ),
        renamed(
            "column:100:1",
            key(ObjKind::Column, Some("public"), Some("old"), "a"),
            ObjSnapshot::Column {
                table_oid: Oid(100),
                schema: "public".into(),
                table: "old".into(),
                column: col(1, "a", "integer"),
            },
            ObjSnapshot::Column {
                table_oid: Oid(100),
                schema: "public".into(),
                table: "new".into(),
                column: col(1, "b", "integer"),
            },
        ),
    ];

    let p = plan(&target, &merged(m), &analysis, &[], &opts()).unwrap();
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.old RENAME TO new",
            "ALTER TABLE public.new RENAME COLUMN a TO b",
            "DROP TABLE public.gone",
            "CREATE TABLE public.added (y integer)",
        ]
    );
    assert_eq!(p.steps[0].items, [ItemId("table:100".into())]);
    assert_eq!(p.steps[1].items, [ItemId("column:100:1".into())]);
    assert!(!p.steps[0].data_loss && !p.steps[1].data_loss);
}

#[test]
fn fk_after_referenced_unique() {
    // `b` sorts before `z`: only the bucket order puts z's unique before b's foreign key.
    let z_cols = vec![not_null(col(1, "id", "integer"))];
    let unique = constraint(310, "z_id_key", ConstraintKind::Unique, "UNIQUE (id)", &[1]);
    let foreign = fk(
        311,
        "b_z_id_fkey",
        "FOREIGN KEY (z_id) REFERENCES public.z(id)",
        &[1],
        ("public", "z", &[1]),
    );
    let mut z = table(300, "public", "z", z_cols.clone());
    z.constraints = vec![unique.clone()];
    let mut b = table(301, "public", "b", vec![col(1, "z_id", "integer")]);
    b.constraints = vec![foreign.clone()];
    let m = model(vec![b.clone(), z.clone()]);

    // New tables: both in phase 1, the unique first.
    let p = plan_of(&model(vec![]), &m);
    let u = at(
        &p,
        "ALTER TABLE public.z ADD CONSTRAINT z_id_key UNIQUE (id)",
    );
    let f = at(
        &p,
        "ALTER TABLE public.b ADD CONSTRAINT b_z_id_fkey FOREIGN KEY (z_id) REFERENCES public.z(id)",
    );
    let create_b = at(&p, "CREATE TABLE public.b (z_id integer)");
    let create_z = at(&p, "CREATE TABLE public.z (id integer NOT NULL)");
    assert!(create_b < u && create_z < u && u < f, "{:#?}", sqls(&p));
    assert!(p.steps.iter().all(|s| s.phase == Phase::Tx));

    // Existing tables: the unique is built online, so the foreign key waits for it there.
    let target = model(vec![
        table(301, "public", "b", vec![col(1, "z_id", "integer")]),
        table(300, "public", "z", z_cols),
    ]);
    let p = plan_of(&target, &m);
    let index = "CREATE UNIQUE INDEX CONCURRENTLY z_id_key ON public.z USING btree (id)";
    let using = "ALTER TABLE public.z ADD CONSTRAINT z_id_key UNIQUE USING INDEX z_id_key";
    let add = "ALTER TABLE public.b ADD CONSTRAINT b_z_id_fkey FOREIGN KEY (z_id) REFERENCES public.z(id) NOT VALID";
    let validate = "ALTER TABLE public.b VALIDATE CONSTRAINT b_z_id_fkey";
    assert_eq!(sqls(&p), [index, using, add, validate]);
    assert!(p.steps.iter().all(|s| s.phase == Phase::Online));
    assert_eq!(
        step(&p, add).online,
        Some(OnlineCheck::ConstraintExists {
            schema: "public".into(),
            table: "b".into(),
            name: "b_z_id_fkey".into()
        })
    );
}

#[test]
fn quotes_every_identifier() {
    let app = rref(400, "App Role");
    let mut m = model(vec![]);
    m.schemas.push(schema(300, "Billing"));
    m.roles = vec![role(400, "App Role")];
    m.enums = vec![enum_type(310, "Billing", "Status", &[(311, "it's")])];
    m.sequences = vec![sequence(320, "Billing", "Seq")];
    let mut ab = col(2, "a\"b", "text");
    ab.grants = vec![grant(app.clone(), "UPDATE")];
    let mut t = table(
        330,
        "Billing",
        "my table",
        vec![not_null(col(1, "Order", "integer")), ab],
    );
    t.constraints = vec![constraint(
        331,
        "select",
        ConstraintKind::Check,
        "CHECK ((\"Order\" > 0))",
        &[1],
    )];
    t.indexes = vec![index(332, "Idx", &["\"a\"\"b\""])];
    let mut own = policy(333, "own rows", "true");
    own.roles = vec![app.clone()];
    t.policies = vec![own];
    t.rls_enabled = true;
    t.grants = vec![grant(app.clone(), "SELECT")];
    m.tables = vec![t];

    let p = plan_of(&model(vec![]), &m);
    assert_eq!(
        sqls(&p),
        [
            "CREATE ROLE \"App Role\" WITH NOLOGIN INHERIT NOBYPASSRLS CONNECTION LIMIT -1",
            "CREATE SCHEMA \"Billing\"",
            "CREATE TYPE \"Billing\".\"Status\" AS ENUM ('it''s')",
            "CREATE SEQUENCE \"Billing\".\"Seq\" AS bigint INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 1 CACHE 1 NO CYCLE",
            "CREATE TABLE \"Billing\".\"my table\" (\"Order\" integer NOT NULL, \"a\"\"b\" text)",
            "ALTER TABLE \"Billing\".\"my table\" ADD CONSTRAINT \"select\" CHECK ((\"Order\" > 0))",
            "CREATE INDEX \"Idx\" ON \"Billing\".\"my table\" USING btree (\"a\"\"b\")",
            "ALTER TABLE \"Billing\".\"my table\" ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY \"own rows\" ON \"Billing\".\"my table\" AS PERMISSIVE FOR ALL TO \"App Role\" USING (true)",
            "GRANT SELECT ON TABLE \"Billing\".\"my table\" TO \"App Role\"",
            "GRANT UPDATE (\"a\"\"b\") ON TABLE \"Billing\".\"my table\" TO \"App Role\"",
        ]
    );

    // Renames quote both names.
    let before = table(500, "public", "Order", vec![col(1, "select", "integer")]);
    let after = table(500, "public", "my table", vec![col(1, "a\"b", "integer")]);
    let mut analysis = no_analysis();
    analysis.items = vec![
        renamed(
            "t",
            key(ObjKind::Table, Some("public"), None, "Order"),
            ObjSnapshot::table(&before),
            ObjSnapshot::table(&after),
        ),
        renamed(
            "c",
            key(ObjKind::Column, Some("public"), Some("Order"), "select"),
            ObjSnapshot::Column {
                table_oid: Oid(500),
                schema: "public".into(),
                table: "Order".into(),
                column: col(1, "select", "integer"),
            },
            ObjSnapshot::Column {
                table_oid: Oid(500),
                schema: "public".into(),
                table: "my table".into(),
                column: col(1, "a\"b", "integer"),
            },
        ),
    ];
    let p = plan(
        &model(vec![before]),
        &merged(model(vec![after])),
        &analysis,
        &[],
        &opts(),
    )
    .unwrap();
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.\"Order\" RENAME TO \"my table\"",
            "ALTER TABLE public.\"my table\" RENAME COLUMN \"select\" TO \"a\"\"b\"",
        ]
    );
}

// ---- phase transforms on tables that exist on the target ----

#[test]
fn new_index_on_existing_table_is_concurrent_phase2() {
    let target = model(vec![table(
        100,
        "public",
        "t",
        vec![col(1, "a", "integer")],
    )]);
    let mut t = table(100, "public", "t", vec![col(1, "a", "integer")]);
    t.indexes = vec![index(0, "t_a_idx", &["a"])];
    let p = plan_of(&target, &model(vec![t]));
    let sql = "CREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a)";
    assert_eq!(sqls(&p), [sql]);
    let s = step(&p, sql);
    assert_eq!(s.phase, Phase::Online);
    assert_eq!(
        s.online,
        Some(OnlineCheck::IndexValid {
            schema: "public".into(),
            table: "t".into(),
            name: "t_a_idx".into()
        })
    );
    // E15: the session is the owner's own; no role switch.
    assert_eq!(
        step_script(s, &opts()),
        "SET statement_timeout = 0;\nCREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a);\n"
    );
}

#[test]
fn new_table_gets_everything_in_phase1() {
    let mut z = table(300, "public", "z", vec![not_null(col(1, "id", "integer"))]);
    z.constraints = vec![constraint(
        310,
        "z_id_key",
        ConstraintKind::Unique,
        "UNIQUE (id)",
        &[1],
    )];
    let mut t = table(
        301,
        "public",
        "t",
        vec![not_null(col(1, "a", "integer")), col(2, "z_id", "integer")],
    );
    t.constraints = vec![
        constraint(
            311,
            "t_a_check",
            ConstraintKind::Check,
            "CHECK ((a > 0))",
            &[1],
        ),
        fk(
            312,
            "t_z_id_fkey",
            "FOREIGN KEY (z_id) REFERENCES public.z(id)",
            &[2],
            ("public", "z", &[1]),
        ),
    ];
    t.indexes = vec![index(313, "t_a_idx", &["a"])];
    let p = plan_of(&model(vec![]), &model(vec![t, z]));
    assert_eq!(
        sqls(&p),
        [
            "CREATE TABLE public.t (a integer NOT NULL, z_id integer)",
            "CREATE TABLE public.z (id integer NOT NULL)",
            "ALTER TABLE public.t ADD CONSTRAINT t_a_check CHECK ((a > 0))",
            "ALTER TABLE public.z ADD CONSTRAINT z_id_key UNIQUE (id)",
            "ALTER TABLE public.t ADD CONSTRAINT t_z_id_fkey FOREIGN KEY (z_id) REFERENCES public.z(id)",
            "CREATE INDEX t_a_idx ON public.t USING btree (a)",
        ]
    );
    assert!(
        p.steps
            .iter()
            .all(|s| s.phase == Phase::Tx && s.online.is_none())
    );
}

#[test]
fn new_fk_is_not_valid_then_validated() {
    let mut a = table(100, "public", "a", vec![not_null(col(1, "id", "integer"))]);
    a.constraints = vec![constraint(
        110,
        "a_pkey",
        ConstraintKind::PrimaryKey,
        "PRIMARY KEY (id)",
        &[1],
    )];
    let b = table(101, "public", "b", vec![col(1, "a_id", "integer")]);
    let target = model(vec![a.clone(), b.clone()]);
    let mut b2 = b.clone();
    let mut unvalidated = constraint(
        0,
        "b_small_check",
        ConstraintKind::Check,
        "CHECK ((a_id < 100)) NOT VALID",
        &[1],
    );
    unvalidated.validated = false;
    b2.constraints = vec![
        fk(
            0,
            "b_a_id_fkey",
            "FOREIGN KEY (a_id) REFERENCES public.a(id)",
            &[1],
            ("public", "a", &[1]),
        ),
        constraint(
            0,
            "b_a_id_check",
            ConstraintKind::Check,
            "CHECK ((a_id > 0))",
            &[1],
        ),
        unvalidated,
    ];
    let p = plan_of(&target, &model(vec![a, b2]));
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.b ADD CONSTRAINT b_a_id_check CHECK ((a_id > 0)) NOT VALID",
            "ALTER TABLE public.b ADD CONSTRAINT b_small_check CHECK ((a_id < 100)) NOT VALID",
            "ALTER TABLE public.b ADD CONSTRAINT b_a_id_fkey FOREIGN KEY (a_id) REFERENCES public.a(id) NOT VALID",
            "ALTER TABLE public.b VALIDATE CONSTRAINT b_a_id_check",
            "ALTER TABLE public.b VALIDATE CONSTRAINT b_a_id_fkey",
        ]
    );
    let phases: Vec<Phase> = p.steps.iter().map(|s| s.phase).collect();
    assert_eq!(
        phases,
        [
            Phase::Tx,
            Phase::Tx,
            Phase::Tx,
            Phase::Online,
            Phase::Online
        ]
    );
    assert_eq!(
        p.steps[4].online,
        Some(OnlineCheck::ConstraintValidated {
            schema: "public".into(),
            table: "b".into(),
            name: "b_a_id_fkey".into()
        })
    );
}

#[test]
fn new_unique_uses_concurrent_index_then_using_index() {
    let cols = vec![col(1, "a", "integer"), col(2, "b", "integer")];
    let target = model(vec![table(100, "public", "t", cols.clone())]);
    let mut t = table(100, "public", "t", cols);
    t.constraints = vec![
        constraint(0, "t_a_key", ConstraintKind::Unique, "UNIQUE (a)", &[1]),
        constraint(
            0,
            "t_ab_key",
            ConstraintKind::Unique,
            "UNIQUE NULLS NOT DISTINCT (a) INCLUDE (b)",
            &[1],
        ),
    ];
    let p = plan_of(&target, &model(vec![t]));
    assert_eq!(
        sqls(&p),
        [
            "CREATE UNIQUE INDEX CONCURRENTLY t_a_key ON public.t USING btree (a)",
            "ALTER TABLE public.t ADD CONSTRAINT t_a_key UNIQUE USING INDEX t_a_key",
            "CREATE UNIQUE INDEX CONCURRENTLY t_ab_key ON public.t USING btree (a) INCLUDE (b) NULLS NOT DISTINCT",
            "ALTER TABLE public.t ADD CONSTRAINT t_ab_key UNIQUE USING INDEX t_ab_key",
        ]
    );
    assert!(p.steps.iter().all(|s| s.phase == Phase::Online));
    assert_eq!(
        p.steps[0].online,
        Some(OnlineCheck::IndexValid {
            schema: "public".into(),
            table: "t".into(),
            name: "t_a_key".into()
        })
    );
    assert_eq!(
        p.steps[1].online,
        Some(OnlineCheck::ConstraintValidated {
            schema: "public".into(),
            table: "t".into(),
            name: "t_a_key".into()
        })
    );
}

#[test]
fn new_not_null_uses_named_check_then_set_not_null_then_drops_check() {
    let target = model(vec![table(
        16500,
        "public",
        "t",
        vec![col(1, "id", "integer"), col(2, "a", "integer")],
    )]);
    let m = model(vec![table(
        16500,
        "public",
        "t",
        vec![col(1, "id", "integer"), not_null(col(2, "a", "integer"))],
    )]);
    let p = plan_of(&target, &m);
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.t ADD CONSTRAINT chelabase_merge_nn_2_0123456789abcdef0123456789abcdef CHECK (a IS NOT NULL) NOT VALID",
            "ALTER TABLE public.t VALIDATE CONSTRAINT chelabase_merge_nn_2_0123456789abcdef0123456789abcdef",
            "ALTER TABLE public.t ALTER COLUMN a SET NOT NULL",
            "ALTER TABLE public.t DROP CONSTRAINT IF EXISTS chelabase_merge_nn_2_0123456789abcdef0123456789abcdef",
        ]
    );
    let phases: Vec<Phase> = p.steps.iter().map(|s| s.phase).collect();
    assert_eq!(
        phases,
        [Phase::Tx, Phase::Online, Phase::Online, Phase::Online]
    );
    let checks: Vec<Option<OnlineCheck>> = p.steps.iter().map(|s| s.online.clone()).collect();
    assert_eq!(
        checks,
        [
            None,
            Some(OnlineCheck::ConstraintValidated {
                schema: "public".into(),
                table: "t".into(),
                name: "chelabase_merge_nn_2_0123456789abcdef0123456789abcdef".into()
            }),
            Some(OnlineCheck::NotNullSet {
                schema: "public".into(),
                table: "t".into(),
                column: "a".into()
            }),
            None,
        ]
    );
}

#[test]
fn changed_index_is_replaced_concurrently() {
    let cols = vec![col(1, "a", "integer"), col(2, "b", "integer")];
    let mut t = table(100, "public", "t", cols.clone());
    t.indexes = vec![index(110, "t_a_idx", &["a"])];
    let mut m = table(100, "public", "t", cols);
    m.indexes = vec![index(110, "t_a_idx", &["a", "b"])];
    let p = plan_of(&model(vec![t]), &model(vec![m]));
    // The temporary name carries the merge's id, so a leftover of another merge (same prefix,
    // same counter) is never taken for this merge's index and renamed into place.
    let tmp = "chelabase_merge_ix_1_0123456789abcdef0123456789abcdef";
    assert!(tmp.len() <= 63);
    assert!(tmp.starts_with(crate::exclusions::MERGE_INDEX_PREFIX));
    assert_eq!(
        sqls(&p),
        [
            format!("CREATE INDEX CONCURRENTLY {tmp} ON public.t USING btree (a, b)"),
            "DROP INDEX CONCURRENTLY IF EXISTS public.t_a_idx".to_string(),
            format!("ALTER INDEX IF EXISTS public.{tmp} RENAME TO t_a_idx"),
        ]
    );
    assert!(p.steps.iter().all(|s| s.phase == Phase::Online));
    // The resume check also pins the table (`indrelid`).
    assert_eq!(
        p.steps[0].online,
        Some(OnlineCheck::IndexValid {
            schema: "public".into(),
            table: "t".into(),
            name: tmp.into()
        })
    );
    // Another merge gets another name.
    let other = PlanOptions {
        merge_id: Uuid::from_u128(0xffff_0000_ffff_0000_ffff_0000_ffff_0000),
        ..opts()
    };
    let cols = vec![col(1, "a", "integer"), col(2, "b", "integer")];
    let mut t = table(100, "public", "t", cols.clone());
    t.indexes = vec![index(110, "t_a_idx", &["a"])];
    let mut m = table(100, "public", "t", cols);
    m.indexes = vec![index(110, "t_a_idx", &["a", "b"])];
    let q = plan(&model(vec![t]), &merged(model(vec![m])), &no_analysis(), &[], &other).unwrap();
    assert_eq!(
        q.steps[0].sql,
        "CREATE INDEX CONCURRENTLY chelabase_merge_ix_1_ffff0000ffff0000ffff0000ffff0000 ON public.t USING btree (a, b)"
    );
}

#[test]
fn enum_value_used_in_plan_goes_to_phase1a() {
    let mut target = model(vec![table(100, "public", "t", vec![col(1, "m", "mood")])]);
    target.enums = vec![
        enum_type(500, "public", "level", &[(501, "low")]),
        enum_type(510, "public", "mood", &[(511, "sad")]),
    ];
    let mut m_col = col(1, "m", "mood");
    m_col.default = Some("'happy'::mood".into());
    let mut m = model(vec![table(100, "public", "t", vec![m_col])]);
    m.enums = vec![
        enum_type(500, "public", "level", &[(501, "low"), (600, "high")]),
        enum_type(510, "public", "mood", &[(511, "sad"), (610, "happy")]),
    ];
    let p = plan_of(&target, &m);
    assert_eq!(
        sqls(&p),
        [
            "ALTER TYPE public.mood ADD VALUE IF NOT EXISTS 'happy' AFTER 'sad'",
            "ALTER TYPE public.level ADD VALUE IF NOT EXISTS 'high' AFTER 'low'",
            "ALTER TABLE public.t ALTER COLUMN m SET DEFAULT 'happy'::mood",
        ]
    );
    let phases: Vec<Phase> = p.steps.iter().map(|s| s.phase).collect();
    assert_eq!(phases, [Phase::EnumAdd, Phase::Tx, Phase::Tx]);
}

#[test]
fn enum_value_removal_is_unplannable() {
    let mut target = model(vec![]);
    target.enums = vec![enum_type(500, "public", "e", &[(501, "a"), (502, "b")])];
    let mut m = model(vec![]);
    m.enums = vec![enum_type(500, "public", "e", &[(501, "a")])];
    let err = plan(&target, &merged(m), &no_analysis(), &[], &opts()).unwrap_err();
    assert!(
        matches!(&err, SchemaError::Unplannable { object, .. } if object == "public.e"),
        "{err:?}"
    );
}

// ---- roles ----

#[test]
fn roles_are_phase0_role_ops() {
    let mut target = model(vec![]);
    target.roles = vec![role(401, "readers"), role(402, "old")];
    let mut m = model(vec![]);
    let mut app = role(700, "app");
    app.login = true;
    app.bypass_rls = true;
    app.connection_limit = 3;
    app.valid_until = Some("2030-01-01 00:00:00+00".into());
    app.member_of = vec![
        membership(16399, "neon_superuser"),
        membership(401, "readers"),
    ];
    let mut readers = role(401, "readers");
    readers.inherit = false;
    m.roles = vec![app, readers];
    // The role dropped from the merged model is reported, never dropped.
    let p = plan_of(&target, &m);
    assert_eq!(p.steps.len(), 2, "{:#?}", sqls(&p));
    assert!(p.steps.iter().all(|s| s.phase == Phase::Roles));
    assert_eq!(
        p.steps[0].role_op,
        Some(RoleOp::Create(RoleSpecModel {
            name: "app".into(),
            owner_level: true,
            login: true,
            inherit: true,
            connection_limit: 3,
            valid_until: Some("2030-01-01T00:00:00+00:00".into()),
            bypass_rls: true,
            member_of: vec!["readers".into()],
        }))
    );
    assert!(p.steps[0].security);
    assert_eq!(
        p.steps[0].sql,
        "CREATE ROLE app WITH LOGIN INHERIT BYPASSRLS CONNECTION LIMIT 3 VALID UNTIL '2030-01-01T00:00:00+00:00';\nGRANT neon_superuser TO app;\nGRANT readers TO app"
    );
    assert_eq!(
        p.steps[1].role_op,
        Some(RoleOp::Update {
            name: "readers".into(),
            patch: RolePatchModel {
                inherit: Some(false),
                ..Default::default()
            }
        })
    );
    assert!(!p.steps[1].security);
    assert_eq!(p.steps[1].sql, "ALTER ROLE readers WITH NOINHERIT");
    assert!(p.steps.iter().all(|s| !s.sql.contains("PASSWORD")));
}

// E22: the owner's memberships are never planned, even when the merged model's owner differs
// from the target's in them.
#[test]
fn owner_memberships_are_never_planned() {
    let mut target = model(vec![]);
    let mut owner = role(10, "postgres");
    owner.member_of = vec![membership(401, "readers")];
    target.roles = vec![owner.clone(), role(401, "readers")];
    let mut m = model(vec![]);
    let mut merged_owner = owner;
    merged_owner.member_of = crate::model::owner_grants(&named(700, "app"), &postgres());
    m.roles = vec![merged_owner, role(401, "readers"), role(700, "app")];
    let p = plan_of(&target, &m);
    assert_eq!(sqls(&p), ["CREATE ROLE app WITH NOLOGIN INHERIT NOBYPASSRLS CONNECTION LIMIT -1"]);
}

// ---- user steps ----

#[test]
fn user_step_is_placed_after_its_anchor() {
    let m = model(vec![
        table(300, "public", "t1", vec![col(1, "a", "integer")]),
        table(301, "public", "t2", vec![col(1, "b", "integer")]),
    ]);
    let base = plan_of(&model(vec![]), &m);
    let anchor = step(&base, "CREATE TABLE public.t1 (a integer)").id.clone();
    let steps = [
        user("u1", Some(&anchor.0), "UPDATE public.t1 SET a = 1"),
        user("u0", None, "SELECT 1"),
        user("u2", Some("u1"), "UPDATE public.t1 SET a = 2"),
    ];
    let p = plan(&model(vec![]), &merged(m), &no_analysis(), &steps, &opts()).unwrap();
    assert_eq!(
        sqls(&p),
        [
            "SELECT 1",
            "CREATE TABLE public.t1 (a integer)",
            "UPDATE public.t1 SET a = 1",
            "UPDATE public.t1 SET a = 2",
            "CREATE TABLE public.t2 (b integer)",
        ]
    );
    let ids: Vec<&str> = p.steps.iter().map(|s| s.id.0.as_str()).collect();
    assert_eq!(ids[0], "u0");
    assert_eq!(ids[2], "u1");
    assert_eq!(ids[3], "u2");
    assert_eq!(p.steps[2].kind, StepKind::User);
    assert_eq!(p.steps[1].kind, StepKind::Generated);

    let missing = [user("u9", Some("nope"), "SELECT 1")];
    let err = plan(
        &model(vec![]),
        &merged(model(vec![])),
        &no_analysis(),
        &missing,
        &opts(),
    )
    .unwrap_err();
    assert!(
        matches!(err, SchemaError::Steps(ref e) if e.len() == 1),
        "{err:?}"
    );
}

#[test]
fn concurrent_user_step_moves_to_phase2() {
    let target = model(vec![table(
        100,
        "public",
        "t",
        vec![col(1, "a", "integer")],
    )]);
    let mut t = table(
        100,
        "public",
        "t",
        vec![col(1, "a", "integer"), col(2, "b", "integer")],
    );
    t.indexes = vec![index(0, "t_b_idx", &["b"])];
    let m = model(vec![t]);
    let base = plan_of(&target, &m);
    let add = step(&base, "ALTER TABLE public.t ADD COLUMN b integer")
        .id
        .clone();
    let steps = [user(
        "u1",
        Some(&add.0),
        "UPDATE public.t SET b = a; CREATE INDEX CONCURRENTLY t_ab_idx ON public.t (a, b)",
    )];
    let p = plan(&target, &merged(m), &no_analysis(), &steps, &opts()).unwrap();
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.t ADD COLUMN b integer",
            "UPDATE public.t SET b = a",
            "CREATE INDEX CONCURRENTLY t_ab_idx ON public.t (a, b)",
            "CREATE INDEX CONCURRENTLY t_b_idx ON public.t USING btree (b)",
        ]
    );
    let phases: Vec<Phase> = p.steps.iter().map(|s| s.phase).collect();
    assert_eq!(phases, [Phase::Tx, Phase::Tx, Phase::Online, Phase::Online]);
    assert_eq!(p.steps[1].id.0, "u1.1");
    assert_eq!(p.steps[2].id.0, "u1.2");
}

#[test]
fn refused_user_step_fails_the_plan() {
    let steps = [user("u1", None, "UPDATE t SET a = 1; COMMIT")];
    let err = plan(
        &model(vec![]),
        &merged(model(vec![])),
        &no_analysis(),
        &steps,
        &opts(),
    )
    .unwrap_err();
    let SchemaError::Steps(errors) = err else {
        panic!("{err:?}");
    };
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].index, 1);
    assert_eq!(errors[0].sql, "COMMIT");
}

// ---- scripts ----

#[test]
fn phase1_script_has_guard_marker_and_timeouts() {
    let p = plan_of(
        &model(vec![]),
        &model(vec![table(
            300,
            "public",
            "t",
            vec![col(1, "a", "integer")],
        )]),
    );
    let script = phase1_script(&p, &opts(), "0123abcd");
    // E15: the script runs in the owner's own session (psql -U postgres): no role switch, and
    // the marker insert runs as the owner inside the transaction.
    let expected = format!(
        "BEGIN;\n\
         SET LOCAL lock_timeout = '5000ms';\n\
         SET LOCAL statement_timeout = '15min';\n\
         DO $chelabase_guard$\n\
         BEGIN\n\
         IF ({TARGET_DIGEST_SQL}) IS DISTINCT FROM '0123abcd' THEN\n\
         RAISE EXCEPTION 'chelabase_merge_stale';\n\
         END IF;\n\
         END\n\
         $chelabase_guard$;\n\
         -- step create-table:public.t\n\
         CREATE TABLE public.t (a integer);\n\
         -- step chelabase:marker\n\
         INSERT INTO chelabase_merge.applied (merge_id) VALUES ('01234567-89ab-cdef-0123-456789abcdef');\n\
         -- step chelabase:commit\n\
         COMMIT;\n"
    );
    assert_eq!(script, expected);
    // The digest is one SELECT over the catalog that can be nested in the guard.
    assert!(TARGET_DIGEST_SQL.starts_with("WITH "));
    assert!(TARGET_DIGEST_SQL.contains("pg_catalog.md5("));
    assert!(!TARGET_DIGEST_SQL.contains(';'));
    assert!(!TARGET_DIGEST_SQL.contains("$chelabase_guard$"));
    for excluded in ["'chelabase'", "'chelabase_merge'", "deptype = 'e'"] {
        assert!(TARGET_DIGEST_SQL.contains(excluded), "{excluded}");
    }
    // Same schema exclusions as the catalog reader.
    for schema in crate::exclusions::EXCLUDED_SCHEMAS {
        assert!(TARGET_DIGEST_SQL.contains(&format!("'{schema}'")), "{schema}");
    }
}

#[test]
fn phase1_script_ends_line_comments_before_the_semicolon() {
    let steps = [user("u1", None, "UPDATE public.t SET a = 1 -- backfill")];
    let p = plan(
        &model(vec![]),
        &merged(model(vec![])),
        &no_analysis(),
        &steps,
        &opts(),
    )
    .unwrap();
    let script = phase1_script(&p, &opts(), "x");
    assert!(
        script.contains("-- step u1\nUPDATE public.t SET a = 1 -- backfill\n;\n"),
        "{script}"
    );
}

#[test]
fn scripts_never_switch_roles() {
    // E15: phase 1a, phase 1 and phase 2 all run in the owner's own session.
    let mut target = model(vec![table(100, "public", "t", vec![col(1, "m", "mood")])]);
    target.enums = vec![enum_type(510, "public", "mood", &[(511, "sad")])];
    let mut m_col = col(1, "m", "mood");
    m_col.default = Some("'happy'::mood".into());
    let mut t = table(100, "public", "t", vec![m_col]);
    t.indexes = vec![index(0, "t_m_idx", &["m"])];
    let mut m = model(vec![t]);
    m.enums = vec![enum_type(510, "public", "mood", &[(511, "sad"), (610, "happy")])];
    let p = plan_of(&target, &m);
    let phases: HashSet<Phase> = p.steps.iter().map(|s| s.phase).collect();
    assert!(phases.contains(&Phase::EnumAdd) && phases.contains(&Phase::Online));
    let mut scripts: Vec<String> = p
        .steps
        .iter()
        .filter(|s| s.phase != Phase::Tx)
        .map(|s| step_script(s, &opts()))
        .collect();
    scripts.push(phase1_script(&p, &opts(), "x"));
    for s in &scripts {
        assert!(!s.contains("SET ROLE"), "{s}");
        assert!(!s.contains("RESET ROLE"), "{s}");
        assert!(!s.contains("CREATE SCHEMA"), "the marker setup is separate: {s}");
    }
    let enum_add = p.steps.iter().find(|s| s.phase == Phase::EnumAdd).unwrap();
    assert_eq!(
        step_script(enum_add, &opts()),
        "SET lock_timeout = '5000ms';\nALTER TYPE public.mood ADD VALUE IF NOT EXISTS 'happy' AFTER 'sad';\n"
    );
}

// E19: phase 2 steps that take ACCESS EXCLUSIVE or SHARE ROW EXCLUSIVE, and every phase 1a
// value, wait for their lock at most the merge's lock_timeout (a timeout fails the step; Retry
// resumes it). Concurrent builds and validations keep no lock_timeout.
#[test]
fn locking_online_steps_and_enum_adds_get_the_lock_timeout() {
    let target = model(vec![table(
        16500,
        "public",
        "t",
        vec![col(1, "id", "integer"), col(2, "a", "integer")],
    )]);
    let mut t = table(
        16500,
        "public",
        "t",
        vec![col(1, "id", "integer"), not_null(col(2, "a", "integer"))],
    );
    t.indexes = vec![index(0, "t_id_idx", &["id"])];
    let p = plan_of(&target, &model(vec![t]));
    let o = PlanOptions {
        lock_timeout_ms: 750,
        ..opts()
    };
    let script = |needle: &str| {
        let s = p
            .steps
            .iter()
            .find(|s| s.sql.contains(needle))
            .unwrap_or_else(|| panic!("no step {needle}"));
        assert_eq!(s.phase, Phase::Online, "{needle}");
        step_script(s, &o)
    };
    let timed = "SET statement_timeout = 0;\nSET lock_timeout = '750ms';\n";
    assert_eq!(
        script("SET NOT NULL"),
        format!("{timed}ALTER TABLE public.t ALTER COLUMN a SET NOT NULL;\n")
    );
    assert!(script("DROP CONSTRAINT IF EXISTS").starts_with(timed));
    for weak in ["VALIDATE CONSTRAINT", "CREATE INDEX CONCURRENTLY"] {
        let s = script(weak);
        assert!(s.starts_with("SET statement_timeout = 0;\n"), "{s}");
        assert!(!s.contains("lock_timeout"), "{weak}: {s}");
    }
}

// E19: the online foreign key (`NOT VALID`, SHARE ROW EXCLUSIVE on both tables) waits at most
// the lock_timeout too.
#[test]
fn share_row_exclusive_step_gets_the_lock_timeout() {
    let mut s = step_of_locks(&["SHARE ROW EXCLUSIVE public.a", "SHARE ROW EXCLUSIVE public.b"]);
    s.sql = "ALTER TABLE public.a ADD CONSTRAINT a_b_fk FOREIGN KEY (b) REFERENCES public.b (id) NOT VALID".into();
    let o = PlanOptions {
        lock_timeout_ms: 1200,
        ..opts()
    };
    assert!(
        step_script(&s, &o).starts_with("SET statement_timeout = 0;\nSET lock_timeout = '1200ms';\nALTER TABLE"),
        "{}",
        step_script(&s, &o)
    );
    s.locks = vec!["SHARE UPDATE EXCLUSIVE public.a".into()];
    assert!(!step_script(&s, &o).contains("lock_timeout"));
}

// Fix round 2: `ADD CONSTRAINT … UNIQUE USING INDEX` takes ACCESS EXCLUSIVE (Postgres 17), so it
// waits for its lock at most the lock_timeout (E19); the concurrent build before it doesn't.
#[test]
fn unique_using_index_waits_at_most_the_lock_timeout() {
    let cols = vec![col(1, "a", "integer")];
    let target = model(vec![table(100, "public", "t", cols.clone())]);
    let mut t = table(100, "public", "t", cols);
    t.constraints = vec![constraint(0, "t_a_key", ConstraintKind::Unique, "UNIQUE (a)", &[1])];
    let p = plan_of(&target, &model(vec![t]));
    let o = PlanOptions {
        lock_timeout_ms: 750,
        ..opts()
    };
    let using = step(&p, "ALTER TABLE public.t ADD CONSTRAINT t_a_key UNIQUE USING INDEX t_a_key");
    assert_eq!(using.locks, ["ACCESS EXCLUSIVE public.t"]);
    assert_eq!(
        step_script(using, &o),
        "SET statement_timeout = 0;\nSET lock_timeout = '750ms';\nALTER TABLE public.t ADD CONSTRAINT t_a_key UNIQUE USING INDEX t_a_key;\n"
    );
    let build = step(&p, "CREATE UNIQUE INDEX CONCURRENTLY t_a_key ON public.t USING btree (a)");
    assert!(!step_script(build, &o).contains("lock_timeout"));
}

/// The lock Postgres 17 takes for a phase 2 statement, by its form.
fn pg17_lock(sql: &str) -> &'static str {
    if sql.contains("CONCURRENTLY")
        || sql.contains("VALIDATE CONSTRAINT")
        || sql.starts_with("ALTER INDEX")
        || sql.starts_with("COMMENT ON INDEX")
    {
        "SHARE UPDATE EXCLUSIVE"
    } else if sql.contains("FOREIGN KEY") {
        "SHARE ROW EXCLUSIVE"
    } else {
        // SET NOT NULL, DROP CONSTRAINT, ADD CONSTRAINT … USING INDEX.
        "ACCESS EXCLUSIVE"
    }
}

// Fix round 2: every phase 2 step the planner makes is labelled with the lock Postgres 17
// takes, since E19's lock_timeout follows the labels.
#[test]
fn online_lock_labels_match_postgres_17() {
    // A changed index with a comment: build, drop, rename and comment.
    let cols = vec![col(1, "a", "integer"), col(2, "b", "integer")];
    let mut t = table(100, "public", "t", cols.clone());
    t.indexes = vec![index(110, "t_a_idx", &["a"])];
    let mut m = table(100, "public", "t", cols);
    let mut changed = index(110, "t_a_idx", &["a", "b"]);
    changed.comment = Some("by a and b".into());
    m.indexes = vec![changed];
    let indexes = plan_of(&model(vec![t]), &model(vec![m]));

    // A new NOT NULL, a validated check and a unique on an existing table, and a foreign key
    // waiting for that unique (so it's added online).
    let a = table(200, "public", "a", vec![col(1, "id", "integer"), col(2, "x", "integer")]);
    let b = table(201, "public", "b", vec![col(1, "a_id", "integer")]);
    let target = model(vec![a.clone(), b.clone()]);
    let mut a2 = table(
        200,
        "public",
        "a",
        vec![col(1, "id", "integer"), not_null(col(2, "x", "integer"))],
    );
    a2.constraints = vec![
        constraint(0, "a_id_key", ConstraintKind::Unique, "UNIQUE (id)", &[1]),
        constraint(0, "a_x_check", ConstraintKind::Check, "CHECK ((x > 0))", &[2]),
    ];
    let mut b2 = b.clone();
    b2.constraints = vec![fk(
        0,
        "b_a_id_fkey",
        "FOREIGN KEY (a_id) REFERENCES public.a(id)",
        &[1],
        ("public", "a", &[1]),
    )];
    let constraints = plan_of(&target, &model(vec![a2, b2]));

    let online: Vec<&Step> = indexes
        .steps
        .iter()
        .chain(&constraints.steps)
        .filter(|s| s.phase == Phase::Online)
        .collect();
    for needle in [
        "CREATE INDEX CONCURRENTLY",
        "DROP INDEX CONCURRENTLY",
        "ALTER INDEX IF EXISTS",
        "COMMENT ON INDEX",
        "CREATE UNIQUE INDEX CONCURRENTLY",
        "UNIQUE USING INDEX",
        "VALIDATE CONSTRAINT a_x_check",
        "SET NOT NULL",
        "DROP CONSTRAINT IF EXISTS",
        "FOREIGN KEY",
    ] {
        assert!(
            online.iter().any(|s| s.sql.contains(needle)),
            "no online step {needle}: {:?}",
            online.iter().map(|s| &s.sql).collect::<Vec<_>>()
        );
    }
    for s in online {
        let want = pg17_lock(&s.sql);
        assert!(!s.locks.is_empty(), "{}: unlabelled", s.sql);
        for l in &s.locks {
            assert!(l.starts_with(&format!("{want} ")), "{}: {l} (want {want})", s.sql);
        }
    }
}

/// A phase 2 generated step holding `locks`.
fn step_of_locks(locks: &[&str]) -> Step {
    Step {
        id: StepId("s".into()),
        kind: StepKind::Generated,
        phase: Phase::Online,
        sql: String::new(),
        items: Vec::new(),
        data_loss: false,
        rewrite: false,
        security: false,
        locks: locks.iter().map(|l| l.to_string()).collect(),
        role_op: None,
        online: None,
        breaking: None,
    }
}

#[test]
fn marker_setup_is_hardened_and_gives_the_owner_select_and_insert_only() {
    let sql = marker_setup_sql();
    let lines: Vec<&str> = sql.lines().collect();
    // One transaction, a fixed search path before anything else runs.
    assert_eq!(lines[0], "BEGIN;");
    assert_eq!(lines[1], "SET LOCAL search_path = pg_catalog, pg_temp;");
    assert_eq!(lines.last(), Some(&"COMMIT;"));
    // The ownership and shape checks come before any statement that touches the objects.
    let guard = sql.find("DO $chelabase_marker$").expect("guard");
    let create_schema = sql
        .find("CREATE SCHEMA IF NOT EXISTS chelabase_merge;")
        .expect("schema");
    let create_table = sql
        .find(
            "CREATE TABLE IF NOT EXISTS chelabase_merge.applied (merge_id pg_catalog.uuid PRIMARY KEY, applied_at pg_catalog.timestamptz NOT NULL DEFAULT pg_catalog.now());",
        )
        .expect("table");
    assert!(guard < create_schema && create_schema < create_table, "{sql}");
    let body = &sql[guard..create_schema];
    for needle in [
        "pg_catalog.pg_namespace",
        "nspowner",
        "pg_catalog.pg_class",
        "relowner",
        "session_user",
        "relkind",
        "'r'",
        "pg_catalog.pg_trigger",
        "pg_catalog.pg_rewrite",
        "RAISE EXCEPTION",
    ] {
        assert!(body.contains(needle), "{needle} in {body}");
    }
    // The body parses as PL/pgSQL.
    let start = body.find("$chelabase_marker$").unwrap() + "$chelabase_marker$".len();
    let end = body.rfind("$chelabase_marker$").unwrap();
    pg_query::parse_plpgsql(&format!(
        "CREATE FUNCTION f() RETURNS void LANGUAGE plpgsql AS $f${}$f$",
        &body[start..end]
    ))
    .expect("the guard's body parses");
    // Privileges: nothing for PUBLIC, the owner may read and insert only.
    for needle in [
        "REVOKE ALL ON SCHEMA chelabase_merge FROM PUBLIC;",
        "REVOKE ALL ON TABLE chelabase_merge.applied FROM PUBLIC;",
        "GRANT USAGE ON SCHEMA chelabase_merge TO postgres;",
        "GRANT SELECT, INSERT ON TABLE chelabase_merge.applied TO postgres;",
    ] {
        assert!(sql.contains(needle), "{needle}");
    }
    for forbidden in ["UPDATE", "DELETE", "TRUNCATE", "CREATE ON SCHEMA", "GRANT ALL"] {
        assert!(!sql.contains(forbidden), "{forbidden}");
    }
    pg_query::parse(&sql).expect("the setup parses");
    // The phase 1 marker is the insert only.
    assert_eq!(
        marker_sql(opts().merge_id),
        "INSERT INTO chelabase_merge.applied (merge_id) VALUES ('01234567-89ab-cdef-0123-456789abcdef');"
    );
}

#[test]
fn marker_setup_checks_again_after_creating() {
    // A tenant could create `chelabase_merge` between the first check and the CREATEs (which
    // then do nothing): the same check runs again after them, in the same transaction.
    let sql = marker_setup_sql();
    let first = sql.find("DO $chelabase_marker$").expect("first check");
    let create_table = sql
        .find("CREATE TABLE IF NOT EXISTS chelabase_merge.applied")
        .expect("table");
    let second = sql.rfind("DO $chelabase_marker$").expect("second check");
    assert!(first < create_table && create_table < second, "{sql}");
    let commit = sql.rfind("COMMIT;").unwrap();
    assert!(second < commit);
    // The second check is the same block, and it comes before the owner's grants.
    let block = |at: usize| {
        let end = sql[at..].find("$chelabase_marker$;").unwrap() + at;
        sql[at..end].to_string()
    };
    assert_eq!(block(first), block(second));
    let grant = sql.find("GRANT USAGE ON SCHEMA chelabase_merge").unwrap();
    assert!(second < grant, "{sql}");
    assert_eq!(sql.matches("DO $chelabase_marker$").count(), 2);
    pg_query::parse(&sql).expect("the setup parses");
}

#[test]
fn admin_digest_pins_the_search_path_and_qualifies_operators() {
    // On the superuser session an unqualified `||` could resolve to a tenant's operator.
    let admin = admin_digest_sql();
    assert!(
        admin.starts_with(DIGEST_PRELUDE),
        "{admin}"
    );
    assert_eq!(DIGEST_PRELUDE, "SET search_path = pg_catalog, pg_temp;\n");
    assert!(admin.ends_with(&format!("{TARGET_DIGEST_SQL};\n")));
    pg_query::parse(&admin).expect("the admin digest parses");
    // No unqualified concatenation anywhere in the digest (the guard's copy included).
    let unqualified = TARGET_DIGEST_SQL.replace("OPERATOR(pg_catalog.||)", "");
    assert!(!unqualified.contains("||"), "{unqualified}");
    assert!(TARGET_DIGEST_SQL.contains("OPERATOR(pg_catalog.||)"));
}

#[test]
fn user_statements_at_the_same_anchor_keep_phase_order() {
    // Step A (concurrent, phase 2) and step B (phase 1) on one anchor: B would run in phase 1
    // before A, so B is refused, with or without an anchor.
    let target = model(vec![table(
        100,
        "public",
        "t",
        vec![col(1, "a", "integer")],
    )]);
    let m = model(vec![table(
        100,
        "public",
        "t",
        vec![col(1, "a", "integer"), col(2, "b", "integer")],
    )]);
    let base = plan_of(&target, &m);
    let add = step(&base, "ALTER TABLE public.t ADD COLUMN b integer").id.0.clone();
    for anchor in [None, Some(add.as_str())] {
        let steps = [
            user("a", anchor, "CREATE UNIQUE INDEX CONCURRENTLY t_x ON public.t (a)"),
            user(
                "b",
                anchor,
                "ALTER TABLE public.t ADD CONSTRAINT t_x UNIQUE USING INDEX t_x",
            ),
        ];
        let err = plan(&target, &merged(m.clone()), &no_analysis(), &steps, &opts())
            .expect_err("b runs before a");
        let SchemaError::Steps(errors) = &err else {
            panic!("{err:?}");
        };
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].reason.starts_with("step b:"), "{}", errors[0].reason);
    }
}

#[test]
fn phase1_user_statement_after_an_online_step_is_refused() {
    // Phase 1 runs before phase 2, so a phase 1 statement anchored after a phase 2 step (or
    // following a concurrent statement in its own step) would run before what it follows.
    let target = model(vec![table(
        100,
        "public",
        "t",
        vec![col(1, "a", "integer")],
    )]);
    let mut t = table(100, "public", "t", vec![col(1, "a", "integer")]);
    t.indexes = vec![index(0, "t_a_idx", &["a"])];
    let m = model(vec![t]);
    let base = plan_of(&target, &m);
    let online = step(&base, "CREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a)")
        .id
        .clone();

    let after_online = [user("u1", Some(&online.0), "UPDATE public.t SET a = 1")];
    let err = plan(&target, &merged(m.clone()), &no_analysis(), &after_online, &opts()).unwrap_err();
    let SchemaError::Steps(errors) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].sql, "UPDATE public.t SET a = 1");
    assert!(errors[0].reason.contains("u1"), "{}", errors[0].reason);
    assert!(errors[0].reason.contains("phase 2"), "{}", errors[0].reason);

    // Inside one step: a phase 1 statement after a concurrent one.
    let inside = [user(
        "u2",
        None,
        "CREATE INDEX CONCURRENTLY t_x ON public.t (a); UPDATE public.t SET a = 2",
    )];
    let err = plan(&target, &merged(m.clone()), &no_analysis(), &inside, &opts()).unwrap_err();
    let SchemaError::Steps(errors) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].index, 1);
    assert!(errors[0].reason.contains("u2"), "{}", errors[0].reason);
    assert_eq!(errors[0].sql, "UPDATE public.t SET a = 2");

    // A concurrent statement after an online step is fine: it keeps its place in phase 2.
    let ok = [user(
        "u3",
        Some(&online.0),
        "CREATE INDEX CONCURRENTLY t_y ON public.t (a)",
    )];
    let p = plan(&target, &merged(m), &no_analysis(), &ok, &opts()).unwrap();
    assert_eq!(
        sqls(&p),
        [
            "CREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a)",
            "CREATE INDEX CONCURRENTLY t_y ON public.t (a)",
        ]
    );
}

#[test]
fn new_column_after_a_drop_never_pairs_with_the_dropped_one() {
    // The source drops `b` and adds `c`, which its catalog numbers 3; the target added `d`,
    // also numbered 3 there. T3 numbers `c` after the target's last attnum (4), and the
    // planner drops `b` and adds `c`: `c` never pairs with `b` or `d` by attnum.
    use crate::analysis::{Choice, ConflictKind, Resolution, analyze, merge};
    use crate::identity::{Direction, IdentityMap};
    let base = model(vec![table(
        100,
        "public",
        "t",
        vec![col(1, "a", "integer"), col(2, "b", "integer")],
    )]);
    let target = model(vec![table(
        100,
        "public",
        "t",
        vec![
            col(1, "a", "integer"),
            col(2, "b", "integer"),
            col(3, "d", "integer"),
        ],
    )]);
    let source = model(vec![table(
        100,
        "public",
        "t",
        vec![col(1, "a", "integer"), col(3, "c", "text")],
    )]);
    let map = IdentityMap::self_mapped(&base);
    let dir = Direction::IntoParent;
    let analysis = analyze(&base, &target, &source, &map, dir);
    let resolutions: Vec<Resolution> = analysis
        .conflicts
        .iter()
        .map(|c| {
            assert_eq!(c.kind, ConflictKind::ColumnOrder, "{c:?}");
            Resolution {
                conflict: c.id.clone(),
                choice: Choice::TargetColumnsFirst,
            }
        })
        .collect();
    let m = merge(&analysis, &resolutions, &base, &target, &source, &map, dir).unwrap();
    let attnums: Vec<(&str, i16)> = m.model.tables[0]
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.attnum))
        .collect();
    assert!(attnums.contains(&("c", 4)), "{attnums:?}");
    assert!(attnums.contains(&("d", 3)), "{attnums:?}");
    let p = plan(&target, &m, &analysis, &[], &opts()).unwrap();
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.t DROP COLUMN b",
            "ALTER TABLE public.t ADD COLUMN c text",
        ]
    );
    assert!(step(&p, "ALTER TABLE public.t DROP COLUMN b").data_loss);
}

#[test]
fn new_column_uses_the_merged_models_attnum() {
    // The temporary NOT NULL check of a new column is named after the attnum T3 gave it in the
    // merged model, not one the planner predicts.
    let target = model(vec![table(
        16500,
        "public",
        "t",
        vec![col(1, "id", "integer"), col(2, "a", "integer")],
    )]);
    let m = model(vec![table(
        16500,
        "public",
        "t",
        vec![
            col(1, "id", "integer"),
            col(2, "a", "integer"),
            not_null(col(7, "x", "integer")),
        ],
    )]);
    let p = plan_of(&target, &m);
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.t ADD COLUMN x integer",
            "ALTER TABLE public.t ADD CONSTRAINT chelabase_merge_nn_7_0123456789abcdef0123456789abcdef CHECK (x IS NOT NULL) NOT VALID",
            "ALTER TABLE public.t VALIDATE CONSTRAINT chelabase_merge_nn_7_0123456789abcdef0123456789abcdef",
            "ALTER TABLE public.t ALTER COLUMN x SET NOT NULL",
            "ALTER TABLE public.t DROP CONSTRAINT IF EXISTS chelabase_merge_nn_7_0123456789abcdef0123456789abcdef",
        ]
    );
}

#[test]
fn column_type_change_detaches_the_policies_that_use_it() {
    // Postgres refuses `ALTER COLUMN … TYPE` on a column a policy uses: the policies that name
    // the column are dropped first and created again after the change, all in phase 1.
    let cols = vec![col(1, "a", "integer"), col(2, "b", "integer")];
    let mut t = table(100, "public", "t", cols);
    t.policies = vec![
        policy(110, "uses_a", "(a > 0)"),
        policy(111, "uses_b", "(b > 0)"),
    ];
    let mut checked = policy(112, "checks_a", "true");
    checked.with_check = Some("(a < 10)".into());
    t.policies.push(checked);
    let mut m = t.clone();
    m.columns[0].data_type = "bigint".into();
    let p = plan_of(&model(vec![t]), &model(vec![m]));
    let drop_a = at(&p, "DROP POLICY uses_a ON public.t");
    let drop_c = at(&p, "DROP POLICY checks_a ON public.t");
    let change = at(&p, "ALTER TABLE public.t ALTER COLUMN a TYPE bigint USING a::bigint");
    let create_a = at(
        &p,
        "CREATE POLICY uses_a ON public.t AS PERMISSIVE FOR ALL TO PUBLIC USING (a > 0)",
    );
    let create_c = at(
        &p,
        "CREATE POLICY checks_a ON public.t AS PERMISSIVE FOR ALL TO PUBLIC USING (true) WITH CHECK (a < 10)",
    );
    assert!(drop_a < change && drop_c < change, "{:#?}", sqls(&p));
    assert!(change < create_a && change < create_c, "{:#?}", sqls(&p));
    assert!(
        !sqls(&p).iter().any(|s| s.contains("uses_b")),
        "a policy that doesn't use the column stays: {:#?}",
        sqls(&p)
    );
    assert!(p.steps.iter().all(|s| s.phase == Phase::Tx));
    assert_eq!(p.steps.len(), 5, "{:#?}", sqls(&p));
}

// ---- wider coverage ----

#[test]
fn plan_from_empty_creates_every_type_in_order() {
    let app = rref(400, "app");
    let mut m = model(vec![]);
    m.schemas.push(schema(300, "s"));
    m.schemas[1].grants = vec![grant(app.clone(), "USAGE")];
    m.schemas[1].comment = Some("money".into());
    m.roles = vec![role(400, "app")];
    m.enums = vec![enum_type(310, "s", "e", &[(311, "x"), (312, "y")])];
    let mut seq = sequence(320, "s", "q");
    seq.owned_by = Some(OwnedBy {
        table_oid: Oid(330),
        schema: "s".into(),
        table: "t".into(),
        attnum: 2,
        column: "n".into(),
    });
    seq.grants = vec![grant(app.clone(), "USAGE")];
    m.sequences = vec![seq];
    let mut id = not_null(col(1, "id", "bigint"));
    id.identity = Some(Identity {
        generation: IdentityGeneration::Always,
        sequence_oid: Oid(321),
        sequence_name: "t_id_seq".into(),
        params: seq_params(),
        grants: Vec::new(),
    });
    let mut n = col(2, "n", "bigint");
    n.default = Some("nextval('s.q'::regclass)".into());
    let mut g = col(3, "g", "bigint");
    g.generated = Some("(n * 2)".into());
    let mut e = col(4, "e", "s.e");
    e.collation = None;
    e.comment = Some("state".into());
    let mut t = table(330, "s", "t", vec![id, n, g, e]);
    t.unlogged = true;
    t.options = vec!["fillfactor=70".into()];
    t.comment = Some("things".into());
    t.constraints = vec![constraint(
        331,
        "t_pkey",
        ConstraintKind::PrimaryKey,
        "PRIMARY KEY (id)",
        &[1],
    )];
    let mut ix = index(332, "t_n_idx", &["n"]);
    ix.predicate = Some("n > 0".into());
    ix.include = vec!["g".into()];
    t.indexes = vec![ix];
    t.policies = vec![policy(333, "p", "(n > 0)")];
    t.rls_enabled = true;
    t.rls_forced = true;
    m.tables = vec![t];
    m.default_privileges = vec![DefaultPrivilege {
        oid: Oid(340),
        role: postgres(),
        schema: Some("s".into()),
        object_type: DefaultPrivilegeObject::Tables,
        grants: vec![grant(app.clone(), "SELECT")],
    }];

    let p = plan_of(&model(vec![]), &m);
    assert_eq!(
        sqls(&p),
        [
            "CREATE ROLE app WITH NOLOGIN INHERIT NOBYPASSRLS CONNECTION LIMIT -1",
            "CREATE SCHEMA s",
            "COMMENT ON SCHEMA s IS 'money'",
            "CREATE TYPE s.e AS ENUM ('x', 'y')",
            "CREATE SEQUENCE s.q AS bigint INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 1 CACHE 1 NO CYCLE",
            "CREATE UNLOGGED TABLE s.t (id bigint GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME s.t_id_seq START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 CACHE 1 NO CYCLE) NOT NULL, n bigint DEFAULT nextval('s.q'::regclass), g bigint GENERATED ALWAYS AS (n * 2) STORED, e s.e) WITH (fillfactor='70')",
            "COMMENT ON TABLE s.t IS 'things'",
            "COMMENT ON COLUMN s.t.e IS 'state'",
            "ALTER SEQUENCE s.q OWNED BY s.t.n",
            "ALTER TABLE s.t ADD CONSTRAINT t_pkey PRIMARY KEY (id)",
            "CREATE INDEX t_n_idx ON s.t USING btree (n) INCLUDE (g) WHERE (n > 0)",
            "ALTER TABLE s.t ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE s.t FORCE ROW LEVEL SECURITY",
            "CREATE POLICY p ON s.t AS PERMISSIVE FOR ALL TO PUBLIC USING (n > 0)",
            "GRANT USAGE ON SCHEMA s TO app",
            "GRANT USAGE ON SEQUENCE s.q TO app",
            "ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA s GRANT SELECT ON TABLES TO app",
        ]
    );
    // Every statement is valid SQL (the role step is several statements).
    for s in &p.steps {
        pg_query::parse(&s.sql).unwrap_or_else(|e| panic!("{}: {e}", s.sql));
    }
}

#[test]
fn drop_and_recreate_is_a_drop_and_a_create() {
    let old = table(100, "public", "t", vec![col(1, "a", "integer")]);
    let new = table(900, "public", "t", vec![col(1, "b", "text")]);
    let mut analysis = no_analysis();
    analysis.items = vec![
        applied(
            "table:100",
            key(ObjKind::Table, Some("public"), None, "t"),
            Change::Drop(ObjSnapshot::table(&old)),
        ),
        applied(
            "table:name:public.t",
            key(ObjKind::Table, Some("public"), None, "t"),
            Change::Create(ObjSnapshot::table(&new)),
        ),
    ];
    analysis.items[0].data_loss = true;
    let p = plan(
        &model(vec![old]),
        &merged(model(vec![new])),
        &analysis,
        &[],
        &opts(),
    )
    .unwrap();
    assert_eq!(
        sqls(&p),
        ["DROP TABLE public.t", "CREATE TABLE public.t (b text)"]
    );
    assert!(p.steps[0].data_loss);
}

#[test]
fn rename_onto_a_dropped_name_moves_the_dropped_object_aside() {
    // t2 is renamed to t1 while the old t1 is dropped.
    let t1 = table(100, "public", "t1", vec![col(1, "a", "integer")]);
    let t2 = table(101, "public", "t2", vec![col(1, "b", "integer")]);
    let renamed_t2 = table(101, "public", "t1", vec![col(1, "b", "integer")]);
    let mut analysis = no_analysis();
    analysis.items = vec![renamed(
        "table:101",
        key(ObjKind::Table, Some("public"), None, "t2"),
        ObjSnapshot::table(&t2),
        ObjSnapshot::table(&renamed_t2),
    )];
    let p = plan(
        &model(vec![t1, t2]),
        &merged(model(vec![renamed_t2])),
        &analysis,
        &[],
        &opts(),
    )
    .unwrap();
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.t1 RENAME TO chelabase_merge_tmp_1",
            "ALTER TABLE public.t2 RENAME TO t1",
            "DROP TABLE public.chelabase_merge_tmp_1",
        ]
    );
}

#[test]
fn column_changes_on_an_existing_table() {
    let mut a = col(1, "a", "integer");
    a.default = Some("0".into());
    let target = model(vec![table(
        100,
        "public",
        "t",
        vec![a, not_null(col(2, "b", "text")), col(3, "c", "text")],
    )]);
    let mut a2 = col(1, "a", "bigint");
    a2.default = Some("1".into());
    let mut c2 = col(3, "c", "text");
    c2.comment = Some("note".into());
    let mut d = not_null(col(4, "d", "integer"));
    d.default = Some("7".into());
    let m = model(vec![table(
        100,
        "public",
        "t",
        vec![a2, col(2, "b", "text"), c2, d],
    )]);
    let p = plan_of(&target, &m);
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.t ALTER COLUMN a DROP DEFAULT",
            "ALTER TABLE public.t ALTER COLUMN a TYPE bigint USING a::bigint",
            "ALTER TABLE public.t ALTER COLUMN a SET DEFAULT 1",
            "ALTER TABLE public.t ALTER COLUMN b DROP NOT NULL",
            "COMMENT ON COLUMN public.t.c IS 'note'",
            "ALTER TABLE public.t ADD COLUMN d integer DEFAULT 7 NOT NULL",
        ]
    );
    let ty = step(
        &p,
        "ALTER TABLE public.t ALTER COLUMN a TYPE bigint USING a::bigint",
    );
    assert!(ty.rewrite);
    // No analysis item says the change is safe: flagged.
    assert!(ty.data_loss);
}

#[test]
fn grants_on_an_existing_table_are_diffed() {
    let app = rref(400, "app");
    let web = rref(401, "web");
    let mut t = table(100, "public", "t", vec![col(1, "a", "integer")]);
    t.grants = vec![
        grant(
            RoleRef::Role {
                oid: Oid(10),
                name: "postgres".into(),
            },
            "SELECT",
        ),
        grant(app.clone(), "SELECT"),
        grant(web.clone(), "INSERT"),
    ];
    let mut m = t.clone();
    let mut with_option = grant(app.clone(), "SELECT");
    with_option.grantable = true;
    m.grants = vec![
        grant(
            RoleRef::Role {
                oid: Oid(10),
                name: "postgres".into(),
            },
            "SELECT",
        ),
        with_option,
        grant(RoleRef::Public, "SELECT"),
    ];
    let p = plan_of(&model(vec![t]), &model(vec![m]));
    assert_eq!(
        sqls(&p),
        [
            "REVOKE INSERT ON TABLE public.t FROM web",
            "GRANT SELECT ON TABLE public.t TO PUBLIC",
            "GRANT SELECT ON TABLE public.t TO app WITH GRANT OPTION",
        ]
    );
}

#[test]
fn new_table_grants_skip_what_default_privileges_give() {
    let anon = rref(402, "anon");
    let app = rref(400, "app");
    let mut target = model(vec![]);
    target.default_privileges = vec![DefaultPrivilege {
        oid: Oid(1),
        role: postgres(),
        schema: Some("public".into()),
        object_type: DefaultPrivilegeObject::Tables,
        grants: vec![grant(anon.clone(), "SELECT")],
    }];
    let mut t = table(300, "public", "t", vec![col(1, "a", "integer")]);
    t.grants = vec![grant(anon, "SELECT"), grant(app, "SELECT")];
    // The merged model keeps the target's default privileges.
    let mut m = model(vec![t]);
    m.default_privileges = target.default_privileges.clone();
    let p = plan_of(&target, &m);
    assert_eq!(
        sqls(&p),
        [
            "CREATE TABLE public.t (a integer)",
            "GRANT SELECT ON TABLE public.t TO app",
        ]
    );
}

#[test]
fn renames_follow_the_targets_oid_and_new_objects_never_match_by_oid() {
    // The merged model keeps the target's OIDs and attnums; new objects carry OID 0.
    let mut old = table(100, "public", "old", vec![col(1, "a", "integer")]);
    old.indexes = vec![index(110, "old_a_idx", &["a"])];
    let mut new = table(100, "public", "new", vec![col(1, "b", "integer")]);
    new.indexes = vec![index(110, "new_b_idx", &["b"])];
    let fresh = table(0, "public", "fresh", vec![col(1, "x", "integer")]);
    let p = plan_of(
        &model(vec![old, table(0, "public", "gone", vec![])]),
        &model(vec![new, fresh]),
    );
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.old RENAME TO new",
            "ALTER INDEX public.old_a_idx RENAME TO new_b_idx",
            "ALTER TABLE public.new RENAME COLUMN a TO b",
            "DROP TABLE public.gone",
            "CREATE TABLE public.fresh (x integer)",
        ]
    );
}

#[test]
fn dropped_tables_in_a_reference_cycle_lose_their_foreign_keys_first() {
    let mut a = table(100, "public", "a", vec![col(1, "b_id", "integer")]);
    a.constraints = vec![fk(
        110,
        "a_b_fkey",
        "FOREIGN KEY (b_id) REFERENCES public.b(id)",
        &[1],
        ("public", "b", &[1]),
    )];
    let mut b = table(101, "public", "b", vec![col(1, "a_id", "integer")]);
    b.constraints = vec![fk(
        111,
        "b_a_fkey",
        "FOREIGN KEY (a_id) REFERENCES public.a(id)",
        &[1],
        ("public", "a", &[1]),
    )];
    let p = plan_of(&model(vec![a, b]), &model(vec![]));
    assert_eq!(
        sqls(&p),
        [
            "ALTER TABLE public.a DROP CONSTRAINT a_b_fkey",
            "ALTER TABLE public.b DROP CONSTRAINT b_a_fkey",
            "DROP TABLE public.a",
            "DROP TABLE public.b",
        ]
    );
}

#[test]
fn digest_and_scripts_parse() {
    pg_query::parse(TARGET_DIGEST_SQL).expect("the digest parses");
    let p = plan_of(
        &model(vec![table(
            100,
            "public",
            "t",
            vec![col(1, "a", "integer")],
        )]),
        &model(vec![{
            let mut t = table(100, "public", "t", vec![not_null(col(1, "a", "integer"))]);
            t.indexes = vec![index(0, "t_a_idx", &["a"])];
            t
        }]),
    );
    pg_query::parse(&phase1_script(&p, &opts(), "abc")).expect("phase 1 parses");
    for s in p.steps.iter().filter(|s| s.phase == Phase::Online) {
        pg_query::parse(&step_script(s, &opts())).expect("a phase 2 script parses");
    }
    pg_query::parse(&crate::export::export_sql(&p, &opts())).expect("the export parses");
}

// ---- T14 fixes ----

fn try_plan(target: &SchemaModel, m: &SchemaModel, steps: &[UserStep]) -> Result<Plan, SchemaError> {
    plan(target, &merged(m.clone()), &no_analysis(), steps, &opts())
}

fn step_errors(r: Result<Plan, SchemaError>) -> Vec<StepError> {
    match r {
        Err(SchemaError::Steps(e)) => e,
        other => panic!("not a steps error: {other:?}"),
    }
}

// T14 minor 2: the script marks the marker insert and the COMMIT with lines of their own, and
// records where every part starts, so a failure maps by line, never by re-scanning user text.
#[test]
fn phase1_script_maps_every_line_to_the_part_the_generator_wrote() {
    let target = model(vec![table(100, "public", "t", vec![col(1, "a", "integer")])]);
    let m = model(vec![
        table(100, "public", "t", vec![col(1, "a", "integer")]),
        table(300, "public", "u", vec![col(1, "b", "integer")]),
    ]);
    // A user statement with a line that looks like a step marker, and several lines.
    let steps = [user(
        "fill",
        None,
        "UPDATE public.t\n-- step create-table:public.u\nSET a = 1",
    )];
    let p = try_plan(&target, &m, &steps).unwrap();
    let s = phase1(&p, &opts(), "abc");
    assert_eq!(s.text, phase1_script(&p, &opts(), "abc"));
    assert!(
        s.text.ends_with(&format!(
            "-- step chelabase:marker\n{}\n-- step chelabase:commit\nCOMMIT;\n",
            marker_sql(opts().merge_id)
        )),
        "{}",
        s.text
    );
    let lines: Vec<&str> = s.text.lines().collect();
    let line_of = |needle: &str| lines.iter().position(|l| l.starts_with(needle)).unwrap() + 1;
    assert_eq!(s.at_line(1), Some(&ScriptPart::Prelude));
    assert_eq!(s.at_line(line_of("DO $chelabase_guard$")), Some(&ScriptPart::Guard));
    assert_eq!(s.at_line(line_of("RAISE EXCEPTION")), Some(&ScriptPart::Guard));
    let fill = StepId("fill".into());
    assert_eq!(s.at_line(line_of("UPDATE public.t")), Some(&ScriptPart::Step(fill.clone())));
    // The fake marker inside the user's statement is still the user's step.
    assert_eq!(
        s.at_line(line_of("-- step create-table:public.u")),
        Some(&ScriptPart::Step(fill.clone()))
    );
    assert_eq!(s.at_line(line_of("SET a = 1")), Some(&ScriptPart::Step(fill)));
    assert_eq!(
        s.at_line(line_of("CREATE TABLE public.u")),
        Some(&ScriptPart::Step(StepId("create-table:public.u".into())))
    );
    assert_eq!(s.at_line(line_of("INSERT INTO chelabase_merge")), Some(&ScriptPart::Marker));
    assert_eq!(s.at_line(line_of("COMMIT;")), Some(&ScriptPart::Commit));
    assert_eq!(s.at_line(0), None);
    assert_eq!(s.at_line(lines.len() + 1), None);
    // Every line belongs to exactly the part whose region holds it, in order.
    let starts: Vec<usize> = s.parts.iter().map(|p| p.first_line).collect();
    assert!(starts.windows(2).all(|w| w[0] < w[1]), "{starts:?}");
    assert_eq!(starts[0], 1);
    for line in 1..=lines.len() {
        assert!(s.at_line(line).is_some(), "line {line}");
    }
}

// T14 minor 3: a user step id can't be a generated or another statement's id: `:` and `.`
// (which generated and split ids use) are reserved, and so are control characters and
// duplicates; refused before anything is planned.
#[test]
fn user_step_ids_never_collide_with_generated_or_split_ids() {
    let m = model(vec![table(300, "public", "t", vec![col(1, "a", "integer")])]);
    let target = model(vec![]);
    for bad in [
        "create-table:public.t",
        "own:c1",
        "u.1",
        "x:y",
        "a\nb",
        "",
        "chelabase:marker",
        // T14 F3 fix round (M8): phase 1's own entry in the log.
        "phase1",
    ] {
        let errors = step_errors(try_plan(&target, &m, &[user(bad, None, "SELECT 1")]));
        assert_eq!(errors.len(), 1, "{bad:?}");
        assert!(errors[0].reason.contains("step id"), "{bad:?}: {}", errors[0].reason);
    }
    // Two user steps with one id.
    let errors = step_errors(try_plan(
        &target,
        &m,
        &[user("u", None, "SELECT 1"), user("u", None, "SELECT 2")],
    ));
    assert!(errors.iter().any(|e| e.reason.contains("step id")), "{errors:?}");
    // Every id a plan can hold stays unique.
    let p = try_plan(
        &target,
        &m,
        &[user("u", None, "SELECT 1; SELECT 2"), user("u-1_x", None, "SELECT 3")],
    )
    .unwrap();
    let mut ids: Vec<&str> = p.steps.iter().map(|s| s.id.0.as_str()).collect();
    let n = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), n);
}

// T14 minor 6 / E30: nothing about the owner is ever planned.
#[test]
fn the_owner_is_never_planned() {
    let mut target = model(vec![]);
    target.roles = vec![role(10, "postgres")];
    let mut m = model(vec![]);
    let mut owner = role(10, "postgres");
    owner.connection_limit = 7;
    owner.login = true;
    owner.valid_until = Some("2030-01-01 00:00:00+00".into());
    m.roles = vec![owner];
    assert!(plan_of(&target, &m).steps.is_empty());
}

// E28: a role created with owner-level, or turned owner-level, needs an acknowledgement.
#[test]
fn owner_level_role_steps_are_security() {
    let mut target = model(vec![]);
    target.roles = vec![role(401, "app")];
    let mut m = model(vec![]);
    let mut app = role(401, "app");
    app.member_of = vec![membership(16399, OWNER_LEVEL_ROLE)];
    let mut boss = role(0, "boss");
    boss.member_of = vec![membership(16399, OWNER_LEVEL_ROLE)];
    m.roles = vec![app, boss, role(0, "plain")];
    let p = plan_of(&target, &m);
    let by_id = |id: &str| p.steps.iter().find(|s| s.id.0 == id).unwrap();
    assert!(by_id("role:update:app").security);
    assert!(by_id("role:create:boss").security);
    assert!(!by_id("role:create:plain").security);
    assert!(by_id("role:create:boss").needs_acknowledgement());
    assert!(!by_id("role:create:plain").needs_acknowledgement());
}

// E29 / E29a: a source role rename (the target's OID under a new name) is renamed through the
// control plane, first in phase 0, flagged breaking; nothing else about it (grants, owners,
// policies) is planned as a change of name.
#[test]
fn role_rename_is_planned_first_and_flagged_breaking() {
    let mut target = model(vec![]);
    let mut t = table(100, "public", "t", vec![col(1, "a", "integer")]);
    t.grants = vec![grant(rref(401, "a"), "SELECT")];
    t.owner = named(401, "a");
    t.policies = vec![Policy {
        roles: vec![rref(401, "a")],
        ..policy(110, "p", "true")
    }];
    target.tables = vec![t.clone()];
    target.roles = vec![role(401, "a"), role(402, "other")];
    let mut m = model(vec![]);
    let mut t2 = t.clone();
    t2.grants = vec![grant(rref(401, "b"), "SELECT")];
    t2.owner = named(401, "b");
    t2.policies[0].roles = vec![rref(401, "b")];
    m.tables = vec![t2];
    let mut b = role(401, "b");
    b.inherit = false;
    m.roles = vec![b, role(402, "other"), role(0, "fresh")];
    let p = plan_of(&target, &m);
    assert_eq!(
        sqls(&p),
        [
            "ALTER ROLE a RENAME TO b",
            "ALTER ROLE b WITH NOINHERIT",
            "CREATE ROLE fresh WITH NOLOGIN INHERIT NOBYPASSRLS CONNECTION LIMIT -1",
        ]
    );
    let rename = &p.steps[0];
    assert_eq!(rename.phase, Phase::Roles);
    assert_eq!(
        rename.role_op,
        Some(RoleOp::Rename {
            oid: Oid(401),
            from: "a".into(),
            to: "b".into()
        })
    );
    assert_eq!(
        rename.breaking,
        Some(Breaking::RoleRename {
            role: "a".into(),
            new_name: "b".into()
        })
    );
    assert!(rename.needs_acknowledgement());
    let message = rename.breaking.as_ref().unwrap().message();
    assert!(message.contains("a") && message.contains("authenticate"), "{message}");
    assert!(p.steps[1].breaking.is_none());
    assert_eq!(
        p.steps[1].role_op,
        Some(RoleOp::Update {
            name: "b".into(),
            patch: RolePatchModel {
                inherit: Some(false),
                ..Default::default()
            }
        })
    );
}

// E29: renames that swap or chain names run in an order where each new name is free; a
// cycle, or a new name another role keeps, can't be planned.
#[test]
fn role_renames_chain_in_order_and_refuse_cycles_and_collisions() {
    let mut target = model(vec![]);
    target.roles = vec![role(401, "a"), role(402, "b")];
    // b → c, then a → b.
    let mut m = model(vec![]);
    m.roles = vec![role(401, "b"), role(402, "c")];
    assert_eq!(
        sqls(&plan_of(&target, &m)),
        ["ALTER ROLE b RENAME TO c", "ALTER ROLE a RENAME TO b"]
    );
    // a ↔ b.
    let mut swap = model(vec![]);
    swap.roles = vec![role(401, "b"), role(402, "a")];
    let err = try_plan(&target, &swap, &[]).unwrap_err();
    assert!(
        matches!(&err, SchemaError::Unplannable { reason, .. }
            if reason.contains("cycle") && reason.contains("on the target before merging")),
        "{err:?}"
    );
    // a → b while b stays (the merge never drops a role).
    let mut taken = model(vec![]);
    taken.roles = vec![role(401, "b")];
    let err = try_plan(&target, &taken, &[]).unwrap_err();
    assert!(matches!(&err, SchemaError::Unplannable { .. }), "{err:?}");
}

/// The invariant behind T14 minors 10–12: every enum value phase 1 uses is added in phase 1a
/// and committed before it, whatever its text, and placing user steps sees the phases the
/// enum move gives. Exhaustive over label shapes (quotes, backslashes, both) and uses (a
/// default, a user step).
#[test]
fn every_enum_value_phase1_uses_is_added_in_phase1a() {
    let labels = ["happy", "it's", "a\\b", "x'\\y", "\\"];
    for label in labels {
        let lit = format!("'{}'", label.replace('\'', "''"));
        for via_user in [false, true] {
            let mut target = model(vec![table(100, "public", "t", vec![col(1, "m", "mood")])]);
            target.enums = vec![enum_type(510, "public", "mood", &[(511, "sad")])];
            let mut m_col = col(1, "m", "mood");
            if !via_user {
                // pg_get_expr's form: standard-conforming, quotes doubled.
                m_col.default = Some(format!("{lit}::mood"));
            }
            let mut m = model(vec![table(100, "public", "t", vec![m_col])]);
            m.enums = vec![enum_type(510, "public", "mood", &[(511, "sad"), (610, label)])];
            let steps = if via_user {
                vec![user("set", None, &format!("UPDATE public.t SET m = {lit}"))]
            } else {
                vec![]
            };
            let p = try_plan(&target, &m, &steps).unwrap();
            let add = p
                .steps
                .iter()
                .find(|s| s.id.0.starts_with("add-enum-value:"))
                .unwrap();
            assert_eq!(add.phase, Phase::EnumAdd, "{label:?} via user {via_user}");
        }
    }
}

// T14 minor 11: an added value equal to a target value the plan renames away can't go to
// phase 1a (it would find the old value still there).
#[test]
fn enum_add_after_a_rename_away_is_unplannable_when_phase1_uses_it() {
    let mut target = model(vec![table(100, "public", "t", vec![col(1, "m", "mood")])]);
    target.enums = vec![enum_type(510, "public", "mood", &[(511, "a")])];
    let mut m_col = col(1, "m", "mood");
    m_col.default = Some("'a'::mood".into());
    let mut m = model(vec![table(100, "public", "t", vec![m_col])]);
    m.enums = vec![enum_type(510, "public", "mood", &[(511, "b"), (610, "a")])];
    let before = enum_type(510, "public", "mood", &[(511, "a")]);
    let after = enum_type(510, "public", "mood", &[(511, "b")]);
    let mut analysis = no_analysis();
    analysis.items = vec![renamed(
        "enum:510",
        key(ObjKind::Enum, Some("public"), None, "mood"),
        ObjSnapshot::Enum(before),
        ObjSnapshot::Enum(after),
    )];
    let err = plan(&target, &merged(m.clone()), &analysis, &[], &opts()).unwrap_err();
    assert!(
        matches!(&err, SchemaError::Unplannable { reason, .. } if reason.contains("renames")),
        "{err:?}"
    );
    // Not used in phase 1: it stays in phase 1, after the rename.
    let mut unused = m;
    unused.tables[0].columns[0].default = None;
    let p = plan(&target, &merged(unused), &analysis, &[], &opts()).unwrap();
    let rename = p.steps.iter().position(|s| s.sql.contains("RENAME VALUE")).unwrap();
    let add = p.steps.iter().position(|s| s.sql.contains("ADD VALUE")).unwrap();
    assert!(rename < add && p.steps[add].phase == Phase::Tx, "{:#?}", sqls(&p));
}

// T14 minor 12: a user ADD VALUE anchored after a generated add that phase 1a takes is placed
// with the final phases, not refused.
#[test]
fn user_add_value_after_a_generated_add_moved_to_phase1a_is_accepted() {
    let mut target = model(vec![table(100, "public", "t", vec![col(1, "m", "mood")])]);
    target.enums = vec![enum_type(510, "public", "mood", &[(511, "sad")])];
    let mut m_col = col(1, "m", "mood");
    m_col.default = Some("'happy'::mood".into());
    let mut m = model(vec![table(100, "public", "t", vec![m_col])]);
    m.enums = vec![enum_type(510, "public", "mood", &[(511, "sad"), (610, "happy")])];
    let anchor = step(
        &plan_of(&target, &m),
        "ALTER TYPE public.mood ADD VALUE IF NOT EXISTS 'happy' AFTER 'sad'",
    )
    .id
    .clone();
    let steps = [user(
        "more",
        Some(&anchor.0),
        "ALTER TYPE public.mood ADD VALUE IF NOT EXISTS 'glad'",
    )];
    let p = try_plan(&target, &m, &steps).unwrap();
    let phases: Vec<(&str, Phase)> = p.steps.iter().map(|s| (s.id.0.as_str(), s.phase)).collect();
    assert_eq!(
        phases,
        [
            ("add-enum-value:public.mood.happy", Phase::EnumAdd),
            ("more", Phase::EnumAdd),
            ("set-default:public.t.m", Phase::Tx),
        ]
    );
}

// T14 minor 13: turning a column into a generated one drops and adds it: the policies using it
// are detached first, and its indexes, constraints and grants (which the drop takes with it)
// are made again.
#[test]
fn generated_conversion_detaches_policies_and_rebuilds_what_the_drop_takes() {
    let cols = vec![col(1, "a", "integer"), col(2, "b", "integer")];
    let mut t = table(100, "public", "t", cols);
    t.policies = vec![policy(110, "uses_b", "(b > 0)")];
    t.indexes = vec![index(120, "t_b_idx", &["b"])];
    t.constraints = vec![constraint(130, "t_b_check", ConstraintKind::Check, "CHECK ((b > 0))", &[2])];
    t.columns[1].grants = vec![grant(rref(401, "app"), "SELECT")];
    let mut m = t.clone();
    m.columns[1].generated = Some("(a * 2)".into());
    let p = plan_of(&model(vec![t]), &model(vec![m]));
    let drop_policy = at(&p, "DROP POLICY uses_b ON public.t");
    let drop_col = at(&p, "ALTER TABLE public.t DROP COLUMN b");
    let add_col = at(
        &p,
        "ALTER TABLE public.t ADD COLUMN b integer GENERATED ALWAYS AS (a * 2) STORED",
    );
    let policy = at(
        &p,
        "CREATE POLICY uses_b ON public.t AS PERMISSIVE FOR ALL TO PUBLIC USING (b > 0)",
    );
    let check = at(
        &p,
        "ALTER TABLE public.t ADD CONSTRAINT t_b_check CHECK ((b > 0)) NOT VALID",
    );
    let index = at(&p, "CREATE INDEX CONCURRENTLY t_b_idx ON public.t USING btree (b)");
    let grant = at(&p, "GRANT SELECT (b) ON TABLE public.t TO app");
    assert!(drop_policy < drop_col && drop_col < add_col, "{:#?}", sqls(&p));
    assert!(add_col < policy && add_col < check && add_col < index && add_col < grant, "{:#?}", sqls(&p));
}

// T14 minor 14: a policy's comment is part of it: planned, and kept when the policy is made
// again.
#[test]
fn policy_comments_are_planned() {
    let cols = vec![col(1, "a", "integer")];
    let mut t = table(100, "public", "t", cols.clone());
    t.policies = vec![Policy {
        comment: Some("old".into()),
        ..policy(110, "p", "(a > 0)")
    }];
    let mut m = t.clone();
    m.policies[0].comment = Some("it's new".into());
    let p = plan_of(&model(vec![t.clone()]), &model(vec![m.clone()]));
    assert_eq!(sqls(&p), ["COMMENT ON POLICY p ON public.t IS 'it''s new'"]);
    // Re-created (its expression changed): the comment comes back.
    m.policies[0].using = Some("(a > 1)".into());
    let p = plan_of(&model(vec![t]), &model(vec![m]));
    let create = at(
        &p,
        "CREATE POLICY p ON public.t AS PERMISSIVE FOR ALL TO PUBLIC USING (a > 1)",
    );
    let comment = at(&p, "COMMENT ON POLICY p ON public.t IS 'it''s new'");
    assert!(create < comment);
}

// T14 minor 15: a default that isn't constant at ADD COLUMN time (a volatile function) rewrites
// the table.
#[test]
fn volatile_defaults_rewrite_the_table() {
    let target = model(vec![table(100, "public", "t", vec![col(1, "a", "integer")])]);
    for (default, rewrites) in [
        ("random()", true),
        ("clock_timestamp()", true),
        ("gen_random_uuid()", true),
        ("nextval('public.s'::regclass)", true),
        ("(random() * (10)::double precision)", true),
        ("now()", false),
        ("CURRENT_TIMESTAMP", false),
        ("'x'::text", false),
        ("42", false),
        ("lower('A'::text)", false),
        // Fix round M6: only Postgres's own functions; a user's `public.now()` may be anything.
        ("public.now()", true),
        ("pg_catalog.now()", false),
        ("timezone('UTC'::text, now())", false),
    ] {
        let mut c = col(2, "b", "text");
        c.default = Some(default.into());
        let m = model(vec![table(100, "public", "t", vec![col(1, "a", "integer"), c])]);
        let p = plan_of(&target, &m);
        let add = p.steps.iter().find(|s| s.id.0.starts_with("add-column")).unwrap();
        assert_eq!(add.rewrite, rewrites, "{default}");
    }
}

// T14 minor 16: a user's phase 2 and phase 1a statements must survive a resume: a concurrent
// index is named and on a schema-qualified table (checked by name, rebuilt when invalid), a
// concurrent drop says IF EXISTS, and an added enum value IF NOT EXISTS.
#[test]
fn user_online_and_enum_statements_are_resumable() {
    let target = model(vec![table(100, "public", "t", vec![col(1, "a", "integer")])]);
    let m = target.clone();
    let p = try_plan(
        &target,
        &m,
        &[user("ix", None, "CREATE INDEX CONCURRENTLY t_a_idx ON public.t (a)")],
    )
    .unwrap();
    assert_eq!(
        p.steps[0].online,
        Some(OnlineCheck::IndexValid {
            schema: "public".into(),
            table: "t".into(),
            name: "t_a_idx".into(),
        })
    );
    for (sql, why) in [
        ("CREATE INDEX CONCURRENTLY ON public.t (a)", "name"),
        ("CREATE INDEX CONCURRENTLY t_a_idx ON t (a)", "schema"),
        ("DROP INDEX CONCURRENTLY public.t_a_idx", "IF EXISTS"),
        ("ALTER TYPE public.e ADD VALUE 'x'", "IF NOT EXISTS"),
    ] {
        let errors = step_errors(try_plan(&target, &m, &[user("u", None, sql)]));
        assert_eq!(errors.len(), 1, "{sql}");
        assert!(errors[0].reason.contains(why), "{sql}: {}", errors[0].reason);
    }
    for ok in [
        "DROP INDEX CONCURRENTLY IF EXISTS public.t_a_idx",
        "ALTER TYPE public.e ADD VALUE IF NOT EXISTS 'x'",
        "REINDEX INDEX CONCURRENTLY public.t_a_idx",
    ] {
        try_plan(&target, &m, &[user("u", None, ok)]).unwrap_or_else(|e| panic!("{ok}: {e:?}"));
    }
}

// T14 minor 17: the temporary NOT NULL check carries the merge id, so another merge's leftover
// never collides with it.
#[test]
fn not_null_check_name_is_unique_per_merge() {
    let target = model(vec![table(16500, "public", "t", vec![col(1, "a", "integer")])]);
    let m = model(vec![table(16500, "public", "t", vec![not_null(col(1, "a", "integer"))])]);
    let p = plan_of(&target, &m);
    let check = p
        .steps
        .iter()
        .find(|s| s.id.0.starts_with("not-null-check"))
        .unwrap();
    let name = format!(
        "{}1_{}",
        crate::exclusions::MERGE_NOT_NULL_CHECK_PREFIX,
        opts().merge_id.simple()
    );
    assert!(name.len() < 64);
    assert_eq!(
        check.sql,
        format!("ALTER TABLE public.t ADD CONSTRAINT {name} CHECK (a IS NOT NULL) NOT VALID")
    );
}

// T14 minor 20: the phase 1 guard's DO body is valid PL/pgSQL.
#[test]
fn phase1_guard_body_parses_as_plpgsql() {
    let p = plan_of(&model(vec![]), &model(vec![]));
    let script = phase1_script(&p, &opts(), "it's");
    let start = script.find("DO $chelabase_guard$").unwrap() + "DO $chelabase_guard$".len();
    let end = script.rfind("$chelabase_guard$").unwrap();
    pg_query::parse_plpgsql(&format!(
        "CREATE FUNCTION f() RETURNS void LANGUAGE plpgsql AS $f${}$f$",
        &script[start..end]
    ))
    .expect("the guard's body parses");
}

// Fix round M1: memberships are compared with the target as it reads after the renames, so a
// renamed group the role stays in is no membership change.
#[test]
fn a_renamed_group_is_no_membership_change() {
    let mut target = model(vec![]);
    let mut a = role(401, "a");
    a.member_of = vec![membership(402, "g")];
    target.roles = vec![a.clone(), role(402, "g")];
    let mut m = model(vec![]);
    a.member_of = vec![membership(402, "h")];
    m.roles = vec![a, role(402, "h")];
    assert_eq!(sqls(&plan_of(&target, &m)), ["ALTER ROLE g RENAME TO h"]);
}

// Fix round M5: whether `DROP COLUMN` took an index is read from the target's index: an
// index the merged model now names the column in, but the target's didn't, isn't re-made as
// new (it's changed as usual).
#[test]
fn conversion_rebuilds_only_the_indexes_the_drop_took() {
    let cols = vec![col(1, "a", "integer"), col(2, "b", "integer")];
    let mut t = table(100, "public", "t", cols);
    t.indexes = vec![index(120, "t_a_idx", &["a"])];
    let mut m = t.clone();
    m.columns[1].generated = Some("(a * 2)".into());
    m.indexes[0].keys = vec!["a".into(), "b".into()];
    let p = plan_of(&model(vec![t]), &model(vec![m]));
    assert!(
        !sqls(&p).contains(&"CREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a, b)"),
        "{:#?}",
        sqls(&p)
    );
    // Rebuilt through a temporary index, as a changed index is.
    assert!(
        p.steps.iter().any(|s| s.sql.starts_with("CREATE INDEX CONCURRENTLY chelabase_merge_ix_")),
        "{:#?}",
        sqls(&p)
    );
}

// ---- E27: what an abandoned merge leaves (T14 F3 fix round, D2) ----

/// The ids of `p`'s first `n` online steps: the ones the log records as done when phase 2
/// stopped at the next.
fn online_done(p: &Plan, n: usize) -> std::collections::HashSet<String> {
    p.steps
        .iter()
        .filter(|s| s.phase == Phase::Online)
        .take(n)
        .map(|s| s.id.0.clone())
        .collect()
}

fn left(kind: fn(String, String, String) -> Leftover, name: &str) -> Leftover {
    kind("public".into(), "t".into(), name.into())
}

fn invalid_index(schema: String, table: String, name: String) -> Leftover {
    Leftover::InvalidIndex { schema, table, name }
}

fn unattached_index(schema: String, table: String, name: String) -> Leftover {
    Leftover::UnattachedIndex { schema, table, name }
}

// D2: a replaced index (E19): stopped in the build, the temporary index may be INVALID; built
// but not renamed into place, it's a valid index nothing uses; stopped in the old index's
// concurrent drop, that one may be INVALID too.
#[test]
fn abandon_lists_the_indexes_of_a_replaced_index() {
    let cols = vec![col(1, "a", "integer"), col(2, "b", "integer")];
    let mut t = table(100, "public", "t", cols.clone());
    t.indexes = vec![index(110, "t_a_idx", &["a"])];
    let mut m = table(100, "public", "t", cols);
    m.indexes = vec![index(110, "t_a_idx", &["a", "b"])];
    let p = plan_of(&model(vec![t]), &model(vec![m]));
    let tmp = "chelabase_merge_ix_1_0123456789abcdef0123456789abcdef";
    assert_eq!(leftovers(&p, &online_done(&p, 0)), [left(invalid_index, tmp)]);
    assert_eq!(
        leftovers(&p, &online_done(&p, 1)),
        [left(unattached_index, tmp), left(invalid_index, "t_a_idx")]
    );
    assert_eq!(leftovers(&p, &online_done(&p, 2)), [left(unattached_index, tmp)]);
    assert_eq!(leftovers(&p, &online_done(&p, 3)), []);
}

// D2: a unique constraint on a table the target has: its index built but not attached is a
// valid index nothing uses; the constraint itself never existed, so it's no NOT VALID one.
#[test]
fn abandon_lists_a_unique_index_not_attached() {
    let cols = vec![col(1, "a", "integer")];
    let target = model(vec![table(100, "public", "t", cols.clone())]);
    let mut t = table(100, "public", "t", cols);
    t.constraints = vec![constraint(0, "t_a_key", ConstraintKind::Unique, "UNIQUE (a)", &[1])];
    let p = plan_of(&target, &model(vec![t]));
    assert_eq!(leftovers(&p, &online_done(&p, 0)), [left(invalid_index, "t_a_key")]);
    assert_eq!(leftovers(&p, &online_done(&p, 1)), [left(unattached_index, "t_a_key")]);
    assert_eq!(leftovers(&p, &online_done(&p, 2)), []);
}

// A NOT VALID constraint is listed only once it exists: a foreign key added in phase 2 whose
// add didn't run left nothing; added but not validated, it's left NOT VALID. The kinds read as
// the API serves them.
#[test]
fn abandon_lists_a_not_valid_constraint_only_once_it_exists() {
    let step = |id: &str, sql: &str, online: OnlineCheck| Step {
        id: StepId(id.into()),
        kind: StepKind::Generated,
        phase: Phase::Online,
        sql: sql.into(),
        items: Vec::new(),
        data_loss: false,
        rewrite: false,
        security: false,
        locks: Vec::new(),
        role_op: None,
        online: Some(online),
        breaking: None,
    };
    let (schema, table, name) = ("public".to_string(), "t".to_string(), "t_fk".to_string());
    let p = Plan {
        steps: vec![
            step(
                "add",
                "ALTER TABLE public.t ADD CONSTRAINT t_fk FOREIGN KEY (a) REFERENCES public.u (id) NOT VALID",
                OnlineCheck::ConstraintExists { schema: schema.clone(), table: table.clone(), name: name.clone() },
            ),
            step(
                "validate",
                "ALTER TABLE public.t VALIDATE CONSTRAINT t_fk",
                OnlineCheck::ConstraintValidated { schema, table, name },
            ),
        ],
    };
    assert_eq!(leftovers(&p, &online_done(&p, 0)), []);
    let not_valid = leftovers(&p, &online_done(&p, 1));
    assert_eq!(
        serde_json::to_value(&not_valid).unwrap(),
        serde_json::json!([{ "kind": "not_valid_constraint", "schema": "public", "table": "t", "name": "t_fk" }])
    );
    assert_eq!(
        serde_json::to_value([left(invalid_index, "i"), left(unattached_index, "u")]).unwrap(),
        serde_json::json!([
            { "kind": "invalid_index", "schema": "public", "table": "t", "name": "i" },
            { "kind": "unattached_index", "schema": "public", "table": "t", "name": "u" },
        ])
    );
}
