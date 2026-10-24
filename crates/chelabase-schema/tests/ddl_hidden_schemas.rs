//! Final round, area B, T4-5: privilege ops are refused, explicitly, on the schemas the hub
//! never shows: Postgres's own (`pg_*`: `pg_catalog`, `pg_toast`, `pg_temp_<n>`, …),
//! `information_schema`, and Chelabase's (`chelabase`, `chelabase_merge`, `chelabase_guard`,
//! any `chelabase…` name). The renderer answers `DdlErrorKind::HiddenSchema(<schema>)` for the
//! op naming one (its index in `op_index`), for every privilege op that names a schema: table
//! grants and revokes, schema, sequence and default privileges, and "all rows". (A function is
//! named by OID only: its schema, and extension schemas, are the orchestrator's check.)

use chelabase_schema::ddl::*;
use chelabase_types::table_api::*;

fn ctx() -> RenderContext {
    RenderContext {
        server_version_num: 170_000,
        session_role: "owner".into(),
        ..Default::default()
    }
}

fn readers() -> Grantee {
    Grantee::Role("readers".into())
}

fn tref(schema: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        name: "t".into(),
    }
}

/// Every privilege op that names a schema, on `schema`.
fn ops_on(schema: &str) -> Vec<Operation> {
    let grant = TableGrant {
        columns: vec![],
        privileges: vec![Privilege::Select],
        grantee: readers(),
        with_grant_option: false,
    };
    vec![
        Operation::Grant {
            table: tref(schema),
            grant: grant.clone(),
        },
        Operation::Revoke {
            table: tref(schema),
            grant,
        },
        Operation::GrantSchema {
            schema: schema.into(),
            grantee: readers(),
            privileges: vec![SchemaPrivilege::Usage],
        },
        Operation::RevokeSchema {
            schema: schema.into(),
            grantee: Grantee::Public,
            privileges: vec![SchemaPrivilege::Create],
            grant_option_only: false,
        },
        Operation::GrantSequence {
            sequence: tref(schema),
            grantee: readers(),
            privileges: vec![SequencePrivilege::Usage],
        },
        Operation::RevokeSequence {
            sequence: tref(schema),
            grantee: readers(),
            privileges: vec![SequencePrivilege::Usage],
            grant_option_only: true,
        },
        Operation::AlterDefaultPrivileges {
            for_role: "owner".into(),
            schema: schema.into(),
            object: DefaultObject::Tables,
            grant: true,
            grantee: readers(),
            privileges: vec![PrivilegeName::Select],
        },
        Operation::SetAllRows {
            table: tref(schema),
            role: "readers".into(),
            enabled: true,
        },
    ]
}

#[test]
fn privilege_ops_on_hidden_schemas_are_refused_by_name() {
    let hidden = [
        "pg_catalog",
        "pg_toast",
        "pg_toast_temp_1",
        "pg_temp_3",
        "pg_anything",
        "information_schema",
        "chelabase",
        "chelabase_merge",
        "chelabase_guard",
        "chelabase_future",
    ];
    for schema in hidden {
        for op in ops_on(schema) {
            // Behind an op that renders, so the index is the hidden one's.
            let ok = Operation::GrantSchema {
                schema: "public".into(),
                grantee: readers(),
                privileges: vec![SchemaPrivilege::Usage],
            };
            let err =
                render(&[ok, op.clone()], &ctx()).expect_err(&format!("{schema}: {op:?} rendered"));
            assert_eq!(err.op_index, 1, "{schema}: {op:?}");
            assert_eq!(
                err.kind,
                DdlErrorKind::HiddenSchema(schema.to_string()),
                "{schema}: {op:?}"
            );
        }
    }
}

/// Names that only look like hidden ones are ordinary schemas.
#[test]
fn ordinary_schemas_that_look_alike_still_render() {
    for schema in ["public", "app", "pgx", "pg", "information", "my_chelabase"] {
        for op in ops_on(schema) {
            let out = render(std::slice::from_ref(&op), &ctx());
            assert!(
                !matches!(&out, Err(e) if matches!(e.kind, DdlErrorKind::HiddenSchema(_))),
                "{schema}: {op:?}: {out:?}"
            );
        }
    }
}
