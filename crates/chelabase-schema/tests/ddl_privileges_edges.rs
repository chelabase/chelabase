//! T4 coder's edge cases beyond the RED set (red-team checklist "privilege ops"): the
//! allow-list on `CreateTable.grants`, `SetAllRows` idempotence, `run_as` without an owner
//! entry, `privilege_target`, and `verify_privileges` on op order, grant-option revokes,
//! PUBLIC and table-wide revokes.

use std::collections::BTreeMap;

use chelabase_schema::ddl::*;
use chelabase_schema::model::{Grant, NamedRole, Oid, RoleRef, Table};
use chelabase_schema::privileges::*;
use chelabase_types::table_api::*;

fn t(schema: &str, name: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        name: name.into(),
    }
}

fn pg17(session_role: &str) -> RenderContext {
    RenderContext {
        server_version_num: 170_000,
        session_role: session_role.into(),
        ..Default::default()
    }
}

fn select(grantee: Grantee) -> TableGrant {
    TableGrant {
        columns: vec![],
        privileges: vec![Privilege::Select],
        grantee,
        with_grant_option: false,
    }
}

fn notes_with(policies: serde_json::Value) -> ContextTable {
    let table: Table = serde_json::from_value(serde_json::json!({
        "oid": 1, "schema": "public", "name": "notes", "owner": { "oid": 10, "name": "app_owner" }, "comment": null,
        "rls_enabled": true, "rls_forced": false, "unlogged": false, "options": [],
        "columns": [], "constraints": [], "indexes": [], "policies": policies, "grants": []
    }))
    .unwrap();
    ContextTable {
        table,
        total_bytes: 0,
    }
}

/// S5 / MH-28: `CreateTable.grants` goes through the same allow-list as `Grant` (PUBLIC, the
/// owner, `authenticator`, reserved names), so the create path is no bypass.
#[test]
fn create_table_grants_check_the_grantee() {
    let ctx = pg17("postgres");
    let create = |grantee: Grantee| {
        Operation::CreateTable(CreateTable {
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
            grants: vec![select(grantee)],
            comment: None,
            owner: None,
        })
    };
    for grantee in [
        Grantee::Public,
        Grantee::Role("postgres".into()),
        Grantee::Role("authenticator".into()),
        Grantee::Role("cb_m_x".into()),
    ] {
        let e = render(&[create(grantee.clone())], &ctx).unwrap_err();
        assert!(
            matches!(e.kind, DdlErrorKind::GranteeNotAllowed(_)),
            "{grantee:?}: {e:?}"
        );
    }
    // A role spelled `PUBLIC` would reach Postgres as PUBLIC: refused as a name.
    assert!(render(&[create(Grantee::Role("PUBLIC".into()))], &ctx).is_err());
    assert!(render(&[create(Grantee::Role("authenticated".into()))], &ctx).is_ok());
}

/// MH-26: switching all rows on twice adds no second policy; off on a table the context
/// doesn't have (nothing known to drop) renders nothing.
#[test]
fn set_all_rows_is_idempotent() {
    let marked = serde_json::json!([{
        "oid": 5, "name": "whatever", "command": "all", "permissive": true,
        "roles": [{ "role": { "oid": 30, "name": "readers" } }],
        "using": "true", "with_check": "true", "comment": ALL_ROWS_MARKER
    }]);
    let ctx = RenderContext {
        tables: vec![notes_with(marked)],
        ..pg17("acting")
    };
    let on = Operation::SetAllRows {
        table: t("public", "notes"),
        role: "readers".into(),
        enabled: true,
    };
    assert_eq!(render(&[on], &ctx).unwrap(), vec![]);
    let off = Operation::SetAllRows {
        table: t("public", "other"),
        role: "readers".into(),
        enabled: false,
    };
    assert_eq!(render(&[off], &ctx).unwrap(), vec![]);
    assert_eq!(
        context_tables(&[Operation::SetAllRows {
            table: t("public", "notes"),
            role: "readers".into(),
            enabled: false,
        }]),
        [t("public", "notes")],
        "the orchestrator reads the table's policies"
    );
}

/// S4: an owner run whose `owners` lacks the object leaves `run_as` unset (the statement runs
/// as the session role and Postgres decides); `privilege_target` names every op's ACL.
#[test]
fn run_as_needs_the_objects_owner_and_targets_are_per_op() {
    let owners: BTreeMap<PrivilegeTarget, String> = [(
        PrivilegeTarget::Schema {
            schema: "app".into(),
        },
        "other".to_string(),
    )]
    .into_iter()
    .collect();
    let ctx = RenderContext {
        owners,
        ..pg17("postgres")
    };
    let out = render(
        &[Operation::Grant {
            table: t("public", "unknown"),
            grant: select(Grantee::Role("readers".into())),
        }],
        &ctx,
    )
    .unwrap();
    assert_eq!(out[0].run_as, None);

    let f = FunctionRef {
        oid: 7,
        identity: "f()".into(),
    };
    let cases = [
        (
            Operation::Revoke {
                table: t("a", "b"),
                grant: select(Grantee::Public),
            },
            Some(PrivilegeTarget::Relation {
                schema: "a".into(),
                name: "b".into(),
            }),
        ),
        (
            Operation::RevokeSequence {
                sequence: t("a", "s"),
                grantee: Grantee::Public,
                privileges: vec![SequencePrivilege::Usage],
                grant_option_only: false,
            },
            Some(PrivilegeTarget::Sequence {
                schema: "a".into(),
                name: "s".into(),
            }),
        ),
        (
            Operation::GrantFunction {
                function: f,
                grantee: Grantee::Public,
            },
            Some(PrivilegeTarget::Function { oid: 7 }),
        ),
        (
            Operation::AlterDefaultPrivileges {
                for_role: "o".into(),
                schema: "a".into(),
                object: DefaultObject::Functions,
                grant: true,
                grantee: Grantee::Public,
                privileges: vec![PrivilegeName::Execute],
            },
            Some(PrivilegeTarget::DefaultAcl {
                for_role: "o".into(),
                schema: "a".into(),
                object: DefaultObject::Functions,
            }),
        ),
        (
            Operation::SetTableComment {
                table: t("a", "b"),
                comment: None,
            },
            None,
        ),
    ];
    for (op, want) in cases {
        assert_eq!(privilege_target(&op), want, "{op:?}");
    }
}

/// P11: an empty resolution is no resolution.
#[test]
fn an_empty_resolved_identity_is_refused() {
    let ctx = RenderContext {
        resolved_functions: [(7, String::new())].into_iter().collect(),
        ..pg17("acting")
    };
    let e = render(
        &[Operation::GrantFunction {
            function: FunctionRef {
                oid: 7,
                identity: "f()".into(),
            },
            grantee: Grantee::Role("readers".into()),
        }],
        &ctx,
    )
    .unwrap_err();
    assert_eq!(e.kind, DdlErrorKind::UnresolvedFunction(7));
}

fn grant(grantor: &str, grantee: RoleRef, privilege: &str, grantable: bool) -> Grant {
    Grant {
        grantor: NamedRole {
            oid: Oid(10),
            name: grantor.into(),
        },
        grantee,
        privilege: privilege.into(),
        grantable,
    }
}

fn role(name: &str) -> RoleRef {
    RoleRef::Role {
        oid: Oid(20),
        name: name.into(),
    }
}

fn page_with(objects: Vec<ObjectPrivileges>) -> PrivilegesPage {
    PrivilegesPage {
        role: "readers".into(),
        server_version_num: 170_000,
        offered: Offered {
            table: vec![],
            sequence: vec![],
            schema: vec![],
            function: vec![],
        },
        schemas: vec![],
        default_privileges: vec![],
        objects,
        next_after: None,
    }
}

fn table_object(grants: Vec<Grant>, columns: Vec<ColumnPrivileges>) -> ObjectPrivileges {
    ObjectPrivileges {
        kind: PrivilegeObjectKind::Table,
        schema: "app".into(),
        name: "t".into(),
        oid: 1,
        identity: None,
        owner: "owner".into(),
        owner_settable: true,
        rls_enabled: false,
        grants,
        columns,
        all_rows: None,
        acl_hash: String::new(),
    }
}

/// MH-24: a later op on the same entry decides what the request wants (revoke then grant ends
/// granted); a grant-option revoke flags only a grantable entry left; PUBLIC is named "PUBLIC".
#[test]
fn verify_follows_op_order_grant_options_and_public() {
    let readers = || Grantee::Role("readers".into());
    let rel = PrivilegeTarget::Relation {
        schema: "app".into(),
        name: "t".into(),
    };
    let revoke_then_grant = [
        Operation::Revoke {
            table: t("app", "t"),
            grant: select(readers()),
        },
        Operation::Grant {
            table: t("app", "t"),
            grant: select(readers()),
        },
    ];
    let held = page_with(vec![table_object(
        vec![grant("owner", role("readers"), "SELECT", false)],
        vec![],
    )]);
    assert_eq!(
        verify_privileges(&revoke_then_grant, std::slice::from_ref(&held)),
        Ok(())
    );

    let option_only = [Operation::Revoke {
        table: t("app", "t"),
        grant: TableGrant {
            with_grant_option: true,
            ..select(readers())
        },
    }];
    assert_eq!(
        verify_privileges(&option_only, std::slice::from_ref(&held)),
        Ok(())
    );
    let grantable = page_with(vec![table_object(
        vec![grant("owner", role("readers"), "SELECT", true)],
        vec![],
    )]);
    let e = verify_privileges(&option_only, &[grantable]).unwrap_err();
    assert_eq!(
        (
            e.len(),
            &e[0].object,
            e[0].still_granted_by.as_deref(),
            e[0].wanted
        ),
        (1, &rel, Some("owner"), false)
    );

    let public = [Operation::Revoke {
        table: t("app", "t"),
        grant: select(Grantee::Public),
    }];
    let page = page_with(vec![table_object(
        vec![grant("owner", RoleRef::Public, "SELECT", false)],
        vec![],
    )]);
    let e = verify_privileges(&public, &[page]).unwrap_err();
    assert_eq!(e[0].grantee, "PUBLIC");
    assert_eq!(
        verify_privileges(&public, &[held]),
        Ok(()),
        "the reader's entry isn't PUBLIC's"
    );
}

/// R-T4a: a column privilege's mismatch names its column (a column grant that didn't land, a
/// column revoke another grantor still holds, a table-wide revoke that left a column entry);
/// `column` is `null` for the object's own entries.
#[test]
fn verify_names_the_column_of_a_column_privilege() {
    let readers = || Grantee::Role("readers".into());
    let ops = [
        Operation::Grant {
            table: t("app", "t"),
            grant: TableGrant {
                columns: vec!["body".into()],
                privileges: vec![Privilege::Update],
                grantee: readers(),
                with_grant_option: false,
            },
        },
        Operation::Revoke {
            table: t("app", "t"),
            grant: TableGrant {
                columns: vec!["note".into()],
                privileges: vec![Privilege::Insert],
                grantee: readers(),
                with_grant_option: false,
            },
        },
        Operation::Revoke {
            table: t("app", "t"),
            grant: select(readers()),
        },
    ];
    let column = |name: &str, grants: Vec<Grant>| ColumnPrivileges {
        name: name.into(),
        grants,
    };
    let page = page_with(vec![table_object(
        vec![],
        vec![
            column("body", vec![]),
            column(
                "note",
                vec![grant("delegate", role("readers"), "INSERT", false)],
            ),
            column("id", vec![grant("owner", role("readers"), "SELECT", false)]),
        ],
    )]);
    let rel = PrivilegeTarget::Relation {
        schema: "app".into(),
        name: "t".into(),
    };
    let m = |column: &str, privilege: &str, by: Option<&str>| PrivilegeMismatch {
        object: rel.clone(),
        privilege: privilege.into(),
        grantee: "readers".into(),
        still_granted_by: by.map(Into::into),
        column: Some(column.into()),
        wanted: by.is_none(),
    };
    let mut got = verify_privileges(&ops, &[page]).unwrap_err();
    got.sort();
    let mut want = vec![
        m("body", "UPDATE", None),
        m("note", "INSERT", Some("delegate")),
        m("id", "SELECT", Some("owner")),
    ];
    want.sort();
    assert_eq!(got, want);

    let wire = serde_json::to_value(&want[0]).unwrap();
    assert!(wire["column"].is_string(), "{wire}");
    let object_level = PrivilegeMismatch {
        column: None,
        ..want[0].clone()
    };
    assert!(serde_json::to_value(&object_level).unwrap()["column"].is_null());
}

/// Final round, T4-3, as an invariant over every short request: however a request toggles one
/// role's "all rows" on a table, the statements it renders leave the table with the hub's
/// policy exactly when its last `SetAllRows` turns it on, and never more than one. Exhaustive
/// over the policy already there or not and every sequence of 1 to 4 toggles; the statements
/// are replayed on the set of the hub's policies (`CREATE POLICY` adds a name that isn't there,
/// `DROP POLICY` removes one that is).
#[test]
fn set_all_rows_ends_as_the_last_toggle_says() {
    let marked = serde_json::json!([{
        "oid": 5, "name": "whatever", "command": "all", "permissive": true,
        "roles": [{ "role": { "oid": 30, "name": "readers" } }],
        "using": "true", "with_check": "true", "comment": ALL_ROWS_MARKER
    }]);
    let toggle = |enabled: bool| Operation::SetAllRows {
        table: t("public", "notes"),
        role: "readers".into(),
        enabled,
    };
    let name_after = |sql: &str, head: &str| -> String {
        let rest = sql.strip_prefix(head).unwrap_or_else(|| panic!("{sql}"));
        rest.split(" ON ").next().unwrap().trim_matches('"').to_string()
    };
    for already in [false, true] {
        let policies = if already { marked.clone() } else { serde_json::json!([]) };
        let ctx = RenderContext {
            tables: vec![notes_with(policies)],
            ..pg17("acting")
        };
        for len in 1..=4u32 {
            for bits in 0..(1u32 << len) {
                let toggles: Vec<bool> = (0..len).map(|i| bits & (1 << i) != 0).collect();
                let ops: Vec<Operation> = toggles.iter().map(|e| toggle(*e)).collect();
                let mut present: std::collections::BTreeSet<String> =
                    if already { ["whatever".to_string()].into() } else { Default::default() };
                let rendered = render(&ops, &ctx).unwrap_or_else(|e| panic!("{toggles:?}: {e:?}"));
                for s in &rendered {
                    if s.sql.starts_with("CREATE POLICY ") {
                        let n = name_after(&s.sql, "CREATE POLICY ");
                        assert!(present.insert(n.clone()), "{toggles:?}: {n} exists: {rendered:#?}");
                    } else if s.sql.starts_with("DROP POLICY ") {
                        let n = name_after(&s.sql, "DROP POLICY ");
                        assert!(present.remove(&n), "{toggles:?}: no {n} to drop: {rendered:#?}");
                    } else {
                        assert!(s.sql.starts_with("COMMENT ON POLICY "), "{toggles:?}: {}", s.sql);
                    }
                }
                let last = *toggles.last().unwrap();
                assert_eq!(
                    !present.is_empty(),
                    last,
                    "already={already} {toggles:?} ends with {present:?}: {rendered:#?}"
                );
                assert!(present.len() <= 1, "{toggles:?}: {present:?}");
            }
        }
    }
}
