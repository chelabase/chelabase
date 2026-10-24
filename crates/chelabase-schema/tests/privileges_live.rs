//! T4 (members hub step 1): live tests of the privileges read ([`privileges_sql`],
//! [`acl_hashes_sql`]) on scratch databases of the metadata Postgres (the workbench stack),
//! parsed with [`parse_privileges`] and checked with [`verify_privileges`].
//!
//! Scratch databases are named `cs_<8 hex unix seconds><4 hex random>` and the run's roles
//! `cs_<same 12 hex>_<name>`, as in `catalog_live.rs`, so either file's sweep removes what a
//! dead run left.
//!
//! The read runs through the extended protocol (`$1` role, `$2` schema, `$3` after, `$4`
//! limit); the helpers bind each parameter by the type the statement declares (text, int4,
//! int8, json/jsonb) and read the one value as json, jsonb or text, so these tests don't pin
//! those types.

use std::collections::{BTreeMap, BTreeSet};

use chelabase_schema::model::{Grant, RoleRef};
use chelabase_schema::privileges::*;
use chelabase_types::table_api::{
    DefaultObject, ExpectedAcl, FunctionRef, Grantee, Operation, Privilege, PrivilegeName,
    PrivilegeTarget, SchemaPrivilege, SequencePrivilege, TableGrant, TableRef,
};
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

const STALE_AFTER_SECS: u64 = 3600;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    })
}

fn url_for(name: &str) -> String {
    let base = url();
    let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
    let query = tail
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    format!("{head}/{name}{query}")
}

async fn connect(url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .expect("metadata Postgres (is the workbench stack up?)");
    tokio::spawn(connection);
    client
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn created_at(name: &str) -> Option<u64> {
    let hex = name.strip_prefix("cs_")?.get(..8)?;
    u64::from_str_radix(hex, 16).ok()
}

async fn texts(client: &Client, sql: &str) -> Vec<String> {
    client
        .simple_query(sql)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or_default().to_string()),
            _ => None,
        })
        .collect()
}

async fn sweep_stale(meta: &Client) {
    let cutoff = now_secs().saturating_sub(STALE_AFTER_SECS);
    let stale = |name: &String| created_at(name).is_some_and(|t| t < cutoff);
    for db in texts(
        meta,
        "SELECT datname FROM pg_catalog.pg_database WHERE datname ~ '^cs_[0-9a-f]{12}$'",
    )
    .await
    .iter()
    .filter(|n| stale(n))
    {
        let _ = meta
            .batch_execute(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"))
            .await;
    }
    for role in texts(
        meta,
        "SELECT rolname FROM pg_catalog.pg_roles WHERE rolname ~ '^cs_[0-9a-f]{12}_'",
    )
    .await
    .iter()
    .filter(|n| stale(n))
    {
        let _ = meta
            .batch_execute(&format!("DROP ROLE IF EXISTS \"{role}\""))
            .await;
    }
}

/// A scratch database `cs_<12 hex>` on the metadata Postgres, with the run's roles.
struct Scratch {
    meta: Client,
    name: String,
    client: Client,
    /// Roles outside the run's prefix (`cb_m_…`), dropped best effort in `finish`.
    extra_roles: Vec<String>,
}

impl Scratch {
    async fn new() -> Scratch {
        let meta = connect(&url()).await;
        sweep_stale(&meta).await;
        let mut random = [0u8; 2];
        getrandom::fill(&mut random).unwrap();
        let name = format!(
            "cs_{:08x}{:02x}{:02x}",
            now_secs() as u32,
            random[0],
            random[1]
        );
        meta.batch_execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let client = connect(&url_for(&name)).await;
        Scratch {
            meta,
            name,
            client,
            extra_roles: Vec::new(),
        }
    }

    fn role(&self, name: &str) -> String {
        format!("{}_{name}", self.name)
    }

    async fn sql(&self, sql: &str) {
        self.client
            .batch_execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }

    /// Creates NOLOGIN roles `names` (with the run's prefix) and returns their full names.
    async fn roles<const N: usize>(&self, names: [&str; N]) -> [String; N] {
        let full = names.map(|n| self.role(n));
        for r in &full {
            self.sql(&format!("CREATE ROLE \"{r}\" NOLOGIN")).await;
        }
        full
    }

    async fn finish(self) {
        let prefix = format!("{}_", self.name);
        let Scratch {
            meta,
            name,
            client,
            extra_roles,
        } = self;
        drop(client);
        meta.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        for role in texts(
            &meta,
            &format!(
                "SELECT rolname FROM pg_catalog.pg_roles WHERE starts_with(rolname, '{prefix}')"
            ),
        )
        .await
        {
            let _ = meta.batch_execute(&format!("DROP ROLE \"{role}\"")).await;
        }
        for role in extra_roles {
            let _ = meta
                .batch_execute(&format!("DROP ROLE IF EXISTS \"{role}\""))
                .await;
        }
    }
}

/// A parameter of the type the prepared statement declares.
fn param(ty: &Type, value: serde_json::Value) -> Box<dyn ToSql + Sync + Send> {
    use serde_json::Value as V;
    match *ty {
        Type::TEXT | Type::VARCHAR | Type::NAME | Type::BPCHAR | Type::UNKNOWN => {
            Box::new(match value {
                V::Null => None,
                V::String(s) => Some(s),
                other => Some(other.to_string()),
            })
        }
        Type::INT4 => Box::new(value.as_i64().map(|n| n as i32)),
        Type::INT8 => Box::new(value.as_i64()),
        Type::JSON | Type::JSONB => Box::new(value),
        ref other => panic!("unexpected parameter type {other}"),
    }
}

/// Runs `sql` with `values` bound by declared type; the one row's one value as JSON text.
async fn json_query(c: &Client, sql: &str, values: Vec<serde_json::Value>) -> String {
    assert!(
        !sql.trim().is_empty(),
        "the SQL is empty (T4 not implemented yet)"
    );
    let stmt = c
        .prepare(sql)
        .await
        .unwrap_or_else(|e| panic!("doesn't prepare: {e:?}"));
    assert_eq!(stmt.params().len(), values.len(), "parameters");
    let params: Vec<Box<dyn ToSql + Sync + Send>> = stmt
        .params()
        .iter()
        .zip(values)
        .map(|(ty, v)| param(ty, v))
        .collect();
    let refs: Vec<&(dyn ToSql + Sync)> = params
        .iter()
        .map(|p| p.as_ref() as &(dyn ToSql + Sync))
        .collect();
    let rows = c
        .query(&stmt, &refs)
        .await
        .unwrap_or_else(|e| panic!("doesn't run: {e:?}"));
    assert_eq!(rows.len(), 1, "one row");
    match *rows[0].columns()[0].type_() {
        Type::JSON | Type::JSONB => rows[0].get::<_, serde_json::Value>(0).to_string(),
        _ => rows[0].get::<_, String>(0),
    }
}

/// One page of `role`'s privileges.
async fn read_page(
    c: &Client,
    role: &str,
    schema: Option<&str>,
    after: Option<&str>,
    limit: i64,
) -> PrivilegesPage {
    use serde_json::json;
    let json = json_query(
        c,
        privileges_sql(),
        vec![json!(role), json!(schema), json!(after), json!(limit)],
    )
    .await;
    parse_privileges(&json).unwrap_or_else(|e| panic!("{e:?}: {json}"))
}

/// Every page of `role`'s privileges for `schema`, in order.
async fn read_all(c: &Client, role: &str, schema: Option<&str>, limit: i64) -> Vec<PrivilegesPage> {
    let mut pages = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = read_page(c, role, schema, after.as_deref(), limit).await;
        after = page.next_after.clone();
        pages.push(page);
        if after.is_none() {
            return pages;
        }
        assert!(pages.len() < 100, "paging doesn't end");
    }
}

fn objects(pages: &[PrivilegesPage]) -> Vec<&ObjectPrivileges> {
    pages.iter().flat_map(|p| &p.objects).collect()
}

fn object<'a>(pages: &'a [PrivilegesPage], name: &str) -> &'a ObjectPrivileges {
    objects(pages)
        .into_iter()
        .find(|o| o.name == name)
        .unwrap_or_else(|| panic!("object {name}"))
}

/// `(grantor, privilege)` of each grant; every grantee must be `grantee`.
fn entries(grants: &[Grant], grantee: &str) -> BTreeSet<(String, String)> {
    grants
        .iter()
        .map(|g| {
            assert!(
                matches!(&g.grantee, RoleRef::Role { name, .. } if name == grantee),
                "{g:?} isn't {grantee}'s"
            );
            (g.grantor.name.clone(), g.privilege.clone())
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// Every grant list of the pages (schemas, default ACLs, objects, columns).
fn all_grants(pages: &[PrivilegesPage]) -> Vec<&Grant> {
    let mut out: Vec<&Grant> = Vec::new();
    for p in pages {
        out.extend(p.schemas.iter().flat_map(|s| &s.grants));
        out.extend(p.default_privileges.iter().flat_map(|d| &d.grants));
        for o in &p.objects {
            out.extend(&o.grants);
            out.extend(o.columns.iter().flat_map(|c| &c.grants));
        }
    }
    out
}

/// Schema `app` owned by `owner`, with tables `t (id, body)` and `u`, sequence `s`, function
/// `f(integer)` and procedure `p()`, all owned by `owner`.
async fn app_schema(s: &Scratch, owner: &str) {
    s.sql(&format!(
        "CREATE SCHEMA app AUTHORIZATION \"{owner}\";
         SET ROLE \"{owner}\";
         CREATE TABLE app.t (id int, body text);
         CREATE TABLE app.u (id int);
         CREATE SEQUENCE app.s;
         CREATE FUNCTION app.f(integer) RETURNS integer LANGUAGE sql AS 'SELECT 1';
         CREATE PROCEDURE app.p() LANGUAGE sql AS 'SELECT 1';
         RESET ROLE;"
    ))
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn read_lists_one_roles_entries_with_every_grantor() {
    let s = Scratch::new().await;
    let [owner, g2, reader, other] = s.roles(["owner", "g2", "reader", "other"]).await;
    app_schema(&s, &owner).await;
    s.sql(&format!(
        "SET ROLE \"{owner}\";
         GRANT USAGE ON SCHEMA app TO \"{reader}\", \"{g2}\";
         GRANT SELECT ON app.t TO \"{g2}\" WITH GRANT OPTION;
         GRANT SELECT, INSERT ON app.t TO \"{reader}\";
         GRANT UPDATE (body) ON app.t TO \"{reader}\";
         GRANT SELECT ON app.t TO \"{other}\";
         GRANT USAGE ON SEQUENCE app.s TO \"{reader}\";
         GRANT EXECUTE ON FUNCTION app.f(integer) TO \"{reader}\";
         ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT SELECT ON TABLES TO \"{reader}\";
         SET ROLE \"{g2}\";
         GRANT SELECT ON app.t TO \"{reader}\";
         RESET ROLE;"
    ))
    .await;
    let pages = read_all(&s.client, &reader, Some("app"), 200).await;
    let top = read_all(&s.client, &reader, None, 200).await;
    s.finish().await;

    let first = &pages[0];
    assert_eq!(first.role, reader);
    assert!(
        first.server_version_num >= 170_000,
        "{}",
        first.server_version_num
    );
    assert_eq!(first.offered.function, ["execute"]);
    for p in [
        "select",
        "insert",
        "update",
        "delete",
        "truncate",
        "references",
        "trigger",
        "maintain",
    ] {
        assert!(
            first.offered.table.iter().any(|x| x == p),
            "{p}: {:?}",
            first.offered.table
        );
    }
    for p in ["usage", "select", "update"] {
        assert!(
            first.offered.sequence.iter().any(|x| x == p),
            "{p}: {:?}",
            first.offered.sequence
        );
    }
    for p in ["usage", "create"] {
        assert!(
            first.offered.schema.iter().any(|x| x == p),
            "{p}: {:?}",
            first.offered.schema
        );
    }

    let t = object(&pages, "t");
    assert_eq!(
        (t.kind, t.schema.as_str(), t.owner.as_str()),
        (PrivilegeObjectKind::Table, "app", owner.as_str())
    );
    assert_eq!(
        entries(&t.grants, &reader),
        pairs(&[(&owner, "SELECT"), (&g2, "SELECT"), (&owner, "INSERT")]),
        "every grantor"
    );
    let body = t
        .columns
        .iter()
        .find(|c| c.name == "body")
        .expect("column body");
    assert_eq!(entries(&body.grants, &reader), pairs(&[(&owner, "UPDATE")]));
    assert!(
        t.columns
            .iter()
            .filter(|c| c.name != "body")
            .all(|c| c.grants.is_empty()),
        "{:?}",
        t.columns
    );
    let seq = object(&pages, "s");
    assert_eq!(seq.kind, PrivilegeObjectKind::Sequence);
    assert_eq!(entries(&seq.grants, &reader), pairs(&[(&owner, "USAGE")]));
    let f = object(&pages, "f");
    assert_eq!(f.kind, PrivilegeObjectKind::Function);
    assert!(
        f.oid > 0
            && f.identity
                .as_deref()
                .is_some_and(|i| i.contains("f(integer)")),
        "{f:?}"
    );
    assert_eq!(
        entries(&f.grants, &reader),
        pairs(&[(&owner, "EXECUTE")]),
        "PUBLIC's EXECUTE isn't the reader's"
    );
    let p = object(&pages, "p");
    assert_eq!(p.kind, PrivilegeObjectKind::Procedure);
    assert!(p.grants.is_empty(), "{p:?}");
    assert!(
        object(&pages, "u").grants.is_empty(),
        "every object is listed, granted or not"
    );
    for g in all_grants(&pages).into_iter().chain(all_grants(&top)) {
        assert!(
            matches!(&g.grantee, RoleRef::Role { name, .. } if *name == reader),
            "only the reader's entries: {g:?}"
        );
    }

    assert!(
        top.iter().all(|p| p.objects.is_empty()),
        "no schema: no objects"
    );
    let app = top
        .iter()
        .flat_map(|p| &p.schemas)
        .find(|x| x.name == "app")
        .expect("schema app");
    assert_eq!(app.owner, owner);
    assert_eq!(entries(&app.grants, &reader), pairs(&[(&owner, "USAGE")]));
    let d = top
        .iter()
        .flat_map(|p| &p.default_privileges)
        .find(|d| d.schema == "app")
        .expect("app's default ACL");
    assert_eq!(
        (d.for_role.as_str(), d.object),
        (owner.as_str(), DefaultObject::Tables)
    );
    assert_eq!(entries(&d.grants, &reader), pairs(&[(&owner, "SELECT")]));
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn read_pages_per_schema_and_skips_marked_and_member_roles() {
    let mut s = Scratch::new().await;
    let [owner, reader, alice] = s.roles(["owner", "reader", "alice"]).await;
    let member = format!("cb_m_{}", s.name);
    s.extra_roles.push(member.clone());
    app_schema(&s, &owner).await;
    s.sql(&format!(
        "CREATE ROLE \"{member}\" LOGIN NOINHERIT;
         ALTER ROLE \"{alice}\" SET chelabase.managed_role = 'personal';
         SET ROLE \"{owner}\";
         CREATE TABLE app.t1 (); CREATE TABLE app.t2 (); CREATE TABLE app.t3 (); CREATE TABLE app.t4 (); CREATE TABLE app.t5 ();
         GRANT USAGE ON SCHEMA app TO \"{reader}\", \"{alice}\", \"{member}\";
         GRANT SELECT ON ALL TABLES IN SCHEMA app TO \"{reader}\", \"{alice}\", \"{member}\";
         GRANT UPDATE (body) ON app.t TO \"{alice}\", \"{member}\";
         GRANT EXECUTE ON FUNCTION app.f(integer) TO \"{alice}\", \"{member}\";
         ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT SELECT ON TABLES TO \"{alice}\", \"{member}\";
         RESET ROLE;"
    ))
    .await;
    let paged = read_all(&s.client, &reader, Some("app"), 2).await;
    let whole = read_all(&s.client, &reader, Some("app"), 500).await;
    let marked = [
        read_all(&s.client, &alice, Some("app"), 500).await,
        read_all(&s.client, &alice, None, 500).await,
    ];
    let members = [
        read_all(&s.client, &member, Some("app"), 500).await,
        read_all(&s.client, &member, None, 500).await,
    ];
    s.finish().await;

    assert!(paged.len() > 1, "limit 2 pages: {}", paged.len());
    assert!(paged.iter().all(|p| p.objects.len() <= 2), "{paged:?}");
    assert!(paged.last().unwrap().next_after.is_none());
    let names: Vec<&str> = objects(&paged).iter().map(|o| o.name.as_str()).collect();
    let distinct: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(names.len(), distinct.len(), "no object twice: {names:?}");
    let whole_names: BTreeSet<&str> = objects(&whole).iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        distinct, whole_names,
        "the pages add up to the whole schema"
    );
    for t in ["t", "u", "s", "f", "p", "t1", "t2", "t3", "t4", "t5"] {
        assert!(distinct.contains(t), "{t}: {distinct:?}");
    }
    for t in ["t1", "t5"] {
        assert_eq!(
            entries(&object(&paged, t).grants, &reader),
            pairs(&[(&owner, "SELECT")])
        );
    }

    for pages in marked.iter().chain(&members) {
        let grants = all_grants(pages);
        assert!(
            grants.is_empty(),
            "a marked or member role's entries are never read: {grants:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_revoke_by_another_grantor_is_seen_by_verify() {
    let s = Scratch::new().await;
    let [owner, g2, reader] = s.roles(["owner", "g2", "reader"]).await;
    app_schema(&s, &owner).await;
    s.sql(&format!(
        "SET ROLE \"{owner}\";
         GRANT USAGE ON SCHEMA app TO \"{g2}\";
         GRANT SELECT ON app.t TO \"{g2}\" WITH GRANT OPTION;
         SET ROLE \"{g2}\";
         GRANT SELECT ON app.t TO \"{reader}\";
         SET ROLE \"{owner}\";
         GRANT SELECT, INSERT ON app.t TO \"{reader}\";
         REVOKE SELECT, INSERT ON app.t FROM \"{reader}\";
         RESET ROLE;"
    ))
    .await;
    let pages = read_all(&s.client, &reader, Some("app"), 200).await;
    s.finish().await;

    let ops = [Operation::Revoke {
        table: TableRef {
            schema: "app".into(),
            name: "t".into(),
        },
        grant: TableGrant {
            columns: vec![],
            privileges: vec![Privilege::Select, Privilege::Insert],
            grantee: Grantee::Role(reader.clone()),
            with_grant_option: false,
        },
    }];
    assert_eq!(
        verify_privileges(&ops, &pages),
        Err(vec![PrivilegeMismatch {
            object: PrivilegeTarget::Relation {
                schema: "app".into(),
                name: "t".into()
            },
            privilege: "SELECT".into(),
            grantee: reader.clone(),
            still_granted_by: Some(g2.clone()),
            column: None,
            wanted: false,
        }])
    );
}

/// `(grantor, privilege)` of each grant; every grantee must be PUBLIC.
fn public_entries(grants: &[Grant]) -> BTreeSet<(String, String)> {
    grants
        .iter()
        .map(|g| {
            assert!(
                matches!(g.grantee, RoleRef::Public),
                "{g:?} isn't PUBLIC's"
            );
            (g.grantor.name.clone(), g.privilege.clone())
        })
        .collect()
}

/// T4-1 (P29): the read takes `public` as PUBLIC (grantee 0) and lists PUBLIC's entries only;
/// a function revoke from PUBLIC that left another grantor's EXECUTE is a mismatch, and one that
/// landed isn't.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_public_function_revoke_another_grantor_holds_is_seen_by_verify() {
    let s = Scratch::new().await;
    let [owner, g2] = s.roles(["owner", "g2"]).await;
    app_schema(&s, &owner).await;
    s.sql(&format!(
        "SET ROLE \"{owner}\";
         GRANT USAGE ON SCHEMA app TO \"{g2}\";
         GRANT EXECUTE ON FUNCTION app.f(integer) TO \"{g2}\" WITH GRANT OPTION;
         SET ROLE \"{g2}\";
         GRANT EXECUTE ON FUNCTION app.f(integer) TO PUBLIC;
         SET ROLE \"{owner}\";
         REVOKE EXECUTE ON FUNCTION app.f(integer) FROM PUBLIC;
         REVOKE EXECUTE ON PROCEDURE app.p() FROM PUBLIC;
         RESET ROLE;"
    ))
    .await;
    let pages = read_all(&s.client, "public", Some("app"), 200).await;
    s.finish().await;

    assert_eq!(pages[0].role, "public");
    let f = object(&pages, "f");
    assert_eq!(
        public_entries(&f.grants),
        pairs(&[(&g2, "EXECUTE")]),
        "PUBLIC's EXECUTE, granted by g2, is read"
    );
    for g in all_grants(&pages) {
        assert!(
            matches!(g.grantee, RoleRef::Public),
            "only PUBLIC's entries: {g:?}"
        );
    }
    assert!(
        object(&pages, "p").grants.is_empty(),
        "{:?}",
        object(&pages, "p")
    );

    let revoke = |name: &str, oid: u32| Operation::RevokeFunction {
        function: FunctionRef {
            oid,
            identity: format!("app.{name}"),
        },
        grantee: Grantee::Public,
        grant_option_only: false,
    };
    assert_eq!(
        verify_privileges(&[revoke("f(integer)", f.oid)], &pages),
        Err(vec![PrivilegeMismatch {
            object: PrivilegeTarget::Function { oid: f.oid },
            privilege: "EXECUTE".into(),
            grantee: "PUBLIC".into(),
            still_granted_by: Some(g2.clone()),
            column: None,
            wanted: false,
        }])
    );
    assert_eq!(
        verify_privileges(&[revoke("p()", object(&pages, "p").oid)], &pages),
        Ok(()),
        "the procedure's revoke landed"
    );
}

/// T4-1 (P29): the same for a schema, a sequence, a table and a default ACL: PUBLIC's entries
/// are read (schema list and object pages), and each revoke from PUBLIC that didn't land is a
/// mismatch.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn public_revokes_that_did_not_land_are_seen_by_verify() {
    let s = Scratch::new().await;
    let [owner, g2] = s.roles(["owner", "g2"]).await;
    app_schema(&s, &owner).await;
    s.sql(&format!(
        "SET ROLE \"{owner}\";
         GRANT USAGE ON SCHEMA app TO \"{g2}\" WITH GRANT OPTION;
         GRANT USAGE ON SEQUENCE app.s TO \"{g2}\" WITH GRANT OPTION;
         GRANT SELECT ON app.t TO \"{g2}\" WITH GRANT OPTION;
         SET ROLE \"{g2}\";
         GRANT USAGE ON SCHEMA app TO PUBLIC;
         GRANT USAGE ON SEQUENCE app.s TO PUBLIC;
         GRANT SELECT ON app.t TO PUBLIC;
         SET ROLE \"{owner}\";
         REVOKE USAGE ON SCHEMA app FROM PUBLIC;
         REVOKE USAGE ON SEQUENCE app.s FROM PUBLIC;
         REVOKE SELECT ON app.t FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT SELECT ON TABLES TO PUBLIC;
         RESET ROLE;"
    ))
    .await;
    let mut pages = read_all(&s.client, "public", Some("app"), 200).await;
    pages.extend(read_all(&s.client, "public", None, 200).await);
    s.finish().await;

    let app = pages
        .iter()
        .flat_map(|p| &p.schemas)
        .find(|x| x.name == "app")
        .expect("schema app");
    assert_eq!(
        public_entries(&app.grants),
        pairs(&[(&g2, "USAGE")]),
        "PUBLIC's schema USAGE, granted by g2, is read"
    );
    assert_eq!(
        public_entries(&object(&pages, "s").grants),
        pairs(&[(&g2, "USAGE")])
    );
    assert_eq!(
        public_entries(&object(&pages, "t").grants),
        pairs(&[(&g2, "SELECT")])
    );
    let d = pages
        .iter()
        .flat_map(|p| &p.default_privileges)
        .find(|d| d.schema == "app")
        .expect("app's default ACL");
    assert_eq!(public_entries(&d.grants), pairs(&[(&owner, "SELECT")]));
    for g in all_grants(&pages) {
        assert!(
            matches!(g.grantee, RoleRef::Public),
            "only PUBLIC's entries: {g:?}"
        );
    }

    let ops = [
        Operation::RevokeSchema {
            schema: "app".into(),
            grantee: Grantee::Public,
            privileges: vec![SchemaPrivilege::Usage],
            grant_option_only: false,
        },
        Operation::RevokeSequence {
            sequence: TableRef {
                schema: "app".into(),
                name: "s".into(),
            },
            grantee: Grantee::Public,
            privileges: vec![SequencePrivilege::Usage],
            grant_option_only: false,
        },
        Operation::Revoke {
            table: TableRef {
                schema: "app".into(),
                name: "t".into(),
            },
            grant: TableGrant {
                columns: vec![],
                privileges: vec![Privilege::Select],
                grantee: Grantee::Public,
                with_grant_option: false,
            },
        },
        Operation::AlterDefaultPrivileges {
            for_role: owner.clone(),
            schema: "app".into(),
            object: DefaultObject::Tables,
            grant: false,
            grantee: Grantee::Public,
            privileges: vec![PrivilegeName::Select],
        },
    ];
    let mismatch = |object: PrivilegeTarget, privilege: &str, by: &str| PrivilegeMismatch {
        object,
        privilege: privilege.into(),
        grantee: "PUBLIC".into(),
        still_granted_by: Some(by.to_string()),
        column: None,
        wanted: false,
    };
    let mut want = vec![
        mismatch(
            PrivilegeTarget::Schema {
                schema: "app".into(),
            },
            "USAGE",
            &g2,
        ),
        mismatch(
            PrivilegeTarget::Sequence {
                schema: "app".into(),
                name: "s".into(),
            },
            "USAGE",
            &g2,
        ),
        mismatch(
            PrivilegeTarget::Relation {
                schema: "app".into(),
                name: "t".into(),
            },
            "SELECT",
            &g2,
        ),
        mismatch(
            PrivilegeTarget::DefaultAcl {
                for_role: owner.clone(),
                schema: "app".into(),
                object: DefaultObject::Tables,
            },
            "SELECT",
            &owner,
        ),
    ];
    want.sort();
    let got = verify_privileges(&ops, &pages).map_err(|mut v| {
        v.sort();
        v
    });
    assert_eq!(got, Err(want));
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_hub_policy_is_found_by_shape_and_marker_not_name() {
    let s = Scratch::new().await;
    let [owner, reader, other] = s.roles(["owner", "reader", "other"]).await;
    app_schema(&s, &owner).await;
    let marker = ALL_ROWS_MARKER;
    let mut sql = format!("SET ROLE \"{owner}\";");
    for t in ["h1", "h2", "h3", "h4", "h5", "h6"] {
        sql.push_str(&format!(
            "CREATE TABLE app.{t} (id int); ALTER TABLE app.{t} ENABLE ROW LEVEL SECURITY;"
        ));
    }
    let shape = |t: &str, name: &str| {
        format!(
            "CREATE POLICY \"{name}\" ON app.{t} AS PERMISSIVE FOR ALL TO \"{reader}\" USING (true) WITH CHECK (true);"
        )
    };
    let mark =
        |t: &str, name: &str| format!("COMMENT ON POLICY \"{name}\" ON app.{t} IS '{marker}';");
    let own = format!("{reader}_all_rows");
    // h1: the user's own `<role>_all_rows` (no marker) and the hub's, under another name.
    sql.push_str(&shape("h1", &own));
    sql.push_str(&shape("h1", "hub_x"));
    sql.push_str(&mark("h1", "hub_x"));
    // h2: the shape without the marker.
    sql.push_str(&shape("h2", &own));
    // h3: the marker on a narrower policy.
    sql.push_str(&format!("CREATE POLICY m ON app.h3 AS PERMISSIVE FOR ALL TO \"{reader}\" USING (id > 0) WITH CHECK (true);"));
    sql.push_str(&mark("h3", "m"));
    // h4: two roles.
    sql.push_str(&format!("CREATE POLICY m ON app.h4 AS PERMISSIVE FOR ALL TO \"{reader}\", \"{other}\" USING (true) WITH CHECK (true);"));
    sql.push_str(&mark("h4", "m"));
    // h5: restrictive.
    sql.push_str(&format!("CREATE POLICY m ON app.h5 AS RESTRICTIVE FOR ALL TO \"{reader}\" USING (true) WITH CHECK (true);"));
    sql.push_str(&mark("h5", "m"));
    // h6: SELECT only.
    sql.push_str(&format!(
        "CREATE POLICY m ON app.h6 AS PERMISSIVE FOR SELECT TO \"{reader}\" USING (true);"
    ));
    sql.push_str(&mark("h6", "m"));
    sql.push_str("RESET ROLE;");
    s.sql(&sql).await;
    let pages = read_all(&s.client, &reader, Some("app"), 200).await;
    s.finish().await;

    let h1 = object(&pages, "h1");
    assert!(h1.rls_enabled);
    assert_eq!(
        h1.all_rows,
        Some(AllRows {
            policy: "hub_x".into()
        })
    );
    for t in ["h2", "h3", "h4", "h5", "h6", "t"] {
        assert_eq!(object(&pages, t).all_rows, None, "{t}");
    }
    assert!(!object(&pages, "t").rls_enabled);
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn acl_hash_covers_every_grantee_and_column_and_ignores_order() {
    let s = Scratch::new().await;
    let [owner, reader, other] = s.roles(["owner", "reader", "other"]).await;
    app_schema(&s, &owner).await;
    s.sql(&format!(
        "SET ROLE \"{owner}\";
         CREATE TABLE app.n1 (id int); CREATE TABLE app.n2 (id int);
         CREATE TABLE app.ta (id int); CREATE TABLE app.tb (id int);
         GRANT SELECT ON app.ta TO \"{reader}\"; GRANT INSERT ON app.ta TO \"{other}\";
         GRANT INSERT ON app.tb TO \"{other}\"; GRANT SELECT ON app.tb TO \"{reader}\";
         ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT SELECT ON TABLES TO \"{reader}\";
         RESET ROLE;"
    ))
    .await;
    let hash = |pages: &[PrivilegesPage], name: &str| object(pages, name).acl_hash.clone();
    let schema_hash = |pages: &[PrivilegesPage]| {
        pages
            .iter()
            .flat_map(|p| &p.schemas)
            .find(|x| x.name == "app")
            .expect("schema app")
            .acl_hash
            .clone()
    };
    let before = read_all(&s.client, &reader, Some("app"), 200).await;
    let top_before = read_all(&s.client, &reader, None, 200).await;
    s.sql(&format!("SET ROLE \"{owner}\"; GRANT SELECT ON app.u TO \"{other}\"; GRANT USAGE ON SCHEMA app TO \"{other}\"; RESET ROLE;"))
        .await;
    let other_grantee = read_all(&s.client, &reader, Some("app"), 200).await;
    let top_after = read_all(&s.client, &reader, None, 200).await;
    s.sql(&format!(
        "SET ROLE \"{owner}\"; GRANT UPDATE (id) ON app.u TO \"{other}\"; RESET ROLE;"
    ))
    .await;
    let column = read_all(&s.client, &reader, Some("app"), 200).await;

    let f_oid = object(&column, "f").oid;
    let targets = vec![
        PrivilegeTarget::Relation {
            schema: "app".into(),
            name: "u".into(),
        },
        PrivilegeTarget::Relation {
            schema: "app".into(),
            name: "ta".into(),
        },
        PrivilegeTarget::Relation {
            schema: "app".into(),
            name: "missing".into(),
        },
        PrivilegeTarget::Schema {
            schema: "app".into(),
        },
        PrivilegeTarget::Sequence {
            schema: "app".into(),
            name: "s".into(),
        },
        PrivilegeTarget::Function { oid: f_oid },
        PrivilegeTarget::DefaultAcl {
            for_role: owner.clone(),
            schema: "app".into(),
            object: DefaultObject::Tables,
        },
    ];
    let json = json_query(
        &s.client,
        acl_hashes_sql(),
        vec![serde_json::to_value(&targets).unwrap()],
    )
    .await;
    s.finish().await;

    let h = hash(&before, "u");
    assert!(
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "{h}"
    );
    assert_eq!(
        hash(&before, "n1"),
        hash(&before, "n2"),
        "two default ACLs hash the same"
    );
    assert_ne!(
        hash(&other_grantee, "u"),
        h,
        "another grantee's grant changes the hash"
    );
    assert_ne!(
        schema_hash(&top_after),
        schema_hash(&top_before),
        "and the schema's"
    );
    assert_ne!(
        hash(&column, "u"),
        hash(&other_grantee, "u"),
        "a column grant changes the relation's hash"
    );
    assert_eq!(
        hash(&before, "ta"),
        hash(&before, "tb"),
        "the same ACL built in another order"
    );

    let got: BTreeMap<PrivilegeTarget, String> = serde_json::from_str::<Vec<ExpectedAcl>>(&json)
        .unwrap_or_else(|e| panic!("{e}: {json}"))
        .into_iter()
        .map(|e| (e.object, e.acl_hash))
        .collect();
    let default_acl = top_after
        .iter()
        .flat_map(|p| &p.default_privileges)
        .find(|d| d.schema == "app")
        .expect("app's default ACL")
        .acl_hash
        .clone();
    let want: BTreeMap<PrivilegeTarget, String> = [
        (targets[0].clone(), hash(&column, "u")),
        (targets[1].clone(), hash(&column, "ta")),
        (targets[3].clone(), schema_hash(&top_after)),
        (targets[4].clone(), hash(&column, "s")),
        (targets[5].clone(), hash(&column, "f")),
        (targets[6].clone(), default_acl),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        got, want,
        "acl_hashes_sql agrees with the read; a missing object is left out"
    );
}

// T5 (deliberate change, T1's ruling P27, written after this test): `owner_settable` is what
// the run-as sequence (P26) can actually reach: the owner itself, a role the owner can SET to,
// or one it holds ADMIN on, directly or through an INHERIT membership (T1 item 6's extra case:
// the transient grant works there, Postgres picking the inherited admin as grantor). This test
// first asserted the plan's direct-row predicate, which greys the inherited-ADMIN owner.
//
// `t_indirect` stays false although P27's literal `pg_has_role(…, 'MEMBER WITH ADMIN OPTION')`
// says true for it: that check follows memberships of any option, so an ADMIN reached only
// through a non-INHERIT membership counts, while the grant itself picks its grantor over
// INHERIT memberships only and fails (`no possible grantors`, SQLSTATE XX000; probed on the
// stack's PG 17.11). Settable must mean "the run-as sequence works".
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn owner_settable_follows_direct_admin_or_set() {
    let s = Scratch::new().await;
    let [dbo, r_admin, r_set, r_inh, r_none, r_indirect, reader, mid, r_via_mid] = s
        .roles([
            "dbo",
            "r_admin",
            "r_set",
            "r_inh",
            "r_none",
            "r_indirect",
            "reader",
            "mid",
            "r_via_mid",
        ])
        .await;
    let mut sql = format!(
        "GRANT \"{r_admin}\" TO \"{dbo}\" WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;
         GRANT \"{r_set}\" TO \"{dbo}\" WITH ADMIN FALSE, INHERIT FALSE, SET TRUE;
         GRANT \"{r_inh}\" TO \"{dbo}\" WITH ADMIN FALSE, INHERIT TRUE, SET FALSE;
         GRANT \"{r_indirect}\" TO \"{r_admin}\" WITH ADMIN TRUE, INHERIT TRUE, SET TRUE;
         GRANT \"{mid}\" TO \"{dbo}\" WITH ADMIN FALSE, INHERIT TRUE, SET FALSE;
         GRANT \"{r_via_mid}\" TO \"{mid}\" WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;
         CREATE SCHEMA sx AUTHORIZATION \"{dbo}\";
         CREATE SCHEMA sa AUTHORIZATION \"{r_admin}\";
         CREATE SCHEMA sn AUTHORIZATION \"{r_none}\";
         CREATE SCHEMA sm AUTHORIZATION \"{r_via_mid}\";"
    );
    for (t, o) in [
        ("t_dbo", &dbo),
        ("t_admin", &r_admin),
        ("t_set", &r_set),
        ("t_inh", &r_inh),
        ("t_none", &r_none),
        ("t_indirect", &r_indirect),
        ("t_via_mid", &r_via_mid),
    ] {
        sql.push_str(&format!(
            "CREATE TABLE sx.{t} (id int); ALTER TABLE sx.{t} OWNER TO \"{o}\";"
        ));
    }
    s.sql(&sql).await;
    // The read runs on the owner's connection: `current_user` is the database owner.
    s.sql(&format!("SET ROLE \"{dbo}\"")).await;
    let pages = read_all(&s.client, &reader, Some("sx"), 200).await;
    let top = read_all(&s.client, &reader, None, 200).await;
    s.sql("RESET ROLE").await;
    s.finish().await;

    for (t, want) in [
        ("t_via_mid", true),
        ("t_dbo", true),
        ("t_admin", true),
        ("t_set", true),
        ("t_inh", false),
        ("t_none", false),
        ("t_indirect", false),
    ] {
        assert_eq!(object(&pages, t).owner_settable, want, "{t}");
    }
    let schema = |n: &str| {
        top.iter()
            .flat_map(|p| &p.schemas)
            .find(|x| x.name == n)
            .unwrap_or_else(|| panic!("schema {n}"))
            .owner_settable
    };
    assert_eq!(
        (schema("sm"), schema("sx"), schema("sa"), schema("sn")),
        (true, true, true, false)
    );
}

/// Final round, T4-4 (amends S3): `acl_hash` hashes `coalesce(acl, acldefault(kind, owner))`, so
/// a NULL ACL (the default: the owner's privileges, and PUBLIC's EXECUTE on a function) and an
/// empty one (`{}`, every privilege revoked, the owner's too) hash apart, in the read and in
/// `acl_hashes_sql` alike, for a table, a sequence, a function and a schema.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_null_acl_and_an_empty_acl_hash_apart() {
    let s = Scratch::new().await;
    let [owner, reader] = s.roles(["owner", "reader"]).await;
    app_schema(&s, &owner).await;
    let f_oid = object(&read_all(&s.client, &reader, Some("app"), 200).await, "f").oid;
    let targets = vec![
        PrivilegeTarget::Relation {
            schema: "app".into(),
            name: "t".into(),
        },
        PrivilegeTarget::Sequence {
            schema: "app".into(),
            name: "s".into(),
        },
        PrivilegeTarget::Function { oid: f_oid },
        PrivilegeTarget::Schema {
            schema: "app".into(),
        },
    ];
    // (the read's hashes, acl_hashes_sql's), in `targets` order.
    async fn hashes(s: &Scratch, reader: &str, targets: &[PrivilegeTarget]) -> (Vec<String>, Vec<String>) {
        let objects = read_all(&s.client, reader, Some("app"), 200).await;
        let top = read_all(&s.client, reader, None, 200).await;
        let schema = top
            .iter()
            .flat_map(|p| &p.schemas)
            .find(|x| x.name == "app")
            .expect("schema app")
            .acl_hash
            .clone();
        let read = vec![
            object(&objects, "t").acl_hash.clone(),
            object(&objects, "s").acl_hash.clone(),
            object(&objects, "f").acl_hash.clone(),
            schema,
        ];
        let json = json_query(
            &s.client,
            acl_hashes_sql(),
            vec![serde_json::to_value(targets).unwrap()],
        )
        .await;
        let got: BTreeMap<PrivilegeTarget, String> = serde_json::from_str::<Vec<ExpectedAcl>>(&json)
            .unwrap_or_else(|e| panic!("{e}: {json}"))
            .into_iter()
            .map(|e| (e.object, e.acl_hash))
            .collect();
        let by_sql = targets
            .iter()
            .map(|t| got.get(t).unwrap_or_else(|| panic!("{t:?} in {json}")).clone())
            .collect();
        (read, by_sql)
    }
    let acls = "SELECT coalesce((SELECT relacl::text FROM pg_class WHERE oid = 'app.t'::regclass), 'NULL') \
                || ' ' || coalesce((SELECT relacl::text FROM pg_class WHERE oid = 'app.s'::regclass), 'NULL') \
                || ' ' || coalesce((SELECT proacl::text FROM pg_proc WHERE oid = 'app.f(integer)'::regprocedure), 'NULL') \
                || ' ' || coalesce((SELECT nspacl::text FROM pg_namespace WHERE nspname = 'app'), 'NULL')";
    let null_acls = texts(&s.client, acls).await;
    let (null_read, null_sql) = hashes(&s, &reader, &targets).await;
    s.sql(&format!(
        "REVOKE ALL ON app.t FROM \"{owner}\";
         REVOKE ALL ON SEQUENCE app.s FROM \"{owner}\";
         REVOKE ALL ON FUNCTION app.f(integer) FROM PUBLIC, \"{owner}\";
         REVOKE ALL ON SCHEMA app FROM \"{owner}\";"
    ))
    .await;
    let empty_acls = texts(&s.client, acls).await;
    let (empty_read, empty_sql) = hashes(&s, &reader, &targets).await;
    s.finish().await;

    assert_eq!(null_acls, ["NULL NULL NULL NULL"], "the setup: default ACLs");
    assert_eq!(empty_acls, ["{} {} {} {}"], "the setup: empty ACLs");
    assert_eq!(null_read, null_sql, "the read and acl_hashes_sql agree (NULL)");
    assert_eq!(empty_read, empty_sql, "the read and acl_hashes_sql agree ({{}})");
    for (i, t) in targets.iter().enumerate() {
        assert_ne!(null_read[i], empty_read[i], "{t:?}: NULL and {{}} hash alike");
    }
}

/// T4 coder's guard (final round, added to the file): `public` can't be a real role's name
/// (Postgres refuses `CREATE ROLE "public"` and the rename to it as reserved), so the read's
/// `role=public` is PUBLIC only; `PUBLIC` or `Public` (another spelling) names no role and reads
/// as having no entries anywhere, although PUBLIC holds some.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn public_is_reserved_and_read_by_its_exact_name_only() {
    let s = Scratch::new().await;
    let [owner, would_be] = s.roles(["owner", "would_be"]).await;
    app_schema(&s, &owner).await;
    s.sql(&format!(
        "SET ROLE \"{owner}\"; GRANT SELECT ON app.t TO PUBLIC; GRANT USAGE ON SCHEMA app TO PUBLIC; RESET ROLE;"
    ))
    .await;
    let create = s.client.batch_execute("CREATE ROLE \"public\" NOLOGIN").await;
    let rename = s
        .client
        .batch_execute(&format!("ALTER ROLE \"{would_be}\" RENAME TO \"public\""))
        .await;
    let mut exact = read_all(&s.client, "public", Some("app"), 200).await;
    exact.extend(read_all(&s.client, "public", None, 200).await);
    let mut spelled = Vec::new();
    for other in ["PUBLIC", "Public"] {
        spelled.push((other, read_all(&s.client, other, Some("app"), 200).await));
        spelled.push((other, read_all(&s.client, other, None, 200).await));
    }
    s.finish().await;

    for (what, result) in [("CREATE ROLE \"public\"", create), ("the rename to \"public\"", rename)] {
        let err = result.expect_err(what);
        assert_eq!(
            err.code(),
            Some(&tokio_postgres::error::SqlState::RESERVED_NAME),
            "{what}: {err:?}"
        );
    }
    assert_eq!(
        public_entries(&object(&exact, "t").grants),
        pairs(&[(owner.as_str(), "SELECT")]),
        "`public` reads PUBLIC's entries"
    );
    assert!(!all_grants(&exact).is_empty());
    for (other, pages) in &spelled {
        assert!(all_grants(pages).is_empty(), "{other}: {:?}", all_grants(pages));
        assert!(pages.iter().all(|p| p.role == *other), "{other}");
    }
}
