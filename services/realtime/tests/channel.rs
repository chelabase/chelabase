//! RT1 T7: join params, canonical channel ids and R4's channel hash (spec §2, W24, P11, R4, S17).

use realtime::channel::{
    JoinTarget, Scope, TableChannel, channel_hash, parse_join, project_channel_id,
};
use serde_json::{Value, json};
use uuid::Uuid;

const DB: &str = "11111111-1111-4111-8111-111111111111";
const BR: &str = "22222222-2222-4222-8222-222222222222";

fn params(schema: &str, table: &str, role: Value) -> Value {
    json!({"database_id": DB, "branch_id": BR, "pg_database": "app", "schema": schema, "table": table, "role": role})
}

fn table(schema: &str, table: &str, role: Value) -> (TableChannel, Option<String>) {
    match parse_join("table", Some(&params(schema, table, role))) {
        Ok(JoinTarget::Table { channel, role }) => (channel, role),
        other => panic!("a table join: {other:?}"),
    }
}

#[test]
fn a_table_join_is_its_structured_params() {
    let (ch, role) = table("public", "items", json!(null));
    assert_eq!(
        ch,
        TableChannel {
            database_id: DB.parse().unwrap(),
            branch_id: BR.parse().unwrap(),
            pg_database: "app".into(),
            schema: "public".into(),
            table: "items".into(),
            bucket: None,
        }
    );
    assert_eq!(role, None, "role null is the owner");
    let (_, role) = table("public", "items", json!("billing"));
    assert_eq!(role.as_deref(), Some("billing"));
}

#[test]
fn the_table_channel_id_is_compact_json_without_the_role() {
    let (ch, _) = table("public", "items", json!("billing"));
    let want = format!(r#"table:["{DB}","{BR}","app","public","items"]"#);
    assert_eq!(ch.id(), want);
    let target = parse_join("table", Some(&params("public", "items", json!(null)))).unwrap();
    assert_eq!(
        target.id(),
        want,
        "the owner and a role watch the same channel id"
    );
}

#[test]
fn names_with_slashes_and_quotes_never_collide() {
    // W24: `/`-joined ids were ambiguous; the JSON encoding isn't.
    let (a, _) = table("a/b", "c", json!(null));
    let (b, _) = table("a", "b/c", json!(null));
    assert_ne!(a.id(), b.id());
    let (q, _) = table("x\"y", "é", json!(null));
    assert_eq!(q.id(), format!(r#"table:["{DB}","{BR}","app","x\"y","é"]"#));
}

#[test]
fn bad_table_params_are_refused() {
    let mut cases: Vec<Value> = vec![];
    for field in ["database_id", "branch_id", "pg_database", "schema", "table"] {
        let mut p = params("public", "items", json!(null));
        p.as_object_mut().unwrap().remove(field);
        cases.push(p);
    }
    for field in ["pg_database", "schema", "table", "role"] {
        let mut p = params("public", "items", json!(null));
        p[field] = json!("");
        cases.push(p);
    }
    let mut p = params("public", "items", json!(null));
    p["database_id"] = json!("not-a-uuid");
    cases.push(p);
    let mut p = params("public", "items", json!(null));
    p["extra"] = json!(1);
    cases.push(p);
    let mut p = params("public", "items", json!(null));
    p["role"] = json!(5);
    cases.push(p);
    cases.push(json!("table"));
    for p in cases {
        assert!(parse_join("table", Some(&p)).is_err(), "{p} is refused");
    }
    assert!(
        parse_join("table", None).is_err(),
        "a table join needs params"
    );
}

#[test]
fn project_channels_are_canonical() {
    let id: Uuid = "00000000-0000-4000-8000-000000000002".parse().unwrap();
    assert_eq!(
        parse_join("project:00000000-0000-4000-8000-000000000002", None),
        Ok(JoinTarget::Project(id))
    );
    let upper = parse_join("project:00000000-0000-4000-8000-00000000000A", None).unwrap();
    assert_eq!(upper.id(), "project:00000000-0000-4000-8000-00000000000a");
    assert_eq!(
        project_channel_id(id),
        "project:00000000-0000-4000-8000-000000000002"
    );
    assert!(parse_join("project:nope", None).is_err());
    assert!(parse_join("project:", None).is_err());
    assert!(
        parse_join(
            "project:00000000-0000-4000-8000-000000000002",
            Some(&json!({"x": 1}))
        )
        .is_err(),
        "a project join takes no params"
    );
}

#[test]
fn app_channels_parse_and_unknown_kinds_are_refused() {
    assert_eq!(
        parse_join("app:room:42", None),
        Ok(JoinTarget::App("app:room:42".into()))
    );
    for ch in ["bogus:x", "tables", "", "table:[]", "TABLE", "project"] {
        assert!(
            parse_join(ch, Some(&params("public", "items", json!(null)))).is_err(),
            "{ch:?} is refused"
        );
    }
}

#[test]
fn the_channel_hash_is_full_sha256_hex_over_scope_and_channel() {
    let b: Uuid = BR.parse().unwrap();
    let scope = Scope::Table {
        branch_id: b,
        db_oid: 16384,
    };
    let h = channel_hash(&scope, "table:x");
    assert_eq!(h.len(), 64);
    assert!(
        h.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{h}"
    );
    assert_eq!(h, channel_hash(&scope, "table:x"), "deterministic");
    // R4: the same channel string under another branch or database is another channel.
    assert_ne!(
        h,
        channel_hash(
            &Scope::Table {
                branch_id: b,
                db_oid: 16385
            },
            "table:x"
        )
    );
    assert_ne!(
        h,
        channel_hash(
            &Scope::Table {
                branch_id: Uuid::from_u128(9),
                db_oid: 16384
            },
            "table:x"
        )
    );
    assert_ne!(h, channel_hash(&scope, "table:y"));
    // A table scope and a project scope never share a hash for one channel string.
    assert_ne!(
        channel_hash(&Scope::Project { project_id: b }, "c"),
        channel_hash(
            &Scope::Table {
                branch_id: b,
                db_oid: 0
            },
            "c"
        )
    );
}

#[test]
fn scopes_round_trip_through_json() {
    for s in [
        Scope::Table {
            branch_id: Uuid::from_u128(1),
            db_oid: 5,
        },
        Scope::Project {
            project_id: Uuid::from_u128(2),
        },
    ] {
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Scope>(&text).unwrap(), s);
    }
}
