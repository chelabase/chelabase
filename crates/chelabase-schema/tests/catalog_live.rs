//! Live tests of the catalog reader: [`CATALOG_SQL`] run on scratch databases of the metadata
//! Postgres (`just deps-up`), parsed with [`parse_catalog`].
//!
//! Scratch databases are named `cs_<8 hex unix seconds><4 hex random>` and the run's roles
//! `cs_<same 12 hex>_<name>` (roles are cluster-wide), so a later run can sweep what a dead run
//! left once it is older than [`STALE_AFTER_SECS`].

use chelabase_schema::catalog::{CATALOG_SQL, parse_catalog};
use chelabase_schema::exclusions;
use chelabase_schema::*;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

const FIXTURE: &str = include_str!("fixtures/full.sql");

/// Scratch databases and roles older than this are swept whoever made them.
const STALE_AFTER_SECS: u64 = 3600;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata".into()
    })
}

/// `url()` pointing at database `name`.
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
        .expect("metadata Postgres (just deps-up)");
    tokio::spawn(connection);
    client
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The creation time a `cs_<12 hex>` name carries.
fn created_at(name: &str) -> Option<u64> {
    let hex = name.strip_prefix("cs_")?.get(..8)?;
    u64::from_str_radix(hex, 16).ok()
}

/// One column of every row of a simple query.
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

/// Runs [`CATALOG_SQL`] as one simple query, as psql runs the script: the SELECT's one value.
async fn read_catalog(client: &Client) -> String {
    let rows = texts(client, CATALOG_SQL).await;
    assert_eq!(rows.len(), 1, "one row");
    rows.into_iter().next().unwrap()
}

/// Drops the `cs_` databases and roles of runs older than [`STALE_AFTER_SECS`].
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
        "SELECT rolname FROM pg_catalog.pg_roles WHERE rolname ~* '^(chelabase_)?cs_[0-9a-f]{12}_'",
    )
    .await
    .iter()
    .filter(|n| stale(&n.to_lowercase().trim_start_matches("chelabase_").to_string()))
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
    /// Reserved roles this run had to create (dropped again in `finish`).
    created_reserved: Vec<String>,
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
            created_reserved: Vec::new(),
        }
    }

    /// The run's role prefix: `cs_<12 hex>_`.
    fn prefix(&self) -> String {
        format!("{}_", self.name)
    }

    /// The run's role named `name`.
    fn role(&self, name: &str) -> String {
        format!("{}{name}", self.prefix())
    }

    async fn sql(&self, sql: &str) {
        self.client.batch_execute(sql).await.unwrap();
    }

    /// Creates the reserved roles, and the owner the data API's default privileges are for,
    /// that don't exist yet (NOLOGIN), remembering which.
    async fn reserved_roles(&mut self) {
        let owner = [exclusions::DATA_API_DEFAULT_ACL_ROLE];
        for role in exclusions::RESERVED_ROLES.iter().chain(owner.iter()) {
            let exists = texts(
                &self.meta,
                &format!("SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = '{role}'"),
            )
            .await;
            if exists.is_empty() {
                self.meta
                    .batch_execute(&format!("CREATE ROLE {role} NOLOGIN"))
                    .await
                    .unwrap();
                self.created_reserved.push(role.to_string());
            }
        }
    }

    /// The reserved roles, then [`FIXTURE`] with the run's role prefix.
    async fn fixture(&mut self) {
        self.reserved_roles().await;
        let sql = FIXTURE.replace("@R@", &self.prefix());
        self.sql(&sql).await;
    }

    async fn model(&self) -> SchemaModel {
        parse_catalog(&read_catalog(&self.client).await).unwrap()
    }

    async fn finish(self) {
        let prefix = self.prefix();
        let Scratch {
            meta,
            name,
            client,
            created_reserved,
        } = self;
        drop(client);
        meta.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        let roles = texts(
            &meta,
            &format!(
                "SELECT rolname FROM pg_catalog.pg_roles WHERE starts_with(rolname, '{prefix}')"
            ),
        )
        .await;
        for role in roles {
            meta.batch_execute(&format!("DROP ROLE \"{role}\""))
                .await
                .unwrap();
        }
        for role in created_reserved {
            // Best effort: another database may have started using it meanwhile.
            let _ = meta
                .batch_execute(&format!("DROP ROLE IF EXISTS {role}"))
                .await;
        }
    }
}

fn table<'a>(m: &'a SchemaModel, schema: &str, name: &str) -> &'a Table {
    m.tables
        .iter()
        .find(|t| t.schema == schema && t.name == name)
        .unwrap_or_else(|| panic!("table {schema}.{name}"))
}

fn named_role(oid: Oid, name: &str) -> RoleRef {
    RoleRef::Role {
        oid,
        name: name.to_string(),
    }
}

fn reported<'a>(m: &'a SchemaModel, kind: ReportedKind, name: &str) -> Option<&'a Reported> {
    m.reported.iter().find(|r| r.kind == kind && r.name == name)
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn fixture_reads_every_object_type() {
    let mut s = Scratch::new().await;
    s.fixture().await;
    let json = read_catalog(&s.client).await;
    assert!(!json.contains('\n'), "one line of JSON");
    let m = parse_catalog(&json).unwrap();
    let reader_name = s.role("reader");
    let app_name = s.role("app");
    s.finish().await;

    assert_eq!(m.server_major, 17);
    let billing = m.schemas.iter().find(|s| s.name == "Billing").unwrap();
    assert_eq!(billing.comment.as_deref(), Some("money"));
    // T14 M8: a schema grant.
    assert!(
        billing.grants.iter().any(|g| matches!(&g.grantee,
            RoleRef::Role { name, .. } if *name == reader_name) && g.privilege == "USAGE"),
        "{:?}",
        billing.grants
    );

    // Roles: a NOLOGIN role, and a member of it (and of neon_superuser: owner-level).
    let reader = m.roles.iter().find(|r| r.name == reader_name).unwrap();
    assert!(!reader.login);
    assert_eq!(reader.comment.as_deref(), Some("readers"));
    let app = m.roles.iter().find(|r| r.name == app_name).unwrap();
    assert!(app.login && !app.inherit && app.bypass_rls);
    assert_eq!(app.connection_limit, 5);
    assert_eq!(app.valid_until.as_deref(), Some("2030-01-01 00:00:00+00"));
    assert_eq!(app.other_attributes, ["CREATEDB"]);
    assert!(app.owner_level(), "{:?}", app.member_of);
    let membership = app
        .member_of
        .iter()
        .find(|m| m.role.name == reader_name)
        .unwrap();
    assert_eq!(membership.role.oid, reader.oid);
    assert!(membership.admin && !membership.inherit && membership.set);
    let reader_ref = named_role(reader.oid, &reader_name);
    let app_ref = named_role(app.oid, &app_name);

    // An identity column with its sequence's parameters and grants.
    let order = table(&m, "Billing", "Order");
    assert_eq!(order.comment.as_deref(), Some("orders"));
    assert!(order.rls_enabled && !order.rls_forced && !order.unlogged);
    assert_eq!(order.options, ["fillfactor=70"]);
    let names: Vec<&str> = order.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "id",
            "my col",
            "status",
            "total",
            "doubled",
            "a\"b",
            "customer_id"
        ]
    );
    let id = &order.columns[0];
    assert!(id.not_null);
    let identity = id.identity.as_ref().expect("identity");
    assert_eq!(identity.generation, IdentityGeneration::Always);
    assert_eq!(identity.sequence_name, "Order_id_seq");
    assert_eq!(
        identity.params,
        SequenceParams {
            data_type: "bigint".into(),
            start: 100,
            increment: 5,
            min: 1,
            max: i64::MAX,
            cache: 3,
            cycle: false,
        }
    );
    assert!(
        identity
            .grants
            .iter()
            .any(|g| g.grantee == app_ref && g.privilege == "USAGE")
    );
    let my_col = &order.columns[1];
    assert_eq!(my_col.collation.as_deref(), Some("pg_catalog.\"C\""));
    assert_eq!(my_col.default.as_deref(), Some("'x'::text"));
    assert_eq!(my_col.comment.as_deref(), Some("free text"));
    // A column grant.
    assert_eq!(my_col.grants.len(), 1);
    assert_eq!(my_col.grants[0].grantee, app_ref);
    assert_eq!(my_col.grants[0].privilege, "UPDATE");
    let status = &order.columns[2];
    assert_eq!(status.data_type, "\"Billing\".status");
    let status_enum = m.enums.iter().find(|e| e.name == "status").unwrap();
    assert_eq!(status.type_oid, status_enum.oid);
    let doubled = &order.columns[4];
    assert_eq!(doubled.generated.as_deref(), Some("(total * (2)::numeric)"));
    assert_eq!(doubled.default, None);
    // The identity sequence isn't a standalone sequence.
    assert!(!m.sequences.iter().any(|q| q.name == "Order_id_seq"));

    // A NOT VALID foreign key, a check with a quoted name, the primary key.
    let fk = order
        .constraints
        .iter()
        .find(|c| c.name == "order_customer_fk")
        .unwrap();
    assert_eq!(fk.kind, ConstraintKind::ForeignKey);
    assert!(!fk.validated);
    assert_eq!(fk.columns, [7]);
    let target = fk.references.as_ref().unwrap();
    assert_eq!(
        (target.schema.as_str(), target.table.as_str()),
        ("public", "customer")
    );
    assert_eq!(target.columns, [1]);
    assert!(fk.definition.ends_with("NOT VALID"), "{}", fk.definition);
    let check = order
        .constraints
        .iter()
        .find(|c| c.name == "positive total")
        .unwrap();
    assert_eq!(check.kind, ConstraintKind::Check);
    assert!(check.validated);
    assert!(
        order
            .constraints
            .iter()
            .any(|c| c.kind == ConstraintKind::PrimaryKey)
    );
    // The PK's index isn't an index; the plain one is, with a quoted name and key.
    let ix_names: Vec<&str> = order.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(ix_names, ["Order status"]);
    assert_eq!(order.indexes[0].keys, ["status", "\"a\"\"b\""]);

    // A policy with USING and WITH CHECK for a role, and one for PUBLIC (Postgres stores a list
    // naming PUBLIC as `{0}` alone).
    let policy = &order.policies[0];
    assert_eq!(policy.name, "own rows");
    assert_eq!(policy.command, PolicyCommand::Update);
    assert!(!policy.permissive);
    assert_eq!(policy.roles, std::slice::from_ref(&reader_ref));
    let public = &order.policies[1];
    assert_eq!(public.name, "public read");
    assert_eq!(public.command, PolicyCommand::Select);
    assert!(public.permissive);
    assert_eq!(public.roles, [RoleRef::Public]);
    assert_eq!(public.with_check, None);
    assert_eq!(policy.using.as_deref(), Some("(total > (0)::numeric)"));
    assert_eq!(
        policy.with_check.as_deref(),
        Some("(total < (1000)::numeric)")
    );
    assert!(
        order
            .grants
            .iter()
            .any(|g| g.grantee == reader_ref && g.privilege == "SELECT")
    );
    // Owner privileges come from acldefault when relacl is NULL.
    let customer = table(&m, "public", "customer");
    assert!(!customer.grants.is_empty());

    // A partial unique index with INCLUDE; the constraints' indexes aren't listed.
    let ix_names: Vec<&str> = customer.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(ix_names, ["customer_region_idx"]);
    let ix = &customer.indexes[0];
    assert!(ix.unique && ix.valid);
    assert_eq!(ix.method, "btree");
    assert_eq!(ix.keys, ["lower(region) DESC"]);
    // T14 M8 / minor 18: INCLUDE columns come back as `CREATE INDEX` writes them (quoted).
    assert_eq!(ix.include, ["email", "\"Mixed Case\""]);
    assert_eq!(ix.predicate.as_deref(), Some("region IS NOT NULL"));
    let booking = table(&m, "public", "booking");
    assert_eq!(booking.constraints[0].kind, ConstraintKind::Exclusion);
    assert!(booking.indexes.is_empty());

    assert!(table(&m, "public", "scratch").unlogged);
    assert_eq!(table(&m, "Billing", "my table").columns[0].name, "select");

    // Enum values in sort order, each with its own OID.
    let labels: Vec<&str> = status_enum
        .labels
        .iter()
        .map(|l| l.label.as_str())
        .collect();
    assert_eq!(labels, ["new", "paid", "refunded", "gone"]);
    // T14 M8: each value has an OID of its own.
    let mut oids: Vec<Oid> = status_enum.labels.iter().map(|l| l.oid).collect();
    oids.sort();
    oids.dedup();
    assert_eq!(oids.len(), 4, "{:?}", status_enum.labels);

    // Sequences: OWNED BY and standalone.
    let tickets = m.sequences.iter().find(|q| q.name == "tickets").unwrap();
    assert_eq!(tickets.params.data_type, "integer");
    assert_eq!((tickets.params.start, tickets.params.cycle), (7, true));
    let owned = tickets.owned_by.as_ref().unwrap();
    assert_eq!(
        (owned.table.as_str(), owned.column.as_str()),
        ("customer", "id")
    );
    let counter = m.sequences.iter().find(|q| q.name == "counter").unwrap();
    assert_eq!(counter.owned_by, None);
    assert!(
        counter
            .grants
            .iter()
            .any(|g| g.grantee == reader_ref && g.privilege == "SELECT")
    );

    // Default privileges: per schema and global.
    let per_schema = m
        .default_privileges
        .iter()
        .find(|d| d.schema.as_deref() == Some("Billing") && d.role.name != app_name)
        .unwrap();
    assert_eq!(per_schema.object_type, DefaultPrivilegeObject::Tables);
    assert_eq!(per_schema.grants.len(), 1);
    assert_eq!(per_schema.grants[0].grantee, reader_ref);
    let global = m
        .default_privileges
        .iter()
        .find(|d| d.schema.is_none() && d.object_type == DefaultPrivilegeObject::Sequences)
        .unwrap();
    assert!(global.grants.iter().any(|g| g.grantee == app_ref));

    // Reported objects.
    for (kind, name) in [
        (ReportedKind::View, "big_orders"),
        (ReportedKind::MaterializedView, "order_count"),
        (ReportedKind::Function, "touch"),
        (ReportedKind::Procedure, "noop"),
        (ReportedKind::Trigger, "touch_order"),
        (ReportedKind::PartitionedTable, "events"),
        (ReportedKind::Partition, "events_2026"),
        (ReportedKind::InheritedTable, "child_t"),
        (ReportedKind::ForeignTable, "remote"),
        (ReportedKind::Domain, "positive"),
        (ReportedKind::CompositeType, "pair"),
        (ReportedKind::Rule, "no_delete"),
        (ReportedKind::Cast, "integer AS public.pair"),
        (ReportedKind::Operator, "==="),
        (ReportedKind::Publication, "customers"),
        (ReportedKind::EventTrigger, "cs_on_ddl"),
        (ReportedKind::Extension, "plpgsql"),
    ] {
        let r = reported(&m, kind.clone(), name)
            .unwrap_or_else(|| panic!("{kind:?} {name} in {:#?}", m.reported));
        assert_ne!(r.oid, Oid(0));
    }
    let view = reported(&m, ReportedKind::View, "big_orders").unwrap();
    assert_eq!(view.schema.as_deref(), Some("public"));
    assert!(view.definition.as_deref().unwrap().contains("total > 100"));
    let trigger = reported(&m, ReportedKind::Trigger, "touch_order").unwrap();
    assert_eq!(trigger.parent.as_deref(), Some("Order"));
    let partition = reported(&m, ReportedKind::Partition, "events_2026").unwrap();
    assert_eq!(partition.parent.as_deref(), Some("events"));
    let noop = reported(&m, ReportedKind::Procedure, "noop").unwrap();
    assert_eq!(noop.arguments.as_deref(), Some("IN a integer"));
    // T14 M6: a table that INHERITS is reported with its parent; the parent is a table.
    let child = reported(&m, ReportedKind::InheritedTable, "child_t").unwrap();
    assert_eq!(child.parent.as_deref(), Some("parent_t"));
    assert_eq!(child.definition.as_deref(), Some("INHERITS (public.parent_t)"));
    table(&m, "public", "parent_t");
    // Partitions, inheritance children, views and foreign tables aren't tables.
    for name in [
        "events",
        "events_2026",
        "child_t",
        "big_orders",
        "remote",
        "order_count",
    ] {
        assert!(!m.tables.iter().any(|t| t.name == name), "{name}");
    }
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn two_reads_are_identical() {
    let mut s = Scratch::new().await;
    s.fixture().await;
    let first = read_catalog(&s.client).await;
    // T14 M8: another session, not only another read in the same one.
    let other = connect(&url_for(&s.name)).await;
    let second = read_catalog(&other).await;
    drop(other);
    s.finish().await;
    assert_eq!(first, second);
    assert_eq!(
        parse_catalog(&first).unwrap(),
        parse_catalog(&second).unwrap()
    );
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn excluded_objects_are_absent() {
    let mut s = Scratch::new().await;
    s.fixture().await;
    let m = s.model().await;
    let reader_name = s.role("reader");
    let app_name = s.role("app");
    s.finish().await;

    // Chelabase's schemas and everything in them.
    for schema in [
        "chelabase",
        "chelabase_merge",
        "pg_catalog",
        "information_schema",
    ] {
        assert!(!m.schemas.iter().any(|s| s.name == schema), "{schema}");
        assert!(!m.tables.iter().any(|t| t.schema == schema), "{schema}");
        assert!(
            !m.reported
                .iter()
                .any(|r| r.schema.as_deref() == Some(schema) && r.kind != ReportedKind::Extension),
            "{schema}"
        );
    }
    assert!(!m.schemas.iter().any(|s| s.name.starts_with("pg_")));

    // The users table: only the app's own parts.
    let users = table(&m, "public", "users");
    let columns: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["nickname"]);
    assert!(users.constraints.is_empty(), "{:?}", users.constraints);
    let policies: Vec<&str> = users.policies.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(policies, ["users_extra"]);
    let indexes: Vec<&str> = users.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(indexes, ["users_nickname_idx"]);
    for g in &users.grants {
        if let RoleRef::Role { name, .. } = &g.grantee {
            assert!(
                !exclusions::USERS_GRANTEES.contains(&name.as_str()),
                "{g:?}"
            );
        }
    }
    assert!(
        users
            .grants
            .iter()
            .any(|g| matches!(&g.grantee, RoleRef::Role { name, .. } if *name == reader_name))
    );
    for trigger in exclusions::USERS_GUARD_TRIGGERS {
        assert!(reported(&m, ReportedKind::Trigger, trigger).is_none());
    }
    assert!(reported(&m, ReportedKind::Trigger, "users_touch").is_some());
    // Chelabase's own event trigger (its function is in `chelabase`).
    assert!(reported(&m, ReportedKind::EventTrigger, "cs_protect").is_none());

    // The extension is reported; its members aren't.
    assert!(reported(&m, ReportedKind::Extension, "pgcrypto").is_some());
    for function in ["digest", "crypt", "gen_salt", "pgp_sym_encrypt"] {
        assert!(!m.reported.iter().any(|r| r.name == function), "{function}");
    }

    // Reserved roles aren't roles; memberships in them stay on the member.
    for role in &m.roles {
        assert!(
            !exclusions::RESERVED_ROLES.contains(&role.name.as_str()),
            "{}",
            role.name
        );
        assert!(!role.name.starts_with("pg_"), "{}", role.name);
    }
    let app = m.roles.iter().find(|r| r.name == app_name).unwrap();
    assert!(app.owner_level());

    // The data API's default privileges: anon and authenticated entries are gone, the rest stay.
    let public_tables = m
        .default_privileges
        .iter()
        .find(|d| {
            d.schema.as_deref() == Some("public") && d.object_type == DefaultPrivilegeObject::Tables
        })
        .unwrap();
    let grantees: Vec<&RoleRef> = public_tables.grants.iter().map(|g| &g.grantee).collect();
    assert!(
        grantees
            .iter()
            .all(|g| matches!(g, RoleRef::Role { name, .. } if *name == reader_name)),
        "{grantees:?}"
    );
    assert_eq!(
        public_tables.grants.len(),
        2,
        "SELECT and INSERT for the reader"
    );
    assert!(
        !m.default_privileges
            .iter()
            .any(|d| d.schema.as_deref() == Some("public")
                && d.object_type == DefaultPrivilegeObject::Sequences),
        "a row with only the data API's grantees is dropped"
    );

    // E16: the owner's global REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC is excluded, exactly;
    // the same revoke for another role stays.
    let functions = |role: &str| {
        m.default_privileges.iter().find(|d| {
            d.role.name == role
                && d.schema.is_none()
                && d.object_type == DefaultPrivilegeObject::Functions
        })
    };
    assert!(
        functions(exclusions::DATA_API_DEFAULT_ACL_ROLE).is_none(),
        "{:#?}",
        m.default_privileges
    );
    let app_functions = functions(&app_name).expect("another role's revoke stays");
    assert_eq!(app_functions.grants.len(), 1, "{:?}", app_functions.grants);
    assert_eq!(app_functions.grants[0].privilege, "EXECUTE");
    // T14 M5: another role's per-schema row naming authenticated isn't the data API's.
    let app_billing = m
        .default_privileges
        .iter()
        .find(|d| d.role.name == app_name && d.schema.as_deref() == Some("Billing"))
        .expect("another role's per-schema row stays");
    assert!(
        app_billing
            .grants
            .iter()
            .any(|g| matches!(&g.grantee, RoleRef::Role { name, .. } if name == "authenticated")),
        "{:?}",
        app_billing.grants
    );
}

/// E16 (fix round M7): the owner's global functions row is excluded only when it is exactly the
/// data API's revoke; holding another grant too, it stays.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn the_owners_functions_row_with_another_grant_stays() {
    let mut s = Scratch::new().await;
    s.reserved_roles().await;
    let reader = s.role("reader");
    s.sql(&format!(
        "CREATE ROLE \"{reader}\" NOLOGIN;
         ALTER DEFAULT PRIVILEGES FOR ROLE postgres REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES FOR ROLE postgres GRANT EXECUTE ON FUNCTIONS TO \"{reader}\";"
    ))
    .await;
    let m = s.model().await;
    s.finish().await;
    let row = m
        .default_privileges
        .iter()
        .find(|d| {
            d.role.name == exclusions::DATA_API_DEFAULT_ACL_ROLE
                && d.schema.is_none()
                && d.object_type == DefaultPrivilegeObject::Functions
        })
        .unwrap_or_else(|| panic!("{:#?}", m.default_privileges));
    let grantees: Vec<String> = row
        .grants
        .iter()
        .map(|g| match &g.grantee {
            RoleRef::Public => "PUBLIC".to_string(),
            RoleRef::Role { name, .. } => name.clone(),
        })
        .collect();
    assert!(grantees.contains(&reader), "{grantees:?}");
    assert!(grantees.contains(&"postgres".to_string()), "{grantees:?}");
}

/// T14 M7: every `chelabase_…` role is reserved (any case), listed or not.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn chelabase_roles_are_never_read() {
    let s = Scratch::new().await;
    let lower = format!("chelabase_{}x", s.prefix());
    let upper = format!("CHELABASE_{}Y", s.prefix());
    s.sql(&format!(
        "CREATE ROLE \"{lower}\" NOLOGIN; CREATE ROLE \"{upper}\" NOLOGIN; CREATE ROLE \"{}\" NOLOGIN;",
        s.role("kept")
    ))
    .await;
    let m = s.model().await;
    let kept = s.role("kept");
    s.sql(&format!("DROP ROLE \"{lower}\"; DROP ROLE \"{upper}\";"))
        .await;
    s.finish().await;
    let names: Vec<&str> = m.roles.iter().map(|r| r.name.as_str()).collect();
    assert!(!names.contains(&lower.as_str()), "{names:?}");
    assert!(!names.contains(&upper.as_str()), "{names:?}");
    assert!(names.contains(&kept.as_str()), "{names:?}");
}

/// Members and roles T2: a hidden `cb_m_*` member role, its membership and its table grant
/// are invisible to the catalog read (no role, no ACL entry, no `member_of`).
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn member_roles_and_their_grants_are_invisible() {
    let s = Scratch::new().await;
    let lower = format!("cb_m_test_{}", s.name);
    let upper = format!("CB_M_TEST_{}", s.name);
    let probe = format!("chelabase_probe_{}", s.name);
    let hidden = [lower.clone(), upper.clone(), probe.clone()];
    let group = s.role("group");
    let mut sql = format!(
        "CREATE ROLE \"{group}\" NOLOGIN;
         CREATE ROLE \"{lower}\" LOGIN NOINHERIT; CREATE ROLE \"{upper}\" LOGIN NOINHERIT;
         CREATE ROLE \"{probe}\" NOLOGIN;
         GRANT \"{group}\" TO \"{lower}\" WITH INHERIT FALSE, SET TRUE, ADMIN FALSE;
         CREATE TABLE public.member_probe (id int, note text);
         CREATE SEQUENCE public.member_seq;
         CREATE TABLE public.member_ident (id int GENERATED ALWAYS AS IDENTITY);
         GRANT \"{probe}\" TO \"{group}\";"
    );
    for role in &hidden {
        sql.push_str(&format!(
            "GRANT SELECT ON public.member_probe TO \"{role}\";
             GRANT SELECT (note) ON public.member_probe TO \"{role}\";
             GRANT USAGE ON SEQUENCE public.member_seq TO \"{role}\";
             GRANT USAGE ON SEQUENCE public.member_ident_id_seq TO \"{role}\";
             GRANT USAGE ON SCHEMA public TO \"{role}\";
             ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO \"{role}\";
             ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO \"{role}\";"
        ));
    }
    s.sql(&sql).await;
    let m = s.model().await;
    s.finish().await;
    let meta = connect(&url()).await;
    for role in &hidden {
        // Best effort: the default privileges name the role until the database is gone.
        let _ = meta
            .batch_execute(&format!("DROP ROLE IF EXISTS \"{role}\""))
            .await;
    }
    let names: Vec<&str> = m.roles.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&group.as_str()), "{names:?}");
    let dump = format!("{m:?}");
    for role in &hidden {
        assert!(!dump.contains(role.as_str()), "{role} leaked: {dump}");
    }
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn merge_leftovers_are_reported() {
    let s = Scratch::new().await;
    s.sql(
        "CREATE TABLE public.t (id integer, email text);
         ALTER TABLE public.t ADD CONSTRAINT chelabase_merge_nn_1_16500 CHECK (id IS NOT NULL) NOT VALID;
         ALTER TABLE public.t ADD CONSTRAINT t_id_positive CHECK (id > 0);
         CREATE INDEX chelabase_merge_ix_1 ON public.t (email);
         CREATE INDEX t_email_idx ON public.t (email);",
    )
    .await;
    let m = s.model().await;
    s.finish().await;

    let t = table(&m, "public", "t");
    let constraints: Vec<&str> = t.constraints.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(constraints, ["t_id_positive"]);
    let indexes: Vec<&str> = t.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(indexes, ["t_email_idx"]);
    let check = reported(
        &m,
        ReportedKind::MergeLeftover,
        "chelabase_merge_nn_1_16500",
    )
    .unwrap();
    assert_eq!(check.schema.as_deref(), Some("public"));
    assert_eq!(check.parent.as_deref(), Some("t"));
    let index = reported(&m, ReportedKind::MergeLeftover, "chelabase_merge_ix_1").unwrap();
    assert_eq!(index.parent.as_deref(), Some("t"));
}

/// The target digest (E4) runs on a real catalog, as the admin session runs it (prelude
/// included), and changes with a column's default but not with a role.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn target_digest_runs_and_tracks_ddl() {
    use chelabase_schema::plan::admin_digest_sql;

    let mut s = Scratch::new().await;
    s.fixture().await;
    let digest = |rows: Vec<String>| -> String {
        let digest = rows.last().cloned().unwrap_or_default();
        assert!(
            digest.len() == 32 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "{digest:?}"
        );
        digest
    };
    let sql = admin_digest_sql();
    let first = match s.client.simple_query(&sql).await {
        Ok(messages) => digest(
            messages
                .into_iter()
                .filter_map(|m| match m {
                    SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or_default().to_string()),
                    _ => None,
                })
                .collect(),
        ),
        Err(e) => {
            s.finish().await;
            panic!("the digest doesn't run: {e:?}");
        }
    };
    assert_eq!(digest(texts(&s.client, &sql).await), first, "stable");
    s.sql(&format!("CREATE ROLE {}", s.role("digest_extra"))).await;
    assert_eq!(digest(texts(&s.client, &sql).await), first, "roles are left out");
    s.sql("CREATE TABLE public.digest_probe (id int DEFAULT 1)").await;
    let after = digest(texts(&s.client, &sql).await);
    s.finish().await;
    assert_ne!(after, first, "a new table changes it");
}

/// The planner rebuilds the fixture from nothing: `full.sql` on scratch database A, read; a
/// plan that creates everything, from a fresh database B's own model (it already has
/// `public`), run on B phase by phase (phase 1 in one transaction, each phase 2 step with its
/// script); B read back equals A, modulo OIDs (the drift compare finds nothing). Roles are
/// cluster-wide, so B already has A's and the plan has no phase 0.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn create_everything_round_trip() {
    use chelabase_schema::analysis::{analyze, merge};
    use chelabase_schema::compare::compare;
    use chelabase_schema::plan::{Phase, PlanOptions, plan, step_script};

    let mut a = Scratch::new().await;
    a.fixture().await;
    let source = a.model().await;
    let b = Scratch::new().await;
    let empty = b.model().await;

    let map = IdentityMap::default();
    let analysis = analyze(&empty, &empty, &source, &map, Direction::IntoParent);
    assert!(analysis.conflicts.is_empty(), "{:#?}", analysis.conflicts);
    let merged = merge(&analysis, &[], &empty, &empty, &source, &map, Direction::IntoParent)
        .expect("the merged model");
    let opts = PlanOptions {
        lock_timeout_ms: 5000,
        merge_id: uuid::Uuid::nil(),
    };
    let p = plan(&empty, &merged, &analysis, &[], &opts).expect("the plan");
    assert!(
        p.steps.iter().all(|s| s.phase != Phase::Roles),
        "no phase 0: the roles are cluster-wide"
    );
    let script = |phase: Phase| -> Vec<String> {
        p.steps
            .iter()
            .filter(|s| s.phase == phase)
            .map(|s| s.sql.clone())
            .collect()
    };
    for sql in script(Phase::EnumAdd) {
        b.sql(&sql).await;
    }
    let tx = script(Phase::Tx);
    assert!(!tx.is_empty(), "phase 1 creates the tables");
    b.sql(&format!("BEGIN;\n{};\nCOMMIT;", tx.join(";\n")))
        .await;
    for step in p.steps.iter().filter(|s| s.phase == Phase::Online) {
        b.sql(&step_script(step, &opts)).await;
    }
    let rebuilt = b.model().await;
    b.finish().await;
    a.finish().await;

    let drift = compare(&source, &rebuilt, &[]);
    assert!(
        drift.is_empty(),
        "{} difference(s) after the round trip: {}",
        drift.len(),
        serde_json::to_string_pretty(&drift).unwrap()
    );
    assert_eq!(rebuilt.tables.len(), source.tables.len());
}
