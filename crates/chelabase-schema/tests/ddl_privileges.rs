//! T4 (members hub step 1): the privilege ops' rendering (spec Part 4 "After the red-team",
//! MH-26–29, S4, S5, S21). Every statement is one statement per privilege, as the existing
//! grant rendering does (`tests/ddl_objects.rs`), since the verify matches per privilege.

use std::collections::{BTreeMap, BTreeSet};

use chelabase_schema::ddl::*;
use chelabase_schema::model::Table;
use chelabase_schema::privileges::{ALL_ROWS_MARKER, all_rows_policy_name};
use chelabase_types::table_api::*;
use proptest::prelude::*;

const OWNER: &str = "postgres";

fn t(schema: &str, name: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        name: name.into(),
    }
}
fn notes() -> TableRef {
    t("public", "notes")
}
fn role(r: &str) -> Grantee {
    Grantee::Role(r.into())
}
/// A Postgres 17 context with no owners: a role run as `session_role`.
fn pg17(session_role: &str) -> RenderContext {
    RenderContext {
        server_version_num: 170_005,
        session_role: session_role.into(),
        ..Default::default()
    }
}
fn sqls(ops: &[Operation], ctx: &RenderContext) -> Vec<String> {
    render(ops, ctx)
        .unwrap_or_else(|e| panic!("{e:?}"))
        .into_iter()
        .map(|s| s.sql)
        .collect()
}
fn refused(ops: &[Operation], ctx: &RenderContext) -> (usize, DdlErrorKind) {
    let e = render(ops, ctx).unwrap_err();
    (e.op_index, e.kind)
}
fn function(oid: u32, identity: &str) -> FunctionRef {
    FunctionRef {
        oid,
        identity: identity.into(),
    }
}
fn resolved(pairs: &[(u32, &str)]) -> BTreeMap<u32, String> {
    pairs.iter().map(|(o, s)| (*o, s.to_string())).collect()
}

/// A context table `schema.name` with `policies` (catalog JSON shape).
fn context_table(schema: &str, name: &str, policies: serde_json::Value) -> ContextTable {
    let table: Table = serde_json::from_value(serde_json::json!({
        "oid": 1, "schema": schema, "name": name, "owner": { "oid": 10, "name": "app_owner" }, "comment": null,
        "rls_enabled": true, "rls_forced": false, "unlogged": false, "options": [],
        "columns": [{ "attnum": 1, "name": "id", "data_type": "integer", "type_oid": 23, "collation": null, "default": null,
                      "not_null": false, "identity": null, "generated": null, "comment": null, "grants": [] }],
        "constraints": [], "indexes": [], "policies": policies, "grants": []
    }))
    .unwrap();
    ContextTable {
        table,
        total_bytes: 0,
    }
}

/// A catalog policy: `FOR ALL`, permissive unless said, roles by name.
fn policy(
    name: &str,
    permissive: bool,
    roles: &[&str],
    using: &str,
    with_check: Option<&str>,
    comment: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "oid": 5, "name": name, "command": "all", "permissive": permissive,
        "roles": roles.iter().map(|r| serde_json::json!({ "role": { "oid": 30, "name": r } })).collect::<Vec<_>>(),
        "using": using, "with_check": with_check, "comment": comment
    })
}

fn parse_one(sql: &str) -> pg_query::NodeEnum {
    let parsed = pg_query::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(parsed.protobuf.stmts.len(), 1, "{sql}");
    parsed.protobuf.stmts[0]
        .stmt
        .as_ref()
        .unwrap()
        .node
        .clone()
        .unwrap()
}

#[test]
fn schema_ops_render_quoted_one_statement_each() {
    let ctx = pg17("acting");
    let ops = [
        Operation::GrantSchema {
            schema: "App".into(),
            grantee: role("Readers"),
            privileges: vec![SchemaPrivilege::Usage, SchemaPrivilege::Create],
        },
        Operation::RevokeSchema {
            schema: "app".into(),
            grantee: role("readers"),
            privileges: vec![SchemaPrivilege::Create],
            grant_option_only: false,
        },
    ];
    let out = render(&ops, &ctx).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        out.iter().map(|s| s.sql.as_str()).collect::<Vec<_>>(),
        [
            "GRANT USAGE ON SCHEMA \"App\" TO \"Readers\"",
            "GRANT CREATE ON SCHEMA \"App\" TO \"Readers\"",
            "REVOKE CREATE ON SCHEMA app FROM readers",
        ]
    );
    assert_eq!(
        out.iter().map(|s| s.op_index).collect::<Vec<_>>(),
        [0, 0, 1]
    );
    for s in &out {
        assert!(
            matches!(parse_one(&s.sql), pg_query::NodeEnum::GrantStmt(_)),
            "{}",
            s.sql
        );
        assert!(
            s.lock == LockLevel::None && !s.concurrently && !s.rewrites_table,
            "{s:?}"
        );
    }
    let empty = Operation::GrantSchema {
        schema: "app".into(),
        grantee: role("readers"),
        privileges: vec![],
    };
    assert_eq!(
        refused(&[empty], &ctx),
        (0, DdlErrorKind::Empty("privileges"))
    );
    let bad = Operation::GrantSchema {
        schema: "x".repeat(64),
        grantee: role("readers"),
        privileges: vec![SchemaPrivilege::Usage],
    };
    assert_eq!(
        refused(&[bad], &ctx),
        (0, DdlErrorKind::InvalidName("x".repeat(64)))
    );
}

#[test]
fn revoke_grant_option_only_renders_grant_option_for() {
    let ctx = RenderContext {
        resolved_functions: resolved(&[(16384, "app.f(integer)")]),
        ..pg17("acting")
    };
    assert_eq!(
        sqls(
            &[
                Operation::RevokeSchema {
                    schema: "app".into(),
                    grantee: role("readers"),
                    privileges: vec![SchemaPrivilege::Usage],
                    grant_option_only: true
                },
                Operation::RevokeSequence {
                    sequence: t("app", "s"),
                    grantee: role("readers"),
                    privileges: vec![SequencePrivilege::Select],
                    grant_option_only: true
                },
                Operation::RevokeFunction {
                    function: function(16384, "app.f(integer)"),
                    grantee: role("readers"),
                    grant_option_only: true
                },
                Operation::RevokeSchema {
                    schema: "app".into(),
                    grantee: role("readers"),
                    privileges: vec![SchemaPrivilege::Usage],
                    grant_option_only: false
                },
            ],
            &ctx
        ),
        [
            "REVOKE GRANT OPTION FOR USAGE ON SCHEMA app FROM readers",
            "REVOKE GRANT OPTION FOR SELECT ON SEQUENCE app.s FROM readers",
            "REVOKE GRANT OPTION FOR EXECUTE ON ROUTINE app.f(integer) FROM readers",
            "REVOKE USAGE ON SCHEMA app FROM readers",
        ]
    );
}

#[test]
fn sequence_ops_render() {
    let ctx = pg17("acting");
    let out = render(
        &[
            Operation::GrantSequence {
                sequence: t("app", "Seq"),
                grantee: role("readers"),
                privileges: vec![
                    SequencePrivilege::Usage,
                    SequencePrivilege::Select,
                    SequencePrivilege::Update,
                ],
            },
            Operation::RevokeSequence {
                sequence: t("app", "s"),
                grantee: Grantee::Public,
                privileges: vec![SequencePrivilege::Usage],
                grant_option_only: false,
            },
        ],
        &ctx,
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        out.iter().map(|s| s.sql.as_str()).collect::<Vec<_>>(),
        [
            "GRANT USAGE ON SEQUENCE app.\"Seq\" TO readers",
            "GRANT SELECT ON SEQUENCE app.\"Seq\" TO readers",
            "GRANT UPDATE ON SEQUENCE app.\"Seq\" TO readers",
            "REVOKE USAGE ON SEQUENCE app.s FROM PUBLIC",
        ]
    );
    assert!(out.iter().all(|s| s.lock == LockLevel::None), "{out:?}");
    let empty = Operation::GrantSequence {
        sequence: t("app", "s"),
        grantee: role("readers"),
        privileges: vec![],
    };
    assert_eq!(
        refused(&[empty], &ctx),
        (0, DdlErrorKind::Empty("privileges"))
    );
}

/// MH-27 / P11: only the server's `regprocedure` text for the OID reaches SQL, never the
/// client's `identity`; `ON ROUTINE` so procedures work too (`ON FUNCTION` refuses them); an
/// OID the orchestrator didn't resolve is refused.
#[test]
fn function_ops_render_only_the_resolved_identity() {
    let hostile = "x(int) TO PUBLIC; GRANT ALL ON ALL TABLES IN SCHEMA public TO anon; --";
    let ctx = RenderContext {
        resolved_functions: resolved(&[(16384, "app.f(integer)"), (16390, "app.\"P\"()")]),
        ..pg17("acting")
    };
    let out = sqls(
        &[
            Operation::GrantFunction {
                function: function(16384, hostile),
                grantee: role("readers"),
            },
            Operation::RevokeFunction {
                function: function(16390, "whatever"),
                grantee: Grantee::Public,
                grant_option_only: false,
            },
        ],
        &ctx,
    );
    assert_eq!(
        out,
        [
            "GRANT EXECUTE ON ROUTINE app.f(integer) TO readers",
            "REVOKE EXECUTE ON ROUTINE app.\"P\"() FROM PUBLIC"
        ]
    );
    for s in &out {
        assert!(!s.contains("anon") && !s.contains("whatever"), "{s}");
        parse_one(s);
    }
    let missing = [
        Operation::GrantFunction {
            function: function(16384, "app.f(integer)"),
            grantee: role("readers"),
        },
        Operation::GrantFunction {
            function: function(999, "app.f(integer)"),
            grantee: role("readers"),
        },
    ];
    assert_eq!(
        refused(&missing, &ctx),
        (1, DdlErrorKind::UnresolvedFunction(999))
    );
    let missing = Operation::RevokeFunction {
        function: function(999, "app.f(integer)"),
        grantee: role("readers"),
        grant_option_only: true,
    };
    assert_eq!(
        refused(&[missing], &ctx),
        (0, DdlErrorKind::UnresolvedFunction(999))
    );
}

#[test]
fn default_privileges_render_for_role_in_schema() {
    let ctx = pg17("app_owner");
    let adp = |object: DefaultObject,
               grant: bool,
               grantee: Grantee,
               privileges: Vec<PrivilegeName>| Operation::AlterDefaultPrivileges {
        for_role: "app_owner".into(),
        schema: "app".into(),
        object,
        grant,
        grantee,
        privileges,
    };
    let quoted = Operation::AlterDefaultPrivileges {
        for_role: "app_owner".into(),
        schema: "App".into(),
        object: DefaultObject::Tables,
        grant: true,
        grantee: role("readers"),
        privileges: vec![PrivilegeName::Select, PrivilegeName::Insert],
    };
    let out = sqls(
        &[
            quoted,
            adp(
                DefaultObject::Sequences,
                false,
                Grantee::Public,
                vec![PrivilegeName::Usage],
            ),
            adp(
                DefaultObject::Functions,
                true,
                role("Readers"),
                vec![PrivilegeName::Execute],
            ),
        ],
        &ctx,
    );
    assert_eq!(
        out,
        [
            "ALTER DEFAULT PRIVILEGES FOR ROLE app_owner IN SCHEMA \"App\" GRANT SELECT ON TABLES TO readers",
            "ALTER DEFAULT PRIVILEGES FOR ROLE app_owner IN SCHEMA \"App\" GRANT INSERT ON TABLES TO readers",
            "ALTER DEFAULT PRIVILEGES FOR ROLE app_owner IN SCHEMA app REVOKE USAGE ON SEQUENCES FROM PUBLIC",
            "ALTER DEFAULT PRIVILEGES FOR ROLE app_owner IN SCHEMA app GRANT EXECUTE ON FUNCTIONS TO \"Readers\"",
        ]
    );
    for s in &out {
        assert!(
            matches!(
                parse_one(s),
                pg_query::NodeEnum::AlterDefaultPrivilegesStmt(_)
            ),
            "{s}"
        );
    }
    // A privilege the object kind doesn't have is refused.
    for (object, p) in [
        (DefaultObject::Tables, PrivilegeName::Execute),
        (DefaultObject::Tables, PrivilegeName::Usage),
        (DefaultObject::Sequences, PrivilegeName::Insert),
        (DefaultObject::Functions, PrivilegeName::Select),
    ] {
        let r = render(&[adp(object, true, role("readers"), vec![p])], &ctx);
        assert!(r.is_err(), "{object:?} {p:?}: {r:?}");
    }
    assert_eq!(
        refused(
            &[adp(DefaultObject::Tables, true, role("readers"), vec![])],
            &ctx
        ),
        (0, DdlErrorKind::Empty("privileges"))
    );

    // MH-28: `FOR ROLE` is never a reserved role; the session role (the owner in an owner run)
    // is allowed.
    let owner_run = pg17(OWNER);
    let for_role = |r: &str| Operation::AlterDefaultPrivileges {
        for_role: r.into(),
        schema: "app".into(),
        object: DefaultObject::Tables,
        grant: true,
        grantee: role("readers"),
        privileges: vec![PrivilegeName::Select],
    };
    for r in [
        "anon",
        "authenticated",
        "authenticator",
        "cloud_admin",
        "neon_superuser",
        "owner",
        "pg_read_all_data",
        "cb_m_x",
        "chelabase_admin",
        "CHELABASE_X",
    ] {
        let (i, kind) = refused(&[for_role(r)], &owner_run);
        assert!(
            i == 0 && matches!(&kind, DdlErrorKind::ForRoleNotAllowed(n) if n == r),
            "{r}: {kind:?}"
        );
    }
    assert_eq!(
        sqls(&[for_role(OWNER)], &owner_run),
        [
            "ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA app GRANT SELECT ON TABLES TO readers"
        ]
    );
}

/// MH-28: PUBLIC only in revokes, on every op that takes a grantee (the table ops too, S5).
#[test]
fn public_is_refused_as_a_grantee_except_in_revokes() {
    let ctx = RenderContext {
        resolved_functions: resolved(&[(16384, "app.f(integer)")]),
        ..pg17(OWNER)
    };
    let tg = |grantee: Grantee| TableGrant {
        columns: vec![],
        privileges: vec![Privilege::Select],
        grantee,
        with_grant_option: false,
    };
    let grants = [
        Operation::GrantSchema {
            schema: "app".into(),
            grantee: Grantee::Public,
            privileges: vec![SchemaPrivilege::Usage],
        },
        Operation::GrantSequence {
            sequence: t("app", "s"),
            grantee: Grantee::Public,
            privileges: vec![SequencePrivilege::Usage],
        },
        Operation::GrantFunction {
            function: function(16384, "app.f(integer)"),
            grantee: Grantee::Public,
        },
        Operation::AlterDefaultPrivileges {
            for_role: OWNER.into(),
            schema: "app".into(),
            object: DefaultObject::Tables,
            grant: true,
            grantee: Grantee::Public,
            privileges: vec![PrivilegeName::Select],
        },
        Operation::Grant {
            table: notes(),
            grant: tg(Grantee::Public),
        },
    ];
    for op in &grants {
        let ops = [
            Operation::SetTableComment {
                table: notes(),
                comment: None,
            },
            op.clone(),
        ];
        let (i, kind) = refused(&ops, &ctx);
        assert!(
            i == 1 && matches!(kind, DdlErrorKind::GranteeNotAllowed(_)),
            "{op:?}: {i} {kind:?}"
        );
    }
    let revokes = [
        Operation::RevokeSchema {
            schema: "app".into(),
            grantee: Grantee::Public,
            privileges: vec![SchemaPrivilege::Create],
            grant_option_only: false,
        },
        Operation::RevokeSequence {
            sequence: t("app", "s"),
            grantee: Grantee::Public,
            privileges: vec![SequencePrivilege::Usage],
            grant_option_only: false,
        },
        Operation::RevokeFunction {
            function: function(16384, "app.f(integer)"),
            grantee: Grantee::Public,
            grant_option_only: false,
        },
        Operation::AlterDefaultPrivileges {
            for_role: OWNER.into(),
            schema: "app".into(),
            object: DefaultObject::Tables,
            grant: false,
            grantee: Grantee::Public,
            privileges: vec![PrivilegeName::Select],
        },
        Operation::Revoke {
            table: notes(),
            grant: tg(Grantee::Public),
        },
    ];
    for op in &revokes {
        let out = sqls(std::slice::from_ref(op), &ctx);
        assert!(
            out.len() == 1 && out[0].ends_with(" FROM PUBLIC"),
            "{op:?}: {out:?}"
        );
    }
    // The all-rows policy never targets PUBLIC (MH-26).
    let ctx = RenderContext {
        tables: vec![context_table("public", "notes", serde_json::json!([]))],
        ..pg17(OWNER)
    };
    for r in ["public", "PUBLIC"] {
        let r = render(
            &[Operation::SetAllRows {
                table: notes(),
                role: r.into(),
                enabled: true,
            }],
            &ctx,
        );
        assert!(r.is_err(), "{r:?}");
    }
}

/// MH-28: the owner (the session role), `act_as_reserved` names, `authenticator` are never
/// grantees; `anon` and `authenticated` are.
#[test]
fn reserved_owner_and_authenticator_are_refused_as_grantees() {
    for name in [
        OWNER,
        "authenticator",
        "cloud_admin",
        "neon_superuser",
        "owner",
        "chelabase_admin",
        "chelabase_auth_sync",
        "chelabase_schema_locked",
        "Chelabase_X",
        "pg_read_all_data",
        "PG_monitor",
        "cb_m_abc",
        "CB_M_abc",
    ] {
        assert_eq!(
            privilege_grantee(name, OWNER),
            Err(DdlErrorKind::GranteeNotAllowed(name.to_string())),
            "{name}"
        );
    }
    assert_eq!(
        privilege_grantee("app_owner", "app_owner"),
        Err(DdlErrorKind::GranteeNotAllowed("app_owner".into())),
        "the owner given"
    );
    for name in [
        "anon",
        "authenticated",
        "readers",
        "Readers",
        "pgx",
        "app_owner",
    ] {
        assert_eq!(privilege_grantee(name, OWNER), Ok(name), "{name}");
    }

    let ctx = RenderContext {
        resolved_functions: resolved(&[(16384, "app.f(integer)")]),
        tables: vec![context_table("public", "notes", serde_json::json!([]))],
        ..pg17(OWNER)
    };
    let refused_as = |op: Operation, name: &str| {
        let (_, kind) = refused(std::slice::from_ref(&op), &ctx);
        assert!(
            matches!(&kind, DdlErrorKind::GranteeNotAllowed(n) if n == name),
            "{op:?}: {kind:?}"
        );
    };
    refused_as(
        Operation::GrantSchema {
            schema: "app".into(),
            grantee: role("authenticator"),
            privileges: vec![SchemaPrivilege::Usage],
        },
        "authenticator",
    );
    refused_as(
        Operation::GrantSequence {
            sequence: t("app", "s"),
            grantee: role(OWNER),
            privileges: vec![SequencePrivilege::Usage],
        },
        OWNER,
    );
    refused_as(
        Operation::GrantFunction {
            function: function(16384, "app.f(integer)"),
            grantee: role("cb_m_x"),
        },
        "cb_m_x",
    );
    refused_as(
        Operation::AlterDefaultPrivileges {
            for_role: OWNER.into(),
            schema: "app".into(),
            object: DefaultObject::Tables,
            grant: true,
            grantee: role("chelabase_schema_locked"),
            privileges: vec![PrivilegeName::Select],
        },
        "chelabase_schema_locked",
    );
    refused_as(
        Operation::SetAllRows {
            table: notes(),
            role: "authenticator".into(),
            enabled: true,
        },
        "authenticator",
    );
    refused_as(
        Operation::SetAllRows {
            table: notes(),
            role: OWNER.into(),
            enabled: true,
        },
        OWNER,
    );
    assert_eq!(
        sqls(
            &[Operation::GrantSchema {
                schema: "app".into(),
                grantee: role("anon"),
                privileges: vec![SchemaPrivilege::Usage]
            }],
            &ctx
        ),
        ["GRANT USAGE ON SCHEMA app TO anon"]
    );
}

/// MH-31: `MAINTAIN` by server version, on the table op and on default privileges; an unknown
/// version (0, the default context) is below 17.
#[test]
fn maintain_is_refused_below_pg17() {
    let maintain = |r: &RenderContext| {
        [
            Operation::Grant {
                table: notes(),
                grant: TableGrant {
                    columns: vec![],
                    privileges: vec![Privilege::Maintain],
                    grantee: role("readers"),
                    with_grant_option: false,
                },
            },
            Operation::Revoke {
                table: notes(),
                grant: TableGrant {
                    columns: vec![],
                    privileges: vec![Privilege::Maintain],
                    grantee: role("readers"),
                    with_grant_option: false,
                },
            },
            Operation::AlterDefaultPrivileges {
                for_role: r.session_role.clone(),
                schema: "public".into(),
                object: DefaultObject::Tables,
                grant: true,
                grantee: role("readers"),
                privileges: vec![PrivilegeName::Maintain],
            },
        ]
    };
    for version in [160_004, 0] {
        let ctx = RenderContext {
            server_version_num: version,
            session_role: OWNER.into(),
            ..Default::default()
        };
        for op in maintain(&ctx) {
            let (_, kind) = refused(std::slice::from_ref(&op), &ctx);
            assert!(
                matches!(kind, DdlErrorKind::PrivilegeNotSupported(_)),
                "{version} {op:?}: {kind:?}"
            );
        }
    }
    let ctx = RenderContext {
        server_version_num: 170_000,
        session_role: OWNER.into(),
        ..Default::default()
    };
    assert_eq!(
        sqls(&maintain(&ctx), &ctx),
        [
            "GRANT MAINTAIN ON public.notes TO readers",
            "REVOKE MAINTAIN ON public.notes FROM readers",
            "ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT MAINTAIN ON TABLES TO readers",
        ]
    );
}

/// MH-26 / P9: on, a permissive `FOR ALL TO <role> USING (true) WITH CHECK (true)` policy
/// named by `all_rows_policy_name` (avoiding the table's own names), plus the comment marker;
/// off, a drop of only the policy with that shape and the marker, whatever its name.
#[test]
fn set_all_rows_renders_the_marked_permissive_policy() {
    let ctx = RenderContext {
        tables: vec![context_table("public", "notes", serde_json::json!([]))],
        ..pg17("acting")
    };
    let out = render(
        &[Operation::SetAllRows {
            table: notes(),
            role: "readers".into(),
            enabled: true,
        }],
        &ctx,
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        out.iter().map(|s| s.sql.as_str()).collect::<Vec<_>>(),
        [
            "CREATE POLICY readers_all_rows ON public.notes AS PERMISSIVE FOR ALL TO readers USING (true) WITH CHECK (true)",
            "COMMENT ON POLICY readers_all_rows ON public.notes IS 'chelabase:all_rows'",
        ]
    );
    assert_eq!(ALL_ROWS_MARKER, "chelabase:all_rows");
    assert!(out.iter().all(|s| s.table == Some(notes())), "{out:?}");
    assert!(matches!(
        parse_one(&out[0].sql),
        pg_query::NodeEnum::CreatePolicyStmt(_)
    ));
    assert!(matches!(
        parse_one(&out[1].sql),
        pg_query::NodeEnum::CommentStmt(_)
    ));

    // The user's own `readers_all_rows` is taken: the hub's policy gets the suffixed name.
    let own = policy(
        "readers_all_rows",
        true,
        &["readers"],
        "true",
        Some("true"),
        None,
    );
    let ctx = RenderContext {
        tables: vec![context_table(
            "public",
            "notes",
            serde_json::json!([own.clone()]),
        )],
        ..pg17("acting")
    };
    let name = all_rows_policy_name("readers", &BTreeSet::from(["readers_all_rows".to_string()]));
    assert_ne!(name, "readers_all_rows");
    let out = sqls(
        &[Operation::SetAllRows {
            table: notes(),
            role: "readers".into(),
            enabled: true,
        }],
        &ctx,
    );
    assert_eq!(
        out,
        [
            format!(
                "CREATE POLICY {name} ON public.notes AS PERMISSIVE FOR ALL TO readers USING (true) WITH CHECK (true)"
            ),
            format!("COMMENT ON POLICY {name} ON public.notes IS 'chelabase:all_rows'"),
        ]
    );

    // Off: only the marked policy of the exact shape for that role is dropped.
    let policies = serde_json::json!([
        own,
        policy(
            "readers_all_rows_0a1b2c3d",
            true,
            &["readers"],
            "true",
            Some("true"),
            Some(ALL_ROWS_MARKER)
        ),
        policy(
            "marked_but_narrow",
            true,
            &["readers"],
            "(id > 0)",
            Some("true"),
            Some(ALL_ROWS_MARKER)
        ),
        policy(
            "marked_restrictive",
            false,
            &["readers"],
            "true",
            Some("true"),
            Some(ALL_ROWS_MARKER)
        ),
        policy(
            "marked_two_roles",
            true,
            &["others", "readers"],
            "true",
            Some("true"),
            Some(ALL_ROWS_MARKER)
        ),
        policy(
            "marked_no_check",
            true,
            &["readers"],
            "true",
            None,
            Some(ALL_ROWS_MARKER)
        ),
        policy(
            "others_all_rows",
            true,
            &["others"],
            "true",
            Some("true"),
            Some(ALL_ROWS_MARKER)
        ),
    ]);
    let ctx = RenderContext {
        tables: vec![context_table("public", "notes", policies)],
        ..pg17("acting")
    };
    assert_eq!(
        sqls(
            &[Operation::SetAllRows {
                table: notes(),
                role: "readers".into(),
                enabled: false
            }],
            &ctx
        ),
        ["DROP POLICY readers_all_rows_0a1b2c3d ON public.notes"]
    );
}

/// S4: in an owner run (`session_role` the database owner, `owners` filled), a statement on an
/// object another role owns runs as that owner; a role run (`owners` empty) never sets it.
#[test]
fn run_as_is_set_when_the_object_owner_is_not_the_session_role() {
    use PrivilegeTarget as P;
    let rel = |s: &str, n: &str| P::Relation {
        schema: s.into(),
        name: n.into(),
    };
    let owners: BTreeMap<PrivilegeTarget, String> = [
        (rel("public", "notes"), "app_owner"),
        (rel("public", "mine"), OWNER),
        (
            P::Schema {
                schema: "app".into(),
            },
            "schema_owner",
        ),
        (
            P::Schema {
                schema: "public".into(),
            },
            OWNER,
        ),
        (
            P::Sequence {
                schema: "public".into(),
                name: "s".into(),
            },
            "seq_owner",
        ),
        (P::Function { oid: 16384 }, "fn_owner"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_string()))
    .collect();
    let tg = TableGrant {
        columns: vec![],
        privileges: vec![Privilege::Select],
        grantee: role("readers"),
        with_grant_option: false,
    };
    let ops = [
        Operation::Grant {
            table: notes(),
            grant: tg.clone(),
        },
        Operation::Grant {
            table: t("public", "mine"),
            grant: tg.clone(),
        },
        Operation::GrantSchema {
            schema: "app".into(),
            grantee: role("readers"),
            privileges: vec![SchemaPrivilege::Usage],
        },
        Operation::GrantSchema {
            schema: "public".into(),
            grantee: role("readers"),
            privileges: vec![SchemaPrivilege::Usage],
        },
        Operation::GrantSequence {
            sequence: t("public", "s"),
            grantee: role("readers"),
            privileges: vec![SequencePrivilege::Usage],
        },
        Operation::GrantFunction {
            function: function(16384, "public.f()"),
            grantee: role("readers"),
        },
        Operation::Revoke {
            table: notes(),
            grant: tg,
        },
        Operation::SetAllRows {
            table: notes(),
            role: "readers".into(),
            enabled: true,
        },
    ];
    let base = RenderContext {
        resolved_functions: resolved(&[(16384, "public.f()")]),
        tables: vec![context_table("public", "notes", serde_json::json!([]))],
        ..pg17(OWNER)
    };
    let owner_run = RenderContext {
        owners,
        ..base.clone()
    };
    let out = render(&ops, &owner_run).unwrap_or_else(|e| panic!("{e:?}"));
    let got: Vec<(usize, Option<&str>)> = out
        .iter()
        .map(|s| (s.op_index, s.run_as.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            (0, Some("app_owner")),
            (1, None),
            (2, Some("schema_owner")),
            (3, None),
            (4, Some("seq_owner")),
            (5, Some("fn_owner")),
            (6, Some("app_owner")),
            (7, Some("app_owner")),
            (7, Some("app_owner")),
        ]
    );
    let role_run = RenderContext {
        session_role: "acting".into(),
        ..base
    };
    let out = render(&ops, &role_run).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(out.len(), 9);
    assert!(out.iter().all(|s| s.run_as.is_none()), "{out:?}");
}

/// S4 / T1 item 4: `ALTER DEFAULT PRIVILEGES FOR ROLE r` runs as `r` when `r` isn't the session
/// role in an owner run, so `FOR ROLE` names the current role.
#[test]
fn default_privileges_for_another_role_run_as_that_role() {
    let adp = |for_role: &str| Operation::AlterDefaultPrivileges {
        for_role: for_role.into(),
        schema: "app".into(),
        object: DefaultObject::Tables,
        grant: true,
        grantee: role("readers"),
        privileges: vec![PrivilegeName::Select],
    };
    let owners: BTreeMap<PrivilegeTarget, String> = [
        (
            PrivilegeTarget::DefaultAcl {
                for_role: "app_owner".into(),
                schema: "app".into(),
                object: DefaultObject::Tables,
            },
            "app_owner".to_string(),
        ),
        (
            PrivilegeTarget::Relation {
                schema: "public".into(),
                name: "notes".into(),
            },
            "app_owner".to_string(),
        ),
    ]
    .into_iter()
    .collect();
    let owner_run = RenderContext {
        owners,
        ..pg17(OWNER)
    };
    let out =
        render(&[adp("app_owner"), adp(OWNER)], &owner_run).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        out.iter()
            .map(|s| (s.sql.as_str(), s.run_as.as_deref()))
            .collect::<Vec<_>>(),
        [
            (
                "ALTER DEFAULT PRIVILEGES FOR ROLE app_owner IN SCHEMA app GRANT SELECT ON TABLES TO readers",
                Some("app_owner")
            ),
            (
                "ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA app GRANT SELECT ON TABLES TO readers",
                None
            ),
        ]
    );
    let role_run = pg17("app_owner");
    let out = render(&[adp("app_owner")], &role_run).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(out[0].run_as, None);
}

/// S5: the existing table `Grant`/`Revoke` go through the same allow-list.
#[test]
fn table_grant_and_revoke_check_the_grantee() {
    let ctx = pg17(OWNER);
    let tg = |grantee: Grantee| TableGrant {
        columns: vec!["id".into()],
        privileges: vec![Privilege::Select],
        grantee,
        with_grant_option: false,
    };
    for (grantee, name) in [
        (role(OWNER), OWNER),
        (role("authenticator"), "authenticator"),
        (role("cb_m_x"), "cb_m_x"),
        (role("pg_read_all_data"), "pg_read_all_data"),
        (role("chelabase_admin"), "chelabase_admin"),
    ] {
        let (i, kind) = refused(
            &[Operation::Grant {
                table: notes(),
                grant: tg(grantee),
            }],
            &ctx,
        );
        assert!(
            i == 0 && matches!(&kind, DdlErrorKind::GranteeNotAllowed(n) if n == name),
            "{name}: {kind:?}"
        );
    }
    let (_, kind) = refused(
        &[Operation::Grant {
            table: notes(),
            grant: tg(Grantee::Public),
        }],
        &ctx,
    );
    assert!(
        matches!(kind, DdlErrorKind::GranteeNotAllowed(_)),
        "PUBLIC in a grant: {kind:?}"
    );
    assert_eq!(
        sqls(
            &[Operation::Revoke {
                table: notes(),
                grant: tg(Grantee::Public)
            }],
            &ctx
        ),
        ["REVOKE SELECT (id) ON public.notes FROM PUBLIC"]
    );
    assert_eq!(
        sqls(
            &[Operation::Grant {
                table: notes(),
                grant: tg(role("anon"))
            }],
            &ctx
        ),
        ["GRANT SELECT (id) ON public.notes TO anon"]
    );
    // The empty check still comes first (`tests/ddl_objects.rs`).
    let none = TableGrant {
        columns: vec![],
        privileges: vec![],
        grantee: Grantee::Public,
        with_grant_option: false,
    };
    assert_eq!(
        refused(
            &[Operation::Grant {
                table: notes(),
                grant: none
            }],
            &ctx
        ),
        (0, DdlErrorKind::Empty("privileges"))
    );
}

/// S21 (user): `Grant` and `CreateTable.grants` refuse `with_grant_option: true` (MH-29);
/// `Revoke` keeps it as "revoke the grant option only".
#[test]
fn table_grant_refuses_the_grant_option() {
    let ctx = pg17(OWNER);
    let tg = |with_grant_option: bool| TableGrant {
        columns: vec![],
        privileges: vec![Privilege::Select],
        grantee: role("readers"),
        with_grant_option,
    };
    let ops = [
        Operation::SetTableComment {
            table: notes(),
            comment: None,
        },
        Operation::Grant {
            table: notes(),
            grant: tg(true),
        },
    ];
    let r = render(&ops, &ctx);
    assert!(r.as_ref().is_err_and(|e| e.op_index == 1), "{r:?}");
    let create = CreateTable {
        table: t("public", "fresh"),
        columns: vec![ColumnSpec {
            name: "id".into(),
            data_type: TypeSpec {
                schema: "pg_catalog".into(),
                name: "int4".into(),
                modifiers: vec![],
                array: false,
            },
            not_null: false,
            default: None,
            identity: None,
            generated: None,
            collation: None,
            comment: None,
        }],
        primary_key: vec![],
        constraints: vec![],
        rls: None,
        policies: vec![],
        grants: vec![tg(false), tg(true)],
        comment: None,
        owner: None,
    };
    let r = render(&[ops[0].clone(), Operation::CreateTable(create)], &ctx);
    assert!(r.as_ref().is_err_and(|e| e.op_index == 1), "{r:?}");
    assert_eq!(
        sqls(
            &[Operation::Revoke {
                table: notes(),
                grant: tg(true)
            }],
            &ctx
        ),
        ["REVOKE GRANT OPTION FOR SELECT ON public.notes FROM readers"]
    );
}

// --- Hostile names: each statement is exactly one statement, names survive. ---

/// Names with quotes, `;`, `--` (two `-`), newlines and bidi controls.
fn hostile() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-zA-Z0-9 _\"';\\-\n\u{202E}\u{2066}é中]{1,20}")
        .unwrap()
        .prop_filter("63 bytes", |s| s.len() <= 63)
}

/// A grantee the allow-list takes.
fn hostile_role() -> impl Strategy<Value = String> {
    hostile().prop_filter("an allowed grantee", |s| {
        !chelabase_types::access::act_as_reserved(s)
            && !s.eq_ignore_ascii_case("authenticator")
            && ![
                "public",
                "none",
                "current_user",
                "session_user",
                "current_role",
            ]
            .iter()
            .any(|k| s.eq_ignore_ascii_case(k))
            && s != OWNER
    })
}

fn role_specs(nodes: &[pg_query::protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(pg_query::NodeEnum::RoleSpec(r)) => Some(r.rolename.clone()),
            _ => None,
        })
        .collect()
}

fn grantees(node: &pg_query::NodeEnum) -> Vec<String> {
    match node {
        pg_query::NodeEnum::GrantStmt(g) => role_specs(&g.grantees),
        pg_query::NodeEnum::AlterDefaultPrivilegesStmt(a) => a
            .action
            .as_ref()
            .map(|g| role_specs(&g.grantees))
            .unwrap_or_default(),
        pg_query::NodeEnum::CreatePolicyStmt(p) => role_specs(&p.roles),
        _ => vec![],
    }
}

/// The `FOR ROLE` names of an `ALTER DEFAULT PRIVILEGES`.
fn for_roles(node: &pg_query::NodeEnum) -> Vec<String> {
    let pg_query::NodeEnum::AlterDefaultPrivilegesStmt(a) = node else {
        return vec![];
    };
    a.options
        .iter()
        .filter_map(|o| match o.node.as_ref() {
            Some(pg_query::NodeEnum::DefElem(d)) if d.defname == "roles" => d.arg.as_ref(),
            _ => None,
        })
        .flat_map(|arg| match arg.node.as_ref() {
            Some(pg_query::NodeEnum::List(l)) => role_specs(&l.items),
            _ => vec![],
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn hostile_identifiers_render_one_statement_each(schema in hostile(), table in hostile(), grantee in hostile_role(), for_role in hostile_role()) {
        let ctx = RenderContext {
            resolved_functions: resolved(&[(16384, "app.f(integer)")]),
            tables: vec![context_table(&schema, &table, serde_json::json!([]))],
            ..pg17(OWNER)
        };
        let ops = [
            Operation::GrantSchema { schema: schema.clone(), grantee: role(&grantee), privileges: vec![SchemaPrivilege::Usage, SchemaPrivilege::Create] },
            Operation::RevokeSchema { schema: schema.clone(), grantee: role(&grantee), privileges: vec![SchemaPrivilege::Usage], grant_option_only: true },
            Operation::GrantSequence { sequence: t(&schema, &table), grantee: role(&grantee), privileges: vec![SequencePrivilege::Select] },
            Operation::RevokeSequence { sequence: t(&schema, &table), grantee: role(&grantee), privileges: vec![SequencePrivilege::Update], grant_option_only: false },
            Operation::GrantFunction { function: function(16384, &table), grantee: role(&grantee) },
            Operation::RevokeFunction { function: function(16384, &table), grantee: role(&grantee), grant_option_only: false },
            Operation::AlterDefaultPrivileges {
                for_role: for_role.clone(),
                schema: schema.clone(),
                object: DefaultObject::Tables,
                grant: true,
                grantee: role(&grantee),
                privileges: vec![PrivilegeName::Select],
            },
            Operation::Grant {
                table: t(&schema, &table),
                grant: TableGrant { columns: vec![table.clone()], privileges: vec![Privilege::Select], grantee: role(&grantee), with_grant_option: false },
            },
            Operation::SetAllRows { table: t(&schema, &table), role: grantee.clone(), enabled: true },
        ];
        let out = render(&ops, &ctx).unwrap_or_else(|e| panic!("{e:?}"));
        prop_assert!(out.len() > ops.len(), "{out:?}");
        for s in &out {
            let node = parse_one(&s.sql);
            if !matches!(node, pg_query::NodeEnum::CommentStmt(_)) {
                prop_assert_eq!(grantees(&node), vec![grantee.clone()], "{}", s.sql);
            }
            if matches!(node, pg_query::NodeEnum::AlterDefaultPrivilegesStmt(_)) {
                prop_assert_eq!(for_roles(&node), vec![for_role.clone()], "{}", s.sql);
            }
        }
    }
}

/// Final round (found by T4-5's orchestrator test): before any compute the session role isn't
/// known (empty), so a reserved `FOR ROLE` that may be the database owner's real name
/// (`postgres`) isn't refused yet; what is never a session role still is. With the session
/// role known, a reserved name other than it is refused as before.
#[test]
fn an_unknown_session_role_leaves_reserved_for_roles_to_the_real_render() {
    let for_role = |r: &str| Operation::AlterDefaultPrivileges {
        for_role: r.into(),
        schema: "app".into(),
        object: DefaultObject::Tables,
        grant: true,
        grantee: role("readers"),
        privileges: vec![PrivilegeName::Select],
    };
    let unknown = pg17("");
    for r in ["postgres", "app_owner", "cloud_admin"] {
        assert!(render(&[for_role(r)], &unknown).is_ok(), "{r}");
    }
    for r in ["anon", "authenticated", "public", "current_user"] {
        let (i, kind) = refused(&[for_role(r)], &unknown);
        assert!(
            i == 0 && matches!(kind, DdlErrorKind::ForRoleNotAllowed(_)),
            "{r}: {kind:?}"
        );
    }
    let (_, kind) = refused(&[for_role("postgres")], &pg17("app_owner"));
    assert!(
        matches!(kind, DdlErrorKind::ForRoleNotAllowed(_)),
        "{kind:?}"
    );
}
