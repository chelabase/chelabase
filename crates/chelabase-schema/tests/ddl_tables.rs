use chelabase_schema::ddl::{render, DdlErrorKind, RenderContext, RenderedStatement};
use chelabase_types::table_api::*;

fn t(schema: &str, name: &str) -> TableRef {
    TableRef { schema: schema.into(), name: name.into() }
}
fn ty(name: &str) -> TypeSpec {
    TypeSpec { schema: "pg_catalog".into(), name: name.into(), modifiers: vec![], array: false }
}
fn col(name: &str, type_name: &str) -> ColumnSpec {
    ColumnSpec { name: name.into(), data_type: ty(type_name), not_null: false, default: None, identity: None, generated: None, collation: None, comment: None }
}
fn sqls(ops: &[Operation]) -> Vec<String> {
    stmts(ops).into_iter().map(|s| s.sql).collect()
}
fn stmts(ops: &[Operation]) -> Vec<RenderedStatement> {
    render(ops, &RenderContext::default()).unwrap()
}
fn err(ops: &[Operation]) -> DdlErrorKind {
    render(ops, &RenderContext::default()).unwrap_err().kind
}
fn notes() -> TableRef {
    t("public", "notes")
}
fn create(columns: Vec<ColumnSpec>) -> CreateTable {
    CreateTable { table: notes(), columns, primary_key: vec![], constraints: vec![], rls: None, policies: vec![], grants: vec![], comment: None, owner: None }
}

#[test]
fn create_table_minimal() {
    let mut id = col("id", "int8");
    id.identity = Some(IdentityKind::Always);
    id.not_null = true;
    let mut body = col("body", "text");
    body.default = Some("''".into());
    body.not_null = true;
    let mut c = create(vec![id, body]);
    c.primary_key = vec!["id".into()];
    let out = stmts(&[Operation::CreateTable(c)]);
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].sql,
        "CREATE TABLE public.notes (id pg_catalog.int8 GENERATED ALWAYS AS IDENTITY NOT NULL, body pg_catalog.text DEFAULT '' NOT NULL, PRIMARY KEY (id))"
    );
    assert_eq!((out[0].op_index, out[0].rewrites_table, out[0].lock, out[0].concurrently), (0, false, LockLevel::None, false));
    assert_eq!(out[0].table, Some(notes()));
}

#[test]
fn create_table_full_orders_its_statements() {
    let table = t("app", "Notes");
    let mut body = col("body", "text");
    body.comment = Some("The text".into());
    let user_id = col("user_id", "uuid");
    let c = CreateTable {
        table: table.clone(),
        columns: vec![body, user_id],
        primary_key: vec![],
        constraints: vec![
            ConstraintSpec { name: Some("body_len".into()), kind: ConstraintKindSpec::Check { expression: "length(body) < 1000".into() }, not_valid: false },
            ConstraintSpec { name: None, kind: ConstraintKindSpec::Unique { columns: vec!["body".into()], nulls_not_distinct: false }, not_valid: false },
            ConstraintSpec {
                name: Some("notes_user_fk".into()),
                kind: ConstraintKindSpec::ForeignKey {
                    columns: vec!["user_id".into()],
                    references: t("public", "users"),
                    referenced_columns: vec!["id".into()],
                    on_delete: FkAction::Cascade,
                    on_update: FkAction::NoAction,
                    deferrable: false,
                    initially_deferred: false,
                },
                not_valid: false,
            },
        ],
        rls: Some(RlsSpec { enabled: true, forced: true }),
        policies: vec![PolicySpec {
            name: "own_rows".into(),
            command: PolicyCommandSpec::Select,
            permissive: true,
            roles: vec!["authenticated".into()],
            using: Some("user_id = auth.uid()".into()),
            with_check: None,
        }],
        grants: vec![TableGrant { columns: vec![], privileges: vec![Privilege::Select, Privilege::Insert], grantee: Grantee::Role("authenticated".into()), with_grant_option: false }],
        comment: Some("Team notes".into()),
        owner: Some("app_owner".into()),
    };
    assert_eq!(
        sqls(&[Operation::CreateTable(c)]),
        [
            "CREATE TABLE app.\"Notes\" (body pg_catalog.text, user_id pg_catalog.uuid, CONSTRAINT body_len CHECK (length(body) < 1000), UNIQUE (body), CONSTRAINT notes_user_fk FOREIGN KEY (user_id) REFERENCES public.users (id) ON DELETE CASCADE)",
            "COMMENT ON TABLE app.\"Notes\" IS 'Team notes'",
            "COMMENT ON COLUMN app.\"Notes\".body IS 'The text'",
            "ALTER TABLE app.\"Notes\" ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE app.\"Notes\" FORCE ROW LEVEL SECURITY",
            "CREATE POLICY own_rows ON app.\"Notes\" AS PERMISSIVE FOR SELECT TO authenticated USING (user_id = auth.uid())",
            "GRANT SELECT ON app.\"Notes\" TO authenticated",
            "GRANT INSERT ON app.\"Notes\" TO authenticated",
            "ALTER TABLE app.\"Notes\" OWNER TO app_owner",
        ]
    );
}

#[test]
fn types_render_qualified_with_modifiers_and_arrays() {
    let mut v = col("v", "varchar");
    v.data_type.modifiers = vec![200];
    let mut n = col("n", "numeric");
    n.data_type.modifiers = vec![10, 2];
    let mut tags = col("tags", "text");
    tags.data_type.array = true;
    let mut mood = col("mood", "Mood");
    mood.data_type.schema = "public".into();
    let mut c = col("c", "text");
    c.collation = Some(CollationRef { schema: "pg_catalog".into(), name: "C".into() });
    assert_eq!(
        sqls(&[Operation::CreateTable(create(vec![v, n, tags, mood, c]))]),
        ["CREATE TABLE public.notes (v pg_catalog.\"varchar\"(200), n pg_catalog.\"numeric\"(10,2), tags pg_catalog.text[], mood public.\"Mood\", c pg_catalog.text COLLATE pg_catalog.\"C\")"]
    );
}

#[test]
fn a_generated_column() {
    let mut g = col("total", "int4");
    g.generated = Some("a + b".into());
    assert_eq!(
        sqls(&[Operation::AddColumn { table: notes(), column: g }]),
        ["ALTER TABLE public.notes ADD COLUMN total pg_catalog.int4 GENERATED ALWAYS AS (a + b) STORED"]
    );
}

#[test]
fn table_operations() {
    let ops = [
        Operation::RenameTable { table: notes(), to: "memos".into() },
        Operation::SetTableSchema { table: notes(), schema: "archive".into() },
        Operation::DropTable { table: t("public", "old") },
        Operation::SetTableComment { table: notes(), comment: Some("it's".into()) },
        Operation::SetTableComment { table: notes(), comment: None },
        Operation::SetTableOwner { table: notes(), owner: "Team".into() },
    ];
    assert_eq!(
        sqls(&ops),
        [
            "ALTER TABLE public.notes RENAME TO memos",
            "ALTER TABLE public.notes SET SCHEMA archive",
            "DROP TABLE public.old RESTRICT",
            "COMMENT ON TABLE public.notes IS 'it''s'",
            "COMMENT ON TABLE public.notes IS NULL",
            "ALTER TABLE public.notes OWNER TO \"Team\"",
        ]
    );
    let out = stmts(&ops);
    assert_eq!(out.iter().map(|s| s.op_index).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5]);
    assert_eq!(out[2].lock, LockLevel::AccessExclusive);
    assert_eq!(out[3].lock, LockLevel::ShareUpdateExclusive);
}

#[test]
fn cascade_turns_restrict_into_cascade() {
    let ctx = RenderContext { cascade: true, ..Default::default() };
    let out = render(&[Operation::DropTable { table: notes() }, Operation::DropColumn { table: t("public", "users"), column: "x".into() }], &ctx).unwrap();
    assert_eq!(out[0].sql, "DROP TABLE public.notes CASCADE");
    assert_eq!(out[1].sql, "ALTER TABLE public.users DROP COLUMN x CASCADE");
}

#[test]
fn column_operations() {
    let mut tags = col("tags", "text");
    tags.data_type.array = true;
    tags.default = Some("'{}'".into());
    tags.not_null = true;
    tags.comment = Some("labels".into());
    assert_eq!(
        sqls(&[
            Operation::AddColumn { table: notes(), column: tags },
            Operation::RenameColumn { table: notes(), column: "body".into(), to: "Body".into() },
            Operation::DropColumn { table: notes(), column: "old_body".into() },
        ]),
        [
            "ALTER TABLE public.notes ADD COLUMN tags pg_catalog.text[] DEFAULT '{}' NOT NULL",
            "COMMENT ON COLUMN public.notes.tags IS 'labels'",
            "ALTER TABLE public.notes RENAME COLUMN body TO \"Body\"",
            "ALTER TABLE public.notes DROP COLUMN old_body RESTRICT",
        ]
    );
}

#[test]
fn alter_column_emits_each_change_in_order() {
    let mut v = ty("varchar");
    v.modifiers = vec![200];
    let change = ColumnChange {
        data_type: Some(v),
        using: Some("body::varchar(200)".into()),
        collation: Some(CollationRef { schema: "pg_catalog".into(), name: "C".into() }),
        default: Some(DefaultChange::Set("''".into())),
        not_null: Some(true),
        identity: None,
        comment: Some(CommentChange::Clear),
    };
    assert_eq!(
        sqls(&[Operation::AlterColumn { table: notes(), column: "body".into(), change }]),
        [
            // `USING` isn't applied to the old default: it goes first and comes back after.
            "ALTER TABLE public.notes ALTER COLUMN body DROP DEFAULT",
            "ALTER TABLE public.notes ALTER COLUMN body TYPE pg_catalog.\"varchar\"(200) COLLATE pg_catalog.\"C\" USING body::varchar(200)",
            "ALTER TABLE public.notes ALTER COLUMN body SET DEFAULT ''",
            "ALTER TABLE public.notes ALTER COLUMN body SET NOT NULL",
            "COMMENT ON COLUMN public.notes.body IS NULL",
        ]
    );
    let one = |change: ColumnChange| sqls(&[Operation::AlterColumn { table: notes(), column: "id".into(), change }]);
    assert_eq!(one(ColumnChange { default: Some(DefaultChange::Drop), ..Default::default() }), ["ALTER TABLE public.notes ALTER COLUMN id DROP DEFAULT"]);
    assert_eq!(one(ColumnChange { not_null: Some(false), ..Default::default() }), ["ALTER TABLE public.notes ALTER COLUMN id DROP NOT NULL"]);
    assert_eq!(
        one(ColumnChange { identity: Some(IdentityChange::Add(IdentityKind::ByDefault)), ..Default::default() }),
        ["ALTER TABLE public.notes ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY"]
    );
    assert_eq!(
        one(ColumnChange { identity: Some(IdentityChange::Set(IdentityKind::Always)), ..Default::default() }),
        ["ALTER TABLE public.notes ALTER COLUMN id SET GENERATED ALWAYS"]
    );
    assert_eq!(one(ColumnChange { identity: Some(IdentityChange::Drop), ..Default::default() }), ["ALTER TABLE public.notes ALTER COLUMN id DROP IDENTITY"]);
    assert_eq!(
        one(ColumnChange { comment: Some(CommentChange::Set("x".into())), ..Default::default() }),
        ["COMMENT ON COLUMN public.notes.id IS 'x'"]
    );
}

// T2 review, finding 1: Postgres refuses `SET DEFAULT` and `DROP NOT NULL` on an identity
// column, and `ADD … AS IDENTITY` needs NOT NULL first.
#[test]
fn identity_changes_order_so_postgres_accepts_them() {
    let one = |change: ColumnChange| sqls(&[Operation::AlterColumn { table: notes(), column: "id".into(), change }]);
    assert_eq!(
        one(ColumnChange {
            identity: Some(IdentityChange::Drop),
            not_null: Some(false),
            default: Some(DefaultChange::Set("0".into())),
            ..Default::default()
        }),
        [
            "ALTER TABLE public.notes ALTER COLUMN id DROP IDENTITY",
            "ALTER TABLE public.notes ALTER COLUMN id SET DEFAULT 0",
            "ALTER TABLE public.notes ALTER COLUMN id DROP NOT NULL",
        ]
    );
    assert_eq!(
        one(ColumnChange { identity: Some(IdentityChange::Add(IdentityKind::Always)), not_null: Some(true), ..Default::default() }),
        [
            "ALTER TABLE public.notes ALTER COLUMN id SET NOT NULL",
            "ALTER TABLE public.notes ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY",
        ]
    );
    assert_eq!(
        one(ColumnChange { identity: Some(IdentityChange::Drop), data_type: Some(ty("int4")), ..Default::default() }),
        [
            "ALTER TABLE public.notes ALTER COLUMN id DROP IDENTITY",
            "ALTER TABLE public.notes ALTER COLUMN id TYPE pg_catalog.int4",
        ]
    );
}

// T2 review, finding 2: a type change drops the old default first (once) and sets the new one
// after; only the `TYPE` statement may rewrite.
#[test]
fn a_type_change_drops_the_default_first() {
    let ts = ty("timestamptz");
    let alter = |default: Option<DefaultChange>| {
        stmts(&[Operation::AlterColumn {
            table: notes(),
            column: "at".into(),
            change: ColumnChange { data_type: Some(ts.clone()), using: Some("to_timestamp(at)".into()), default, ..Default::default() },
        }])
    };
    let dropped = alter(Some(DefaultChange::Drop));
    assert_eq!(
        dropped.iter().map(|s| s.sql.as_str()).collect::<Vec<_>>(),
        [
            "ALTER TABLE public.notes ALTER COLUMN at DROP DEFAULT",
            "ALTER TABLE public.notes ALTER COLUMN at TYPE pg_catalog.timestamptz USING to_timestamp(at)",
        ]
    );
    let set = alter(Some(DefaultChange::Set("now()".into())));
    assert_eq!(
        set.iter().map(|s| s.sql.as_str()).collect::<Vec<_>>(),
        [
            "ALTER TABLE public.notes ALTER COLUMN at DROP DEFAULT",
            "ALTER TABLE public.notes ALTER COLUMN at TYPE pg_catalog.timestamptz USING to_timestamp(at)",
            "ALTER TABLE public.notes ALTER COLUMN at SET DEFAULT now()",
        ]
    );
    assert_eq!(set.iter().map(|s| s.rewrites_table).collect::<Vec<_>>(), [false, true, false]);
    // Without a default change, the type change stands alone.
    assert_eq!(alter(None).len(), 1);
}

// T2 review, finding 3: a type outside `pg_catalog` may be a domain with constraints, which
// makes Postgres rewrite the table even with no default: unknown counts as rewriting.
#[test]
fn a_column_of_a_type_outside_pg_catalog_counts_as_rewriting() {
    let mut c = col("p", "posint");
    c.data_type.schema = "public".into();
    assert!(stmts(&[Operation::AddColumn { table: notes(), column: c }])[0].rewrites_table);
}

// T2 review, finding 4: `--` and `/*` start comments, so no operator contains them.
#[test]
fn operators_never_contain_comment_starts() {
    let exclusion = |op: &str| Operation::AddConstraint {
        table: notes(),
        constraint: ConstraintSpec {
            name: None,
            kind: ConstraintKindSpec::Exclusion {
                method: "gist".into(),
                elements: vec![ExclusionElement { column: "a".into(), operator: OperatorRef { schema: "pg_catalog".into(), name: op.into() } }],
                predicate: None,
            },
            not_valid: false,
        },
    };
    for bad in ["--", "=--", "&&/*", "/*", "<-->"] {
        assert_eq!(err(&[exclusion(bad)]), DdlErrorKind::InvalidOperator(bad.into()), "{bad}");
    }
    for good in ["=", "&&", "<->", "@>", "-|-", "~~*", "*/"] {
        assert!(render(&[exclusion(good)], &RenderContext::default()).is_ok(), "{good}");
    }
}

#[test]
fn constraint_operations() {
    let add = |name: Option<&str>, kind: ConstraintKindSpec, not_valid: bool| Operation::AddConstraint {
        table: notes(),
        constraint: ConstraintSpec { name: name.map(Into::into), kind, not_valid },
    };
    let ops = [
        add(Some("body_len"), ConstraintKindSpec::Check { expression: "length(body) < 1000".into() }, true),
        add(None, ConstraintKindSpec::PrimaryKey { columns: vec!["id".into()] }, false),
        add(Some("notes_body_key"), ConstraintKindSpec::Unique { columns: vec!["body".into()], nulls_not_distinct: true }, false),
        add(
            Some("notes_user_fk"),
            ConstraintKindSpec::ForeignKey {
                columns: vec!["user_id".into()],
                references: t("public", "users"),
                referenced_columns: vec!["id".into()],
                on_delete: FkAction::SetNull,
                on_update: FkAction::Restrict,
                deferrable: true,
                initially_deferred: true,
            },
            true,
        ),
        add(
            Some("no_overlap"),
            ConstraintKindSpec::Exclusion {
                method: "gist".into(),
                elements: vec![
                    ExclusionElement { column: "room".into(), operator: OperatorRef { schema: "pg_catalog".into(), name: "=".into() } },
                    ExclusionElement { column: "during".into(), operator: OperatorRef { schema: "pg_catalog".into(), name: "&&".into() } },
                ],
                predicate: Some("active".into()),
            },
            false,
        ),
        Operation::ValidateConstraint { table: notes(), name: "body_len".into() },
        Operation::RenameConstraint { table: notes(), name: "body_len".into(), to: "body_max".into() },
        Operation::DropConstraint { table: notes(), name: "body_max".into() },
    ];
    assert_eq!(
        sqls(&ops),
        [
            "ALTER TABLE public.notes ADD CONSTRAINT body_len CHECK (length(body) < 1000) NOT VALID",
            "ALTER TABLE public.notes ADD PRIMARY KEY (id)",
            "ALTER TABLE public.notes ADD CONSTRAINT notes_body_key UNIQUE NULLS NOT DISTINCT (body)",
            "ALTER TABLE public.notes ADD CONSTRAINT notes_user_fk FOREIGN KEY (user_id) REFERENCES public.users (id) ON DELETE SET NULL ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED NOT VALID",
            "ALTER TABLE public.notes ADD CONSTRAINT no_overlap EXCLUDE USING gist (room WITH pg_catalog.=, during WITH pg_catalog.&&) WHERE (active)",
            "ALTER TABLE public.notes VALIDATE CONSTRAINT body_len",
            "ALTER TABLE public.notes RENAME CONSTRAINT body_len TO body_max",
            "ALTER TABLE public.notes DROP CONSTRAINT body_max RESTRICT",
        ]
    );
    let locks: Vec<LockLevel> = stmts(&ops).into_iter().map(|s| s.lock).collect();
    {
        use LockLevel::*;
        assert_eq!(locks, [AccessExclusive, AccessExclusive, AccessExclusive, ShareRowExclusive, AccessExclusive, ShareUpdateExclusive, AccessExclusive, AccessExclusive]);
    }
}

#[test]
fn add_column_rewrite_flag() {
    let with_default = |d: Option<&str>| {
        let mut c = col("x", "int4");
        c.default = d.map(Into::into);
        stmts(&[Operation::AddColumn { table: notes(), column: c }])[0].rewrites_table
    };
    for plain in [None, Some("0"), Some("-1.5"), Some("'x'"), Some("TRUE"), Some("false"), Some("null")] {
        assert!(!with_default(plain), "{plain:?}");
    }
    for volatile in [Some("now()"), Some("gen_random_uuid()"), Some("'x'::text"), Some("1 + 1")] {
        assert!(with_default(volatile), "{volatile:?}");
    }
    let mut id = col("id2", "int8");
    id.identity = Some(IdentityKind::ByDefault);
    assert!(stmts(&[Operation::AddColumn { table: notes(), column: id }])[0].rewrites_table);
}

#[test]
fn validation_errors_name_the_operation() {
    let long = "x".repeat(64);
    let e = render(&[Operation::DropTable { table: notes() }, Operation::RenameTable { table: notes(), to: long.clone() }], &RenderContext::default()).unwrap_err();
    assert_eq!((e.op_index, e.kind), (1, DdlErrorKind::InvalidName(long)));
    assert_eq!(err(&[Operation::RenameTable { table: notes(), to: String::new() }]), DdlErrorKind::InvalidName(String::new()));
    assert_eq!(err(&[Operation::RenameTable { table: notes(), to: "a\0b".into() }]), DdlErrorKind::InvalidName("a\0b".into()));
    assert!(render(&[Operation::RenameTable { table: notes(), to: "é".repeat(31) }], &RenderContext::default()).is_ok(), "62 bytes");
    assert_eq!(err(&[Operation::CreateTable(create(vec![]))]), DdlErrorKind::Empty("columns"));
    assert_eq!(err(&[Operation::AlterColumn { table: notes(), column: "a".into(), change: ColumnChange::default() }]), DdlErrorKind::Empty("change"));
    assert_eq!(
        err(&[Operation::AlterColumn { table: notes(), column: "a".into(), change: ColumnChange { using: Some("a".into()), ..Default::default() } }]),
        DdlErrorKind::Unsupported("using or collation without a type")
    );
    let pk = ConstraintSpec { name: None, kind: ConstraintKindSpec::PrimaryKey { columns: vec![] }, not_valid: false };
    assert_eq!(err(&[Operation::AddConstraint { table: notes(), constraint: pk }]), DdlErrorKind::Empty("columns"));
    let bad_op = ConstraintSpec {
        name: None,
        kind: ConstraintKindSpec::Exclusion {
            method: "gist".into(),
            elements: vec![ExclusionElement { column: "a".into(), operator: OperatorRef { schema: "pg_catalog".into(), name: "=; drop".into() } }],
            predicate: None,
        },
        not_valid: false,
    };
    assert_eq!(err(&[Operation::AddConstraint { table: notes(), constraint: bad_op }]), DdlErrorKind::InvalidOperator("=; drop".into()));
    let bad_method = ConstraintSpec {
        name: None,
        kind: ConstraintKindSpec::Exclusion { method: "Gist x".into(), elements: vec![], predicate: None },
        not_valid: false,
    };
    assert_eq!(err(&[Operation::AddConstraint { table: notes(), constraint: bad_method }]), DdlErrorKind::InvalidMethod("Gist x".into()));
}
