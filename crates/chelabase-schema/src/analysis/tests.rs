//! Three-way analysis and merge tests, on small Rust-built models.

use super::*;
use crate::identity::{Direction, IdentityMap, IdentityPair, ObjAt};
use crate::model::*;

const INTO: Direction = Direction::IntoParent;

fn nr(oid: u32, name: &str) -> NamedRole {
    NamedRole {
        oid: Oid(oid),
        name: name.to_string(),
    }
}

fn pg() -> NamedRole {
    nr(10, "postgres")
}

fn rr(oid: u32, name: &str) -> RoleRef {
    RoleRef::Role {
        oid: Oid(oid),
        name: name.to_string(),
    }
}

fn grant(grantee: RoleRef, privilege: &str) -> Grant {
    Grant {
        grantor: pg(),
        grantee,
        privilege: privilege.to_string(),
        grantable: false,
    }
}

fn col(attnum: i16, name: &str, ty: &str) -> Column {
    Column {
        attnum,
        name: name.to_string(),
        data_type: ty.to_string(),
        type_oid: Oid(25),
        collation: None,
        default: None,
        not_null: false,
        identity: None,
        generated: None,
        comment: None,
        grants: vec![],
    }
}

fn table(oid: u32, schema: &str, name: &str, columns: Vec<Column>) -> Table {
    Table {
        oid: Oid(oid),
        schema: schema.to_string(),
        name: name.to_string(),
        owner: pg(),
        comment: None,
        rls_enabled: false,
        rls_forced: false,
        unlogged: false,
        options: vec![],
        columns,
        constraints: vec![],
        indexes: vec![],
        policies: vec![],
        grants: vec![],
    }
}

fn role(oid: u32, name: &str) -> Role {
    Role {
        oid: Oid(oid),
        name: name.to_string(),
        login: true,
        inherit: true,
        connection_limit: -1,
        valid_until: None,
        bypass_rls: false,
        member_of: vec![],
        other_attributes: vec![],
        comment: None,
    }
}

fn schema(oid: u32, name: &str) -> Schema {
    Schema {
        oid: Oid(oid),
        name: name.to_string(),
        owner: pg(),
        comment: None,
        grants: vec![],
    }
}

fn fk(
    oid: u32,
    name: &str,
    cols: Vec<i16>,
    target: (u32, &str),
    tcols: Vec<i16>,
    def: &str,
) -> Constraint {
    Constraint {
        oid: Oid(oid),
        name: name.to_string(),
        kind: ConstraintKind::ForeignKey,
        definition: def.to_string(),
        columns: cols,
        references: Some(ForeignKeyTarget {
            table_oid: Oid(target.0),
            schema: "public".to_string(),
            table: target.1.to_string(),
            columns: tcols,
        }),
        deferrable: false,
        initially_deferred: false,
        validated: true,
    }
}

fn policy(oid: u32, name: &str, roles: Vec<RoleRef>, using: &str) -> Policy {
    Policy {
        oid: Oid(oid),
        name: name.to_string(),
        command: PolicyCommand::Select,
        permissive: true,
        roles,
        using: Some(using.to_string()),
        with_check: None,
        comment: None,
    }
}

fn index(oid: u32, name: &str, key: &str) -> Index {
    Index {
        oid: Oid(oid),
        name: name.to_string(),
        method: "btree".to_string(),
        unique: false,
        nulls_not_distinct: false,
        keys: vec![key.to_string()],
        include: vec![],
        predicate: None,
        options: vec![],
        valid: true,
        comment: None,
    }
}

/// The base:
/// - schemas `public` (2200) and `app` (2300);
/// - roles `postgres` (10), `app` (16400), `auditor` (16401);
/// - `public.users` (16420): `id bigint` (1), `email character varying(50)` (2), `code text`
///   (3, NOT NULL), unique `users_code_key` (16425), SELECT granted to `app`;
/// - `public.orders` (16430): `id bigint` (1, NOT NULL), `user_id integer` (2), FK
///   `orders_user_fk` (16440) to `users(id)`, policy `own` (16450) to `app`;
/// - enum `public.status` (16460: `new` 16461, `done` 16462);
/// - sequence `public.tickets` (16470).
fn base() -> SchemaModel {
    let mut m = SchemaModel::empty(17);
    m.schemas = vec![schema(2200, "public"), schema(2300, "app")];
    m.roles = vec![
        role(10, "postgres"),
        role(16400, "app"),
        role(16401, "auditor"),
    ];
    let mut users = table(
        16420,
        "public",
        "users",
        vec![
            col(1, "id", "bigint"),
            col(2, "email", "character varying(50)"),
            Column {
                not_null: true,
                ..col(3, "code", "text")
            },
        ],
    );
    users.constraints = vec![Constraint {
        oid: Oid(16425),
        name: "users_code_key".to_string(),
        kind: ConstraintKind::Unique,
        definition: "UNIQUE (code)".to_string(),
        columns: vec![3],
        references: None,
        deferrable: false,
        initially_deferred: false,
        validated: true,
    }];
    users.grants = vec![grant(rr(16400, "app"), "SELECT")];
    let mut orders = table(
        16430,
        "public",
        "orders",
        vec![
            Column {
                not_null: true,
                ..col(1, "id", "bigint")
            },
            col(2, "user_id", "integer"),
        ],
    );
    orders.constraints = vec![fk(
        16440,
        "orders_user_fk",
        vec![2],
        (16420, "users"),
        vec![1],
        "FOREIGN KEY (user_id) REFERENCES users(id)",
    )];
    orders.policies = vec![policy(16450, "own", vec![rr(16400, "app")], "true")];
    orders.rls_enabled = true;
    m.tables = vec![users, orders];
    m.enums = vec![EnumType {
        oid: Oid(16460),
        schema: "public".to_string(),
        name: "status".to_string(),
        owner: pg(),
        comment: None,
        labels: vec![
            EnumLabel {
                oid: Oid(16461),
                label: "new".to_string(),
            },
            EnumLabel {
                oid: Oid(16462),
                label: "done".to_string(),
            },
        ],
    }];
    m.sequences = vec![Sequence {
        oid: Oid(16470),
        schema: "public".to_string(),
        name: "tickets".to_string(),
        owner: pg(),
        comment: None,
        params: SequenceParams {
            data_type: "bigint".to_string(),
            start: 1,
            increment: 1,
            min: 1,
            max: i64::MAX,
            cache: 1,
            cycle: false,
        },
        owned_by: None,
        grants: vec![],
    }];
    m
}

fn run(b: &SchemaModel, t: &SchemaModel, s: &SchemaModel) -> Analysis {
    analyze(b, t, s, &IdentityMap::self_mapped(b), INTO)
}

fn merged(
    a: &Analysis,
    res: &[Resolution],
    b: &SchemaModel,
    t: &SchemaModel,
    s: &SchemaModel,
) -> Result<MergedModel, MergeError> {
    merge(a, res, b, t, s, &IdentityMap::self_mapped(b), INTO)
}

fn item<'a>(a: &'a Analysis, id: &str) -> &'a Item {
    a.items
        .iter()
        .find(|i| i.id.0 == id)
        .unwrap_or_else(|| panic!("no item {id} in {:#?}", ids(a)))
}

fn ids(a: &Analysis) -> Vec<(String, Outcome)> {
    a.items
        .iter()
        .map(|i| (i.id.0.clone(), i.outcome.clone()))
        .collect()
}

fn conflict<'a>(a: &'a Analysis, id: &str) -> &'a Conflict {
    a.conflicts
        .iter()
        .find(|c| c.id.0 == id)
        .unwrap_or_else(|| panic!("no conflict {id} in {:#?}", a.conflicts))
}

fn cid(id: &str) -> ConflictId {
    ConflictId(id.to_string())
}

fn resolve(id: &str, choice: Choice) -> Resolution {
    Resolution {
        conflict: cid(id),
        choice,
    }
}

fn tbl<'a>(m: &'a mut SchemaModel, name: &str) -> &'a mut Table {
    m.tables.iter_mut().find(|t| t.name == name).unwrap()
}

fn column<'a>(t: &'a mut Table, name: &str) -> &'a mut Column {
    t.columns.iter_mut().find(|c| c.name == name).unwrap()
}

fn mt<'a>(m: &'a SchemaModel, name: &str) -> &'a Table {
    m.tables
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no table {name}"))
}

fn mc<'a>(t: &'a Table, name: &str) -> Option<&'a Column> {
    t.columns.iter().find(|c| c.name == name)
}

fn normalized(m: &SchemaModel) -> SchemaModel {
    let mut m = m.clone();
    m.normalize();
    m
}

// ---- outcomes ----

#[test]
fn source_only_change_applies() {
    let b = base();
    let t = b.clone();
    let mut s = b.clone();
    column(tbl(&mut s, "users"), "email").default = Some("'x'::character varying".to_string());
    let a = run(&b, &t, &s);
    assert_eq!(a.items.len(), 1, "{:#?}", ids(&a));
    let i = item(&a, "column:16420:2");
    assert_eq!(i.outcome, Outcome::Apply);
    assert!(matches!(i.source, Some(Change::Alter { .. })));
    assert!(i.target.is_none());
    assert!(a.conflicts.is_empty());
    assert_eq!(i.key.kind, ObjKind::Column);
    assert_eq!(i.key.table.as_deref(), Some("users"));

    let m = merged(&a, &[], &b, &t, &s).unwrap();
    let email = mc(mt(&m.model, "users"), "email").unwrap();
    assert_eq!(email.default.as_deref(), Some("'x'::character varying"));
    assert_eq!(email.attnum, 2);
    assert!(m.own_sql.is_empty());
}

#[test]
fn target_only_change_is_ignored() {
    let b = base();
    let mut t = b.clone();
    let s = b.clone();
    column(tbl(&mut t, "users"), "email").comment = Some("t".to_string());
    tbl(&mut t, "orders").comment = Some("t".to_string());
    let a = run(&b, &t, &s);
    assert_eq!(item(&a, "column:16420:2").outcome, Outcome::TargetOnly);
    assert_eq!(item(&a, "table:16430").outcome, Outcome::TargetOnly);
    assert!(a.conflicts.is_empty());
    // Nothing applied: the merged model is the target.
    let m = merged(&a, &[], &b, &t, &s).unwrap();
    assert_eq!(m.model, normalized(&t));
}

#[test]
fn identical_changes_overlap() {
    let b = base();
    let mut t = b.clone();
    column(tbl(&mut t, "users"), "email").comment = Some("mail".to_string());
    let s = t.clone();
    let a = run(&b, &t, &s);
    let i = item(&a, "column:16420:2");
    assert_eq!(i.outcome, Outcome::Overlap);
    assert!(i.source.is_some() && i.target.is_some());
    assert!(a.conflicts.is_empty());
}

#[test]
fn different_changes_conflict() {
    let b = base();
    let mut t = b.clone();
    let mut s = b.clone();
    // Different properties of one sub-object still conflict (per sub-object, not per property).
    column(tbl(&mut t, "users"), "email").comment = Some("t".to_string());
    column(tbl(&mut s, "users"), "email").default = Some("'s'::character varying".to_string());
    let a = run(&b, &t, &s);
    let i = item(&a, "column:16420:2");
    assert_eq!(i.outcome, Outcome::Conflict(cid("column:16420:2")));
    let c = conflict(&a, "column:16420:2");
    assert_eq!(c.kind, ConflictKind::Edit);
    assert_eq!(c.items, vec![ItemId("column:16420:2".to_string())]);
    assert!(c.base.is_some() && c.target.is_some() && c.source.is_some());
    assert_eq!(a.conflicts.len(), 1);
}

#[test]
fn drop_vs_edit_conflicts_both_ways() {
    // The source drops a column the target edits.
    let b = base();
    let mut t = b.clone();
    let mut s = b.clone();
    column(tbl(&mut t, "users"), "email").comment = Some("t".to_string());
    tbl(&mut s, "users").columns.retain(|c| c.name != "email");
    let a = run(&b, &t, &s);
    assert_eq!(
        conflict(&a, "column:16420:2").kind,
        ConflictKind::DropVsEdit
    );
    assert!(conflict(&a, "column:16420:2").source.is_none());

    // The reverse: the target drops it, the source edits it.
    let a = run(&b, &s, &t);
    let c = conflict(&a, "column:16420:2");
    assert_eq!(c.kind, ConflictKind::DropVsEdit);
    assert!(c.target.is_none() && c.source.is_some());

    // A dropped table against a new index on it: one conflict for the table and what's in it.
    let mut t = b.clone();
    tbl(&mut t, "orders").indexes = vec![index(16700, "orders_user_idx", "user_id")];
    let mut s = b.clone();
    s.tables.retain(|t| t.name != "orders");
    let a = run(&b, &t, &s);
    let c = conflict(&a, "table:16430");
    assert_eq!(c.kind, ConflictKind::DropVsEdit);
    let ix = "index:name:table:16430/orders_user_idx";
    assert_eq!(item(&a, ix).outcome, Outcome::Conflict(cid("table:16430")));
    assert!(c.items.contains(&ItemId(ix.to_string())));
    // The source's drops of the table's columns go with it.
    assert_eq!(
        item(&a, "column:16430:1").outcome,
        Outcome::Conflict(cid("table:16430"))
    );
    assert_eq!(a.conflicts.len(), 1, "{:#?}", a.conflicts);
}

#[test]
fn same_name_creates_with_different_definitions_collide() {
    let b = base();
    let mut t = b.clone();
    let mut s = b.clone();
    t.roles.push(role(16500, "reporter"));
    s.roles.push(Role {
        login: false,
        ..role(16510, "reporter")
    });
    let a = run(&b, &t, &s);
    let c = conflict(&a, "role:name:reporter");
    assert_eq!(c.kind, ConflictKind::CreateCollision);
    assert!(c.base.is_none());
    assert_eq!(
        item(&a, "role:name:reporter").outcome,
        Outcome::Conflict(cid("role:name:reporter"))
    );
}

#[test]
fn same_name_identical_creates_overlap() {
    let b = base();
    let mut t = b.clone();
    let mut s = b.clone();
    // Each side allocates its own OIDs.
    t.tables.push(table(
        17000,
        "public",
        "audit",
        vec![col(1, "id", "bigint")],
    ));
    s.tables.push(table(
        17100,
        "public",
        "audit",
        vec![col(1, "id", "bigint")],
    ));
    let a = run(&b, &t, &s);
    assert!(a.conflicts.is_empty(), "{:#?}", a.conflicts);
    assert_eq!(
        item(&a, "table:name:schema:2200/audit").outcome,
        Outcome::Overlap
    );
    assert_eq!(
        item(&a, "column:name:table:name:schema:2200/audit/id").outcome,
        Outcome::Overlap
    );
    // The merged model keeps the target's object.
    let m = merged(&a, &[], &b, &t, &s).unwrap();
    assert_eq!(mt(&m.model, "audit").oid, Oid(17000));
}

#[test]
fn both_sides_adding_columns_is_an_order_conflict() {
    let b = base();
    let mut t = b.clone();
    let mut s = b.clone();
    tbl(&mut t, "users").columns.push(col(4, "t_col", "text"));
    tbl(&mut s, "users").columns.push(col(4, "s_col", "text"));
    let a = run(&b, &t, &s);
    let c = conflict(&a, "order:table:16420");
    assert_eq!(c.kind, ConflictKind::ColumnOrder);
    let t_id = ItemId("column:name:table:16420/t_col".to_string());
    let s_id = ItemId("column:name:table:16420/s_col".to_string());
    assert!(c.items.contains(&t_id) && c.items.contains(&s_id));
    assert_eq!(item(&a, &s_id.0).outcome, Outcome::Conflict(c.id.clone()));
    assert_eq!(a.conflicts.len(), 1);

    // Only the source adding columns: no order conflict.
    let a = run(&b, &b, &s);
    assert!(a.conflicts.is_empty());
    assert_eq!(item(&a, &s_id.0).outcome, Outcome::Apply);
}

// ---- identity ----

#[test]
fn rename_is_alter_not_drop_create() {
    let b = base();
    let t = b.clone();
    let mut s = b.clone();
    let users = tbl(&mut s, "users");
    users.name = "accounts".to_string();
    column(users, "email").name = "mail".to_string();
    // Postgres prints the referencing FK with the new name.
    let fk = &mut tbl(&mut s, "orders").constraints[0];
    fk.definition = "FOREIGN KEY (user_id) REFERENCES accounts(id)".to_string();
    fk.references.as_mut().unwrap().table = "accounts".to_string();
    let a = run(&b, &t, &s);
    for i in &a.items {
        assert!(
            matches!(i.source, Some(Change::Alter { .. })),
            "{} isn't an alter",
            i.id.0
        );
    }
    let Some(Change::Alter { before, after }) = &item(&a, "table:16420").source else {
        panic!("an alter");
    };
    let (ObjSnapshot::Table(before), ObjSnapshot::Table(after)) = (&**before, &**after) else {
        panic!("table snapshots");
    };
    assert_eq!(
        (before.name.as_str(), after.name.as_str()),
        ("users", "accounts")
    );
    assert_eq!(item(&a, "column:16420:2").outcome, Outcome::Apply);
    assert!(!item(&a, "column:16420:2").data_loss);

    let m = merged(&a, &[], &b, &t, &s).unwrap();
    let accounts = mt(&m.model, "accounts");
    assert_eq!(accounts.oid, Oid(16420));
    assert_eq!(mc(accounts, "mail").unwrap().attnum, 2);
    let fk = &mt(&m.model, "orders").constraints[0];
    assert_eq!(fk.references.as_ref().unwrap().table, "accounts");
}

#[test]
fn set_schema_is_alter() {
    let b = base();
    let t = b.clone();
    let mut s = b.clone();
    tbl(&mut s, "users").schema = "app".to_string();
    let a = run(&b, &t, &s);
    let i = item(&a, "table:16420");
    let Some(Change::Alter { after, .. }) = &i.source else {
        panic!("an alter");
    };
    let ObjSnapshot::Table(after) = &**after else {
        panic!("a table");
    };
    assert_eq!(after.schema, "app");
    assert!(
        a.items
            .iter()
            .all(|i| !matches!(i.source, Some(Change::Create(_) | Change::Drop(_))))
    );
    // Renaming the schema itself doesn't touch the tables in it.
    let mut s = b.clone();
    s.schemas[0].name = "main".to_string();
    for t in &mut s.tables {
        t.schema = "main".to_string();
        for k in &mut t.constraints {
            if let Some(r) = &mut k.references {
                r.schema = "main".to_string();
            }
        }
    }
    s.enums[0].schema = "main".to_string();
    s.sequences[0].schema = "main".to_string();
    let a = run(&b, &b, &s);
    assert_eq!(
        a.items.iter().map(|i| i.id.0.as_str()).collect::<Vec<_>>(),
        ["schema:2200"]
    );
}

#[test]
fn drop_and_recreate_same_name_is_drop_plus_create_with_data_loss() {
    let b = base();
    let t = b.clone();
    let mut s = b.clone();
    let users = tbl(&mut s, "users");
    users.columns.retain(|c| c.name != "email");
    users.columns.push(col(4, "email", "character varying(50)"));
    let a = run(&b, &t, &s);
    let dropped = item(&a, "column:16420:2");
    assert!(matches!(dropped.source, Some(Change::Drop(_))));
    assert!(dropped.data_loss);
    assert_eq!(dropped.outcome, Outcome::Apply);
    let created = item(&a, "column:name:table:16420/email");
    assert!(matches!(created.source, Some(Change::Create(_))));
    assert!(!created.data_loss);
}

/// After a merge with keep: base table 900 is 500 on the parent and 900 on the branch, and the
/// parent has an unrelated new table that happens to get OID 900.
fn after_merge() -> (SchemaModel, SchemaModel, SchemaModel, IdentityMap) {
    let mut base = SchemaModel::empty(17);
    base.schemas = vec![schema(2200, "public")];
    base.roles = vec![role(10, "postgres")];
    base.tables = vec![table(900, "public", "t", vec![col(1, "id", "bigint")])];
    let mut parent = base.clone();
    parent.tables = vec![
        table(500, "public", "t", vec![col(1, "id", "bigint")]),
        table(900, "public", "other", vec![col(1, "x", "text")]),
    ];
    let branch = base.clone();
    let at = |oid, sub| Some(ObjAt::new(Oid(oid), sub));
    let map = IdentityMap::from_pairs([
        IdentityPair {
            class: ObjClass::Schema,
            base: ObjAt::new(Oid(2200), None),
            parent: at(2200, None),
            branch: at(2200, None),
        },
        IdentityPair {
            class: ObjClass::Role,
            base: ObjAt::new(Oid(10), None),
            parent: at(10, None),
            branch: at(10, None),
        },
        IdentityPair {
            class: ObjClass::Table,
            base: ObjAt::new(Oid(900), None),
            parent: at(500, None),
            branch: at(900, None),
        },
        IdentityPair {
            class: ObjClass::Column,
            base: ObjAt::new(Oid(900), Some(1)),
            parent: at(500, Some(1)),
            branch: at(900, Some(1)),
        },
    ]);
    (base, parent, branch, map)
}

#[test]
fn identity_map_matches_objects_created_by_a_previous_merge() {
    let (base, parent, mut branch, map) = after_merge();
    branch.tables[0].comment = Some("c".to_string());
    let a = analyze(&base, &parent, &branch, &map, INTO);
    let t = item(&a, "table:900");
    assert_eq!(t.outcome, Outcome::Apply);
    assert!(t.target.is_none(), "the parent's 500 is the base's 900");
    // The parent's own 900 is a new object, matched by name.
    assert_eq!(
        item(&a, "table:name:schema:2200/other").outcome,
        Outcome::TargetOnly
    );
    assert!(
        a.items
            .iter()
            .all(|i| !matches!(i.source, Some(Change::Drop(_))))
    );
    assert!(
        a.items
            .iter()
            .all(|i| !matches!(i.target, Some(Change::Drop(_))))
    );
    let m = merge(&a, &[], &base, &parent, &branch, &map, INTO).unwrap();
    // The merged model names objects as the target does.
    let t = mt(&m.model, "t");
    assert_eq!((t.oid, t.comment.as_deref()), (Oid(500), Some("c")));
    assert_eq!(mt(&m.model, "other").oid, Oid(900));
}

// ---- integrity ----

#[test]
fn fk_to_column_dropped_on_target_is_a_dependency_conflict() {
    let b = base();
    let mut t = b.clone();
    let users = tbl(&mut t, "users");
    users.columns.retain(|c| c.name != "code");
    users.constraints.clear();
    let mut s = b.clone();
    let orders = tbl(&mut s, "orders");
    orders.columns.push(col(3, "user_code", "text"));
    orders.constraints.push(fk(
        16490,
        "orders_code_fk",
        vec![3],
        (16420, "users"),
        vec![3],
        "FOREIGN KEY (user_code) REFERENCES users(code)",
    ));
    let a = run(&b, &t, &s);
    let fk_id = "constraint:name:table:16430/orders_code_fk";
    let dep = format!("dependency:{fk_id}");
    let c = conflict(&a, &dep);
    assert_eq!(c.kind, ConflictKind::Dependency);
    assert!(c.items.contains(&ItemId(fk_id.to_string())));
    assert!(c.items.contains(&ItemId("column:16420:3".to_string())));
    assert_eq!(item(&a, fk_id).outcome, Outcome::Conflict(cid(&dep)));
    assert_eq!(
        item(&a, "column:16420:3").outcome,
        Outcome::Conflict(cid(&dep))
    );

    // Keep target: no FK, no column. Take source: both, and the reference resolves.
    let m = merged(&a, &[resolve(&dep, Choice::KeepTarget)], &b, &t, &s).unwrap();
    assert!(
        mt(&m.model, "orders")
            .constraints
            .iter()
            .all(|c| c.name != "orders_code_fk")
    );
    assert!(mc(mt(&m.model, "users"), "code").is_none());
    let m = merged(&a, &[resolve(&dep, Choice::TakeSource)], &b, &t, &s).unwrap();
    let fk = mt(&m.model, "orders")
        .constraints
        .iter()
        .find(|c| c.name == "orders_code_fk")
        .unwrap();
    let code = mc(mt(&m.model, "users"), "code").unwrap();
    assert_eq!(fk.references.as_ref().unwrap().columns, vec![code.attnum]);
    assert_eq!(fk.oid, NEW_OID);
}

#[test]
fn policy_naming_missing_role_is_a_dependency_conflict() {
    let b = base();
    let mut t = b.clone();
    t.roles.retain(|r| r.name != "auditor");
    let mut s = b.clone();
    tbl(&mut s, "orders")
        .policies
        .push(policy(16495, "audit", vec![rr(16401, "auditor")], "true"));
    let a = run(&b, &t, &s);
    let p = "policy:name:table:16430/audit";
    let c = conflict(&a, &format!("dependency:{p}"));
    assert_eq!(c.kind, ConflictKind::Dependency);
    assert!(c.items.contains(&ItemId(p.to_string())));
    assert!(c.items.contains(&ItemId("role:16401".to_string())));
    // A role outside the model (excluded, like `anon`) never counts as missing.
    let mut s = b.clone();
    tbl(&mut s, "orders")
        .policies
        .push(policy(16495, "audit", vec![rr(16300, "anon")], "true"));
    let a = run(&b, &t, &s);
    assert!(a.conflicts.is_empty(), "{:#?}", a.conflicts);
}

#[test]
fn dependency_on_reported_function_is_listed() {
    let b = base();
    let t = b.clone();
    let mut s = b.clone();
    tbl(&mut s, "users").columns.push(Column {
        default: Some("public.next_code()".to_string()),
        ..col(4, "ref_code", "text")
    });
    let f = Reported {
        kind: ReportedKind::Function,
        oid: Oid(16600),
        schema: Some("public".to_string()),
        name: "next_code".to_string(),
        parent: None,
        arguments: Some(String::new()),
        definition: Some("CREATE FUNCTION public.next_code() …".to_string()),
    };
    s.reported.push(f.clone());
    let a = run(&b, &t, &s);
    assert!(a.conflicts.is_empty(), "a dependency, not a conflict");
    assert_eq!(
        a.reported,
        vec![ReportedChange {
            object: f.clone(),
            change: ReportedChangeKind::Created,
            needed_by: vec![ItemId("column:name:table:16420/ref_code".to_string())],
        }]
    );
    // A function the target also has isn't a dependency (nor a change to report).
    let mut t = t;
    t.reported.push(f);
    let a = run(&b, &t, &s);
    assert!(a.reported.is_empty(), "{:#?}", a.reported);
}

// ---- directions ----

#[test]
fn from_parent_direction_swaps_sides() {
    let (base, mut parent, branch, map) = after_merge();
    parent.tables[0].name = "t2".to_string();
    // Update the branch from its parent: the parent is the source.
    let a = analyze(&base, &branch, &parent, &map, Direction::FromParent);
    let t = item(&a, "table:900");
    assert_eq!(t.outcome, Outcome::Apply);
    assert!(matches!(t.source, Some(Change::Alter { .. })));
    // The parent's unrelated 900 comes over as a new table.
    let other = item(&a, "table:name:schema:2200/other");
    assert!(matches!(other.source, Some(Change::Create(_))));
    assert_eq!(other.outcome, Outcome::Apply);
    let m = merge(
        &a,
        &[],
        &base,
        &branch,
        &parent,
        &map,
        Direction::FromParent,
    )
    .unwrap();
    // Named as the branch (the target) names it.
    assert_eq!(mt(&m.model, "t2").oid, Oid(900));
    assert_eq!(mt(&m.model, "other").oid, NEW_OID);
}

// ---- flags ----

#[test]
fn type_widening_is_not_data_loss() {
    let b = base();
    let mut s = b.clone();
    let users = tbl(&mut s, "users");
    column(users, "email").data_type = "character varying(100)".to_string();
    column(users, "code").data_type = "integer".to_string();
    let orders = tbl(&mut s, "orders");
    column(orders, "user_id").data_type = "bigint".to_string();
    column(orders, "id").not_null = false;
    let a = run(&b, &b, &s);
    assert!(
        !item(&a, "column:16420:2").data_loss,
        "varchar(50) → varchar(100)"
    );
    assert!(!item(&a, "column:16430:2").data_loss, "int4 → int8");
    assert!(!item(&a, "column:16430:1").data_loss, "drop NOT NULL");
    assert!(item(&a, "column:16420:3").data_loss, "text → integer");
    for (from, to, safe) in [
        ("character varying(50)", "text", true),
        ("character varying(50)", "character varying", true),
        ("character varying(50)", "character varying(20)", false),
        ("text", "character varying", true),
        ("smallint", "integer", true),
        ("bigint", "integer", false),
        ("numeric(10,2)", "numeric(12,2)", true),
        ("numeric(10,2)", "numeric(12,3)", false),
        ("numeric(10,2)", "numeric", true),
        ("real", "double precision", true),
        ("integer[]", "bigint[]", true),
        ("integer[]", "bigint", false),
    ] {
        assert_eq!(classify::safe_widening(from, to), safe, "{from} → {to}");
    }
}

#[test]
fn bypass_rls_is_flagged_security() {
    let b = base();
    let mut t = b.clone();
    let mut s = b.clone();
    s.roles
        .iter_mut()
        .find(|r| r.name == "app")
        .unwrap()
        .bypass_rls = true;
    s.roles.push(Role {
        bypass_rls: true,
        ..role(16520, "admin2")
    });
    s.roles.push(role(16521, "plain"));
    t.roles.push(Role {
        bypass_rls: true,
        ..role(16530, "target_only")
    });
    let a = run(&b, &t, &s);
    assert!(item(&a, "role:16400").security, "gains BYPASSRLS");
    assert!(
        item(&a, "role:name:admin2").security,
        "created with BYPASSRLS"
    );
    assert!(!item(&a, "role:name:plain").security);
    assert!(
        !item(&a, "role:name:target_only").security,
        "nothing applied"
    );
}

// E22: the memberships Postgres gives the owner in the roles a side made in the owner's session
// stay in the models, but are never a change of the owner; the merged owner keeps the target's.
#[test]
fn owner_memberships_are_never_a_change() {
    let b = base();
    let mut t = b.clone();
    let mut s = b.clone();
    s.roles.push(role(16600, "readers"));
    let owner = s.roles.iter_mut().find(|r| r.name == "postgres").unwrap();
    owner.member_of = owner_grants(&nr(16600, "readers"), &pg());
    // The target made a role of its own the same way.
    t.roles.push(role(16700, "writers"));
    let owner = t.roles.iter_mut().find(|r| r.name == "postgres").unwrap();
    owner.member_of = owner_grants(&nr(16700, "writers"), &pg());

    let a = run(&b, &t, &s);
    assert_eq!(
        ids(&a),
        [
            ("role:name:readers".to_string(), Outcome::Apply),
            ("role:name:writers".to_string(), Outcome::TargetOnly),
        ],
        "only the new roles, never the owner"
    );
    let m = merged(&a, &[], &b, &t, &s).unwrap().model;
    let owner = m.roles.iter().find(|r| r.name == "postgres").unwrap();
    let names: Vec<&str> = owner.member_of.iter().map(|x| x.role.name.as_str()).collect();
    assert_eq!(names, ["writers", "writers"], "the target's, kept in the model");

    // E30: any other change of the owner is reported ("apply by hand"), never merged; the
    // merged owner is the target's, memberships included.
    let mut s2 = s.clone();
    s2.roles
        .iter_mut()
        .find(|r| r.name == "postgres")
        .unwrap()
        .connection_limit = 5;
    let a = run(&b, &t, &s2);
    assert!(a.items.iter().all(|i| i.id.0 != "role:10"), "{:#?}", ids(&a));
    assert!(reported_role(&a, "postgres").is_some());
    let m = merged(&a, &[], &b, &t, &s2).unwrap().model;
    let owner = m.roles.iter().find(|r| r.name == "postgres").unwrap();
    assert_eq!(owner.connection_limit, -1);
    let names: Vec<&str> = owner.member_of.iter().map(|x| x.role.name.as_str()).collect();
    assert_eq!(names, ["writers", "writers"]);
}

// ---- resolutions ----

/// The target and the source both edit `users.email` (comment vs default); the source also
/// comments `orders`.
fn edit_conflict() -> (SchemaModel, SchemaModel, SchemaModel) {
    let b = base();
    let mut t = b.clone();
    let mut s = b.clone();
    column(tbl(&mut t, "users"), "email").comment = Some("t".to_string());
    column(tbl(&mut s, "users"), "email").default = Some("'s'::character varying".to_string());
    tbl(&mut s, "orders").comment = Some("o".to_string());
    (b, t, s)
}

#[test]
fn merge_refuses_unresolved_conflicts() {
    let (b, t, s) = edit_conflict();
    let a = run(&b, &t, &s);
    assert_eq!(
        merged(&a, &[], &b, &t, &s),
        Err(MergeError::Unresolved(vec![cid("column:16420:2")]))
    );
    // A resolution for a conflict that no longer exists doesn't count.
    assert_eq!(
        merged(
            &a,
            &[resolve("column:16420:9", Choice::KeepTarget)],
            &b,
            &t,
            &s
        ),
        Err(MergeError::Unresolved(vec![cid("column:16420:2")]))
    );
}

#[test]
fn keep_target_leaves_item_out() {
    let (b, t, s) = edit_conflict();
    let a = run(&b, &t, &s);
    let m = merged(
        &a,
        &[resolve("column:16420:2", Choice::KeepTarget)],
        &b,
        &t,
        &s,
    )
    .unwrap();
    let email = mc(mt(&m.model, "users"), "email").unwrap();
    assert_eq!(email.comment.as_deref(), Some("t"));
    assert_eq!(email.default, None);
    // Non-conflicting source changes still apply.
    assert_eq!(mt(&m.model, "orders").comment.as_deref(), Some("o"));
}

#[test]
fn take_source_uses_source_version() {
    let (b, t, s) = edit_conflict();
    let a = run(&b, &t, &s);
    let m = merged(
        &a,
        &[resolve("column:16420:2", Choice::TakeSource)],
        &b,
        &t,
        &s,
    )
    .unwrap();
    let email = mc(mt(&m.model, "users"), "email").unwrap();
    assert_eq!(email.default.as_deref(), Some("'s'::character varying"));
    assert_eq!(email.comment, None);

    // Taking a source drop over a target edit removes the column, flagged as data loss.
    let mut t = b.clone();
    column(tbl(&mut t, "users"), "code").comment = Some("t".to_string());
    let mut s = b.clone();
    let users = tbl(&mut s, "users");
    users.columns.retain(|c| c.name != "code");
    users.constraints.clear();
    let a = run(&b, &t, &s);
    assert!(item(&a, "column:16420:3").data_loss);
    let m = merged(
        &a,
        &[resolve("column:16420:3", Choice::TakeSource)],
        &b,
        &t,
        &s,
    )
    .unwrap();
    assert!(mc(mt(&m.model, "users"), "code").is_none());
}

#[test]
fn own_sql_is_carried() {
    let (b, t, s) = edit_conflict();
    let a = run(&b, &t, &s);
    let sql = "ALTER TABLE users ALTER COLUMN email SET DEFAULT 'own';";
    let m = merged(
        &a,
        &[resolve("column:16420:2", Choice::OwnSql(sql.to_string()))],
        &b,
        &t,
        &s,
    )
    .unwrap();
    assert_eq!(m.own_sql, vec![(cid("column:16420:2"), sql.to_string())]);
    // The model keeps the target's version; the own SQL replaces the item in the plan.
    let email = mc(mt(&m.model, "users"), "email").unwrap();
    assert_eq!(email.comment.as_deref(), Some("t"));
    assert!(matches!(
        merged(
            &a,
            &[resolve("column:16420:2", Choice::OwnSql("  ".to_string()))],
            &b,
            &t,
            &s
        ),
        Err(MergeError::InvalidChoice { .. })
    ));
}

fn order_conflict() -> (SchemaModel, SchemaModel, SchemaModel) {
    let b = base();
    let mut t = b.clone();
    let mut s = b.clone();
    tbl(&mut t, "users").columns.push(col(4, "t_col", "text"));
    let users = tbl(&mut s, "users");
    users.columns.push(col(4, "s_col", "text"));
    users.columns.push(col(5, "s_col2", "integer"));
    (b, t, s)
}

#[test]
fn target_columns_first_appends_source_columns() {
    let (b, t, s) = order_conflict();
    let a = run(&b, &t, &s);
    let m = merged(
        &a,
        &[resolve("order:table:16420", Choice::TargetColumnsFirst)],
        &b,
        &t,
        &s,
    )
    .unwrap();
    let cols: Vec<(i16, &str)> = mt(&m.model, "users")
        .columns
        .iter()
        .map(|c| (c.attnum, c.name.as_str()))
        .collect();
    assert_eq!(
        cols,
        [
            (1, "id"),
            (2, "email"),
            (3, "code"),
            (4, "t_col"),
            (5, "s_col"),
            (6, "s_col2")
        ]
    );
    // Only valid on an order conflict.
    let (b, t, s) = edit_conflict();
    let a = run(&b, &t, &s);
    assert!(matches!(
        merged(
            &a,
            &[resolve("column:16420:2", Choice::TargetColumnsFirst)],
            &b,
            &t,
            &s
        ),
        Err(MergeError::InvalidChoice { .. })
    ));
}

#[test]
fn take_source_on_order_conflict_is_invalid() {
    let (b, t, s) = order_conflict();
    let a = run(&b, &t, &s);
    assert!(matches!(
        merged(&a, &[resolve("order:table:16420", Choice::TakeSource)], &b, &t, &s),
        Err(MergeError::InvalidChoice { conflict, .. }) if conflict == cid("order:table:16420")
    ));
    // E12b: only target columns first or own SQL resolve an order conflict.
    assert!(matches!(
        merged(
            &a,
            &[resolve("order:table:16420", Choice::KeepTarget)],
            &b,
            &t,
            &s
        ),
        Err(MergeError::InvalidChoice { .. })
    ));
    let own = Choice::OwnSql("ALTER TABLE users ADD COLUMN s_col text;".to_string());
    assert!(merged(&a, &[resolve("order:table:16420", own)], &b, &t, &s).is_ok());
}

#[test]
fn ids_are_stable_across_recompute() {
    let (b, t, mut s) = edit_conflict();
    let mut audit = table(17000, "public", "audit", vec![col(1, "note", "text")]);
    audit.grants = vec![grant(rr(16400, "app"), "SELECT")];
    s.tables.push(audit);
    let a1 = run(&b, &t, &s);
    // The source is read again; its new objects come back under other OIDs.
    let mut s2 = s.clone();
    tbl(&mut s2, "audit").oid = Oid(17999);
    let a2 = run(&b, &t, &s2);
    let ids1: Vec<&ItemId> = a1.items.iter().map(|i| &i.id).collect();
    let ids2: Vec<&ItemId> = a2.items.iter().map(|i| &i.id).collect();
    assert_eq!(ids1, ids2);
    assert!(ids1.contains(&&ItemId(
        "grant:table:name:schema:2200/audit/role:16400/SELECT".to_string()
    )));
    let c1: Vec<&ConflictId> = a1.conflicts.iter().map(|c| &c.id).collect();
    let c2: Vec<&ConflictId> = a2.conflicts.iter().map(|c| &c.id).collect();
    assert_eq!(c1, c2);
    // Resolutions made against the first analysis apply to the second.
    let res = [resolve(&c1[0].0, Choice::KeepTarget)];
    let m = merged(&a2, &res, &b, &t, &s2).unwrap();
    let audit = mt(&m.model, "audit");
    assert_eq!(audit.oid, NEW_OID);
    assert_eq!(audit.grants.len(), 1);
}

// ---- beyond the brief's list ----

#[test]
fn enum_label_rename_is_an_alter_and_keeps_its_oid() {
    let b = base();
    let mut s = b.clone();
    s.enums[0].labels[1].label = "finished".to_string();
    s.enums[0].labels.push(EnumLabel {
        oid: Oid(16800),
        label: "archived".to_string(),
    });
    let a = run(&b, &b, &s);
    assert_eq!(item(&a, "enum:16460").outcome, Outcome::Apply);
    let m = merged(&a, &[], &b, &b, &s).unwrap();
    let labels: Vec<(Oid, &str)> = m.model.enums[0]
        .labels
        .iter()
        .map(|l| (l.oid, l.label.as_str()))
        .collect();
    assert_eq!(
        labels,
        [
            (Oid(16461), "new"),
            (Oid(16462), "finished"),
            (NEW_OID, "archived")
        ]
    );
}

#[test]
fn enum_label_removal_conflicts_and_cannot_take_source() {
    let b = base();
    let mut s = b.clone();
    // Postgres can't drop an enum value.
    s.enums[0].labels.remove(0);
    let a = run(&b, &b, &s);
    let c = conflict(&a, "enum:16460");
    assert_eq!(c.kind, ConflictKind::Edit);
    assert!(matches!(
        merged(&a, &[resolve("enum:16460", Choice::TakeSource)], &b, &b, &s),
        Err(MergeError::InvalidChoice { .. })
    ));
    assert!(merged(&a, &[resolve("enum:16460", Choice::KeepTarget)], &b, &b, &s).is_ok());
}

#[test]
fn grants_are_items_of_their_own() {
    let b = base();
    let mut s = b.clone();
    tbl(&mut s, "users")
        .grants
        .push(grant(rr(16400, "app"), "INSERT"));
    column(tbl(&mut s, "users"), "email").grants = vec![grant(RoleRef::Public, "UPDATE")];
    let a = run(&b, &b, &s);
    assert_eq!(
        a.items.iter().map(|i| i.id.0.as_str()).collect::<Vec<_>>(),
        [
            "grant:column:16420:2/public/UPDATE",
            "grant:table:16420/role:16400/INSERT"
        ]
    );
    let m = merged(&a, &[], &b, &b, &s).unwrap();
    let users = mt(&m.model, "users");
    assert_eq!(users.grants.len(), 2);
    assert_eq!(mc(users, "email").unwrap().grants.len(), 1);
}

/// The base with every remaining kind of reference: an identity column with grants, an enum
/// column, an owned sequence, schema and sequence grants, a membership and a default privilege.
fn rich() -> SchemaModel {
    let mut m = base();
    m.schemas[0].grants = vec![grant(RoleRef::Public, "USAGE")];
    m.sequences[0].owned_by = Some(OwnedBy {
        table_oid: Oid(16430),
        schema: "public".to_string(),
        table: "orders".to_string(),
        attnum: 2,
        column: "user_id".to_string(),
    });
    m.sequences[0].grants = vec![grant(rr(16400, "app"), "USAGE")];
    let orders = tbl(&mut m, "orders");
    column(orders, "id").identity = Some(Identity {
        generation: IdentityGeneration::Always,
        sequence_oid: Oid(16431),
        sequence_name: "orders_id_seq".to_string(),
        params: m_params(),
        grants: vec![grant(rr(16400, "app"), "USAGE")],
    });
    orders.columns.push(Column {
        type_oid: Oid(16460),
        default: Some("'new'::status".to_string()),
        ..col(3, "state", "status")
    });
    m.roles[2].member_of = vec![Membership {
        role: nr(16400, "app"),
        grantor: pg(),
        admin: false,
        inherit: true,
        set: true,
    }];
    m.default_privileges = vec![DefaultPrivilege {
        oid: Oid(16900),
        role: pg(),
        schema: Some("public".to_string()),
        object_type: DefaultPrivilegeObject::Tables,
        grants: vec![grant(rr(16400, "app"), "SELECT")],
    }];
    m
}

fn m_params() -> SequenceParams {
    SequenceParams {
        data_type: "bigint".to_string(),
        start: 1,
        increment: 1,
        min: 1,
        max: i64::MAX,
        cache: 1,
        cycle: false,
    }
}

#[test]
fn merge_with_nothing_changed_is_the_target() {
    let m = rich();
    let a = run(&m, &m, &m);
    assert!(a.items.is_empty(), "{:#?}", ids(&a));
    let out = merged(&a, &[], &m, &m, &m).unwrap();
    assert_eq!(out.model, normalized(&m));
}

#[test]
fn source_renames_follow_into_target_references() {
    // The source renames orders.user_id and the role app; the owned sequence, the grants and
    // the membership in the merged model use the new names.
    let b = rich();
    let mut s = b.clone();
    column(tbl(&mut s, "orders"), "user_id").name = "customer_id".to_string();
    s.roles[1].name = "web".to_string();
    let a = run(&b, &b, &s);
    assert!(a.conflicts.is_empty(), "{:#?}", a.conflicts);
    let m = merged(&a, &[], &b, &b, &s).unwrap().model;
    assert_eq!(
        m.sequences[0].owned_by.as_ref().unwrap().column,
        "customer_id"
    );
    let auditor = m.roles.iter().find(|r| r.name == "auditor").unwrap();
    assert_eq!(auditor.member_of[0].role.name, "web");
    assert_eq!(mt(&m, "users").grants[0].grantee, rr(16400, "web"));
}

// ---- shared types (T1) ----

fn key(kind: ObjKind, schema: Option<&str>, table: Option<&str>, name: &str) -> ObjKey {
    ObjKey {
        kind,
        schema: schema.map(str::to_string),
        table: table.map(str::to_string),
        name: name.to_string(),
    }
}

#[test]
fn obj_keys_sort_by_kind_then_schema_table_and_name() {
    let mut keys = [
        key(ObjKind::Column, Some("public"), Some("t"), "b"),
        key(ObjKind::Table, Some("public"), None, "t"),
        key(ObjKind::Column, Some("public"), Some("t"), "a"),
        key(ObjKind::Schema, None, None, "public"),
    ];
    keys.sort();
    let names: Vec<&str> = keys.iter().map(|k| k.name.as_str()).collect();
    assert_eq!(names, ["public", "t", "a", "b"]);
}

#[test]
fn table_snapshot_is_shallow() {
    let t = &crate::model::sample::full_model().tables[0];
    let ObjSnapshot::Table(s) = ObjSnapshot::table(t) else {
        panic!("a table snapshot");
    };
    assert_eq!(s, t.shallow());
    assert!(s.columns.is_empty() && s.grants.is_empty());
}

#[test]
fn schema_and_sequence_snapshots_are_shallow() {
    let m = crate::model::sample::full_model();
    let ObjSnapshot::Schema(s) = ObjSnapshot::schema(&m.schemas[0]) else {
        panic!("a schema snapshot");
    };
    assert!(s.grants.is_empty());
    let ObjSnapshot::Sequence(q) = ObjSnapshot::sequence(&m.sequences[0]) else {
        panic!("a sequence snapshot");
    };
    assert!(q.grants.is_empty());
}

// ---- fix round 1 ----

#[test]
fn target_dropped_column_with_source_constraint_is_a_dependency_conflict() {
    let b = base();
    let mut t = b.clone();
    let users = tbl(&mut t, "users");
    users.columns.retain(|c| c.name != "code");
    users.constraints.clear();
    let mut s = b.clone();
    tbl(&mut s, "users").constraints.push(Constraint {
        oid: Oid(16491),
        name: "users_code_check".to_string(),
        kind: ConstraintKind::Check,
        definition: "CHECK (code <> ''::text)".to_string(),
        columns: vec![3],
        references: None,
        deferrable: false,
        initially_deferred: false,
        validated: true,
    });
    let a = run(&b, &t, &s);
    let k = "constraint:name:table:16420/users_code_check";
    let c = conflict(&a, &format!("dependency:{k}"));
    assert_eq!(c.kind, ConflictKind::Dependency);
    assert!(c.items.contains(&ItemId("column:16420:3".to_string())));
    assert_eq!(item(&a, k).outcome, Outcome::Conflict(c.id.clone()));
    // Keep target: neither; take source: both, the check on the restored column.
    let m = merged(&a, &[resolve(&c.id.0, Choice::KeepTarget)], &b, &t, &s).unwrap();
    assert!(mt(&m.model, "users").constraints.is_empty());
    let m = merged(&a, &[resolve(&c.id.0, Choice::TakeSource)], &b, &t, &s).unwrap();
    let users = mt(&m.model, "users");
    let code = mc(users, "code").unwrap().attnum;
    let check = users
        .constraints
        .iter()
        .find(|k| k.name == "users_code_check")
        .unwrap();
    assert_eq!(check.columns, vec![code]);
}

/// Ruling: a child whose parent is decided by another conflict joins that conflict (one
/// resolution for both) instead of getting a dependency conflict of its own.
#[test]
fn grant_on_column_left_out_is_a_dependency_conflict() {
    let (b, t, mut s) = order_conflict();
    column(tbl(&mut s, "users"), "s_col").grants = vec![grant(rr(16400, "app"), "SELECT")];
    let a = run(&b, &t, &s);
    let g = "grant:column:name:table:16420/s_col/role:16400/SELECT";
    assert_eq!(
        item(&a, g).outcome,
        Outcome::Conflict(cid("order:table:16420"))
    );
    assert_eq!(a.conflicts.len(), 1, "{:#?}", a.conflicts);
    let first = merged(
        &a,
        &[resolve("order:table:16420", Choice::TargetColumnsFirst)],
        &b,
        &t,
        &s,
    )
    .unwrap();
    assert_eq!(
        mc(mt(&first.model, "users"), "s_col").unwrap().grants.len(),
        1
    );
    let own = Choice::OwnSql("ALTER TABLE users ADD COLUMN s_col text;".to_string());
    let m = merged(&a, &[resolve("order:table:16420", own)], &b, &t, &s).unwrap();
    assert!(mc(mt(&m.model, "users"), "s_col").is_none());
}

#[test]
fn target_rename_with_source_fk_to_old_name_is_a_dependency_conflict() {
    let b = base();
    let mut t = b.clone();
    tbl(&mut t, "users").name = "accounts".to_string();
    let mut s = b.clone();
    tbl(&mut s, "orders").constraints.push(fk(
        16492,
        "orders_user_fk2",
        vec![2],
        (16420, "users"),
        vec![1],
        "FOREIGN KEY (user_id) REFERENCES public.users(id)",
    ));
    let a = run(&b, &t, &s);
    let k = "constraint:name:table:16430/orders_user_fk2";
    let c = conflict(&a, &format!("dependency:{k}"));
    assert_eq!(c.kind, ConflictKind::Dependency);
    assert!(c.items.contains(&ItemId("table:16420".to_string())));
    // Own SQL or keep target resolve it; nothing rewrites the definition.
    let m = merged(&a, &[resolve(&c.id.0, Choice::KeepTarget)], &b, &t, &s).unwrap();
    assert!(
        mt(&m.model, "orders")
            .constraints
            .iter()
            .all(|k| k.name != "orders_user_fk2")
    );
    // A source rename the source's own new FK follows is fine.
    let mut s2 = s.clone();
    tbl(&mut s2, "users").name = "people".to_string();
    tbl(&mut s2, "orders").constraints[1].definition =
        "FOREIGN KEY (user_id) REFERENCES public.people(id)".to_string();
    let a = run(&b, &b, &s2);
    assert!(a.conflicts.is_empty(), "{:#?}", a.conflicts);
}

#[test]
fn target_column_rename_with_source_index_on_old_name_is_a_dependency_conflict() {
    let b = base();
    let mut t = b.clone();
    column(tbl(&mut t, "users"), "email").name = "mail".to_string();
    let mut s = b.clone();
    tbl(&mut s, "users").indexes = vec![index(16700, "users_email_idx", "lower((email)::text)")];
    let a = run(&b, &t, &s);
    let ix = "index:name:table:16420/users_email_idx";
    let c = conflict(&a, &format!("dependency:{ix}"));
    assert!(c.items.contains(&ItemId("column:16420:2".to_string())));
    // An index on a column nobody renamed is no conflict.
    let mut s = b.clone();
    tbl(&mut s, "users").indexes = vec![index(16700, "users_code_idx", "code")];
    assert!(run(&b, &t, &s).conflicts.is_empty());
    // And an index on a column the target dropped is a dependency too.
    let mut t2 = b.clone();
    let users = tbl(&mut t2, "users");
    users.columns.retain(|c| c.name != "code");
    users.constraints.clear();
    let a = run(&b, &t2, &s);
    conflict(&a, "dependency:index:name:table:16420/users_code_idx");
}

#[test]
fn empty_map_rename_is_drop_plus_create() {
    // Re-baseline (E12c): an empty map matches everything by name.
    let b = base();
    let mut s = b.clone();
    tbl(&mut s, "users").name = "accounts".to_string();
    let a = analyze(&b, &b, &s, &IdentityMap::default(), INTO);
    let dropped = item(&a, "table:name:schema:name:public/users");
    assert!(matches!(dropped.source, Some(Change::Drop(_))));
    assert!(dropped.data_loss);
    let created = item(&a, "table:name:schema:name:public/accounts");
    assert!(matches!(created.source, Some(Change::Create(_))));
}

#[test]
fn same_new_table_in_a_renamed_schema_is_one_item() {
    let b = base();
    let mut t = b.clone();
    t.schemas[1].name = "core".to_string();
    t.tables
        .push(table(17000, "core", "foo", vec![col(1, "id", "bigint")]));
    let mut s = b.clone();
    s.tables
        .push(table(17100, "app", "foo", vec![col(1, "id", "bigint")]));
    let a = run(&b, &t, &s);
    assert_eq!(
        item(&a, "table:name:schema:2300/foo").outcome,
        Outcome::Overlap
    );
    assert!(a.conflicts.is_empty(), "{:#?}", a.conflicts);
    let m = merged(&a, &[], &b, &t, &s).unwrap();
    let foos: Vec<&Table> = m.model.tables.iter().filter(|t| t.name == "foo").collect();
    assert_eq!(foos.len(), 1);
    assert_eq!(foos[0].schema, "core");
}

#[test]
fn grant_ids_name_the_grantor_only_when_not_the_owner() {
    let b = base();
    let mut s = b.clone();
    let by_auditor = Grant {
        grantor: nr(16401, "auditor"),
        ..grant(rr(16400, "app"), "SELECT")
    };
    tbl(&mut s, "users").grants.push(by_auditor.clone());
    let mut s2 = b.clone();
    tbl(&mut s2, "users").grants.insert(0, by_auditor);
    for s in [&s, &s2] {
        let a = run(&b, &b, s);
        assert_eq!(
            a.items.iter().map(|i| i.id.0.as_str()).collect::<Vec<_>>(),
            ["grant:table:16420/role:16400/SELECT/by:role:16401"]
        );
    }
}

#[test]
fn newer_object_with_a_colliding_oid_on_the_from_parent_target_is_new() {
    // From parent: the branch is the target. Its new table gets OID 500, which is the parent's
    // OID for base table 900; it must not be taken for that table.
    let (base, parent, mut branch, map) = after_merge();
    branch
        .tables
        .push(table(500, "public", "fresh", vec![col(1, "id", "bigint")]));
    let a = analyze(&base, &branch, &parent, &map, Direction::FromParent);
    let fresh = item(&a, "table:name:schema:2200/fresh");
    assert_eq!(fresh.outcome, Outcome::TargetOnly);
    assert!(matches!(fresh.target, Some(Change::Create(_))));
    assert!(a.items.iter().all(|i| i.id.0 != "table:900"));
}

#[test]
fn drop_vs_edit_takes_in_a_grant_on_the_dropped_column() {
    let b = base();
    let mut t = b.clone();
    column(tbl(&mut t, "users"), "email").grants = vec![grant(rr(16400, "app"), "UPDATE")];
    let mut s = b.clone();
    tbl(&mut s, "users").columns.retain(|c| c.name != "email");
    let a = run(&b, &t, &s);
    let c = conflict(&a, "column:16420:2");
    assert_eq!(c.kind, ConflictKind::DropVsEdit);
    let g = "grant:column:16420:2/role:16400/UPDATE";
    assert!(c.items.contains(&ItemId(g.to_string())));
    assert_eq!(item(&a, g).outcome, Outcome::Conflict(c.id.clone()));
    assert_eq!(a.conflicts.len(), 1);
}

#[test]
fn take_source_on_a_dependency_whose_target_is_kept_out_is_a_broken_reference() {
    // The target drops users.code and users.email while the source edits both (two drop-vs-edit
    // conflicts) and adds an FK to them (a dependency on both).
    let b = base();
    let mut t = b.clone();
    let users = tbl(&mut t, "users");
    users.columns.retain(|c| c.name == "id");
    users.constraints.clear();
    let mut s = b.clone();
    let users = tbl(&mut s, "users");
    column(users, "code").comment = Some("s".to_string());
    column(users, "email").comment = Some("s".to_string());
    tbl(&mut s, "orders").constraints.push(fk(
        16493,
        "orders_pair_fk",
        vec![2, 1],
        (16420, "users"),
        vec![3, 2],
        "FOREIGN KEY (user_id, id) REFERENCES public.users(code, email)",
    ));
    let a = run(&b, &t, &s);
    let dep = "dependency:constraint:name:table:16430/orders_pair_fk";
    assert_eq!(conflict(&a, dep).kind, ConflictKind::Dependency);
    let res = [
        resolve(dep, Choice::TakeSource),
        resolve("column:16420:3", Choice::TakeSource),
        resolve("column:16420:2", Choice::KeepTarget),
    ];
    assert_eq!(
        merged(&a, &res, &b, &t, &s),
        Err(MergeError::BrokenReference {
            item: ItemId("constraint:name:table:16430/orders_pair_fk".to_string()),
            missing: vec![ItemId("column:16420:2".to_string())],
        })
    );
    let res = [
        resolve(dep, Choice::TakeSource),
        resolve("column:16420:3", Choice::TakeSource),
        resolve("column:16420:2", Choice::TakeSource),
    ];
    assert!(merged(&a, &res, &b, &t, &s).is_ok());
}

#[test]
fn source_references_outside_the_model_get_new_oid() {
    // A source-only table owned by a role outside the model, with a column of a user type the
    // model doesn't cover (a domain): neither OID is the target's.
    let b = base();
    let mut s = b.clone();
    let mut audit = table(
        17000,
        "public",
        "audit",
        vec![Column {
            type_oid: Oid(17500),
            ..col(1, "v", "public.positive")
        }],
    );
    audit.owner = nr(17600, "outsider");
    audit.columns.push(col(2, "n", "integer"));
    audit.columns[1].type_oid = Oid(23);
    s.tables.push(audit);
    let a = run(&b, &b, &s);
    let m = merged(&a, &[], &b, &b, &s).unwrap();
    let audit = mt(&m.model, "audit");
    assert_eq!(
        audit.owner,
        NamedRole {
            oid: NEW_OID,
            name: "outsider".to_string()
        }
    );
    assert_eq!(mc(audit, "v").unwrap().type_oid, NEW_OID);
    // Built-in OIDs are the same in every database.
    assert_eq!(mc(audit, "n").unwrap().type_oid, Oid(23));
}

// ---- T14 fixes ----

fn reported_role<'a>(a: &'a Analysis, name: &str) -> Option<&'a ReportedChange> {
    a.reported
        .iter()
        .find(|r| r.object.kind == ReportedKind::Role && r.object.name == name)
}

fn mrole<'a>(m: &'a SchemaModel, name: &str) -> &'a Role {
    m.roles
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no role {name} in {:#?}", m.roles))
}

// T14 I4: an index left INVALID on the source (an interrupted concurrent build) is reported,
// never merged: not as a create, and not as a change of the base's valid index.
#[test]
fn invalid_source_index_is_reported_not_merged() {
    let mut b = base();
    tbl(&mut b, "users").indexes = vec![index(16700, "users_email_idx", "email")];
    let t = b.clone();
    let mut s = b.clone();
    let users = tbl(&mut s, "users");
    users.indexes[0].valid = false;
    users.indexes.push(Index {
        valid: false,
        ..index(16701, "users_code_idx", "code")
    });
    let a = run(&b, &t, &s);
    assert!(a.items.is_empty(), "{:#?}", ids(&a));
    let names: Vec<(&str, ReportedChangeKind)> = a
        .reported
        .iter()
        .filter(|r| r.object.kind == ReportedKind::InvalidIndex)
        .map(|r| (r.object.name.as_str(), r.change))
        .collect();
    assert_eq!(
        names,
        [
            ("users_code_idx", ReportedChangeKind::Created),
            ("users_email_idx", ReportedChangeKind::Altered)
        ]
    );
    let m = merged(&a, &[], &b, &t, &s).unwrap().model;
    let ix: Vec<(&str, bool)> = mt(&m, "users")
        .indexes
        .iter()
        .map(|i| (i.name.as_str(), i.valid))
        .collect();
    assert_eq!(ix, [("users_email_idx", true)]);

    // `valid` isn't part of a change: a valid source index next to a target one left invalid
    // is no change of either side's definition.
    let mut t2 = b.clone();
    tbl(&mut t2, "users").indexes[0].valid = false;
    let a = run(&b, &t2, &b);
    assert!(a.items.is_empty(), "{:#?}", ids(&a));
}

// T14 M2: own SQL exempts a reference the merged model can't resolve in the assembly too, not
// only in the integrity check.
#[test]
fn own_sql_exempts_references_in_the_merged_model_assembly() {
    let b = base();
    let mut t = b.clone();
    let users = tbl(&mut t, "users");
    users.columns.retain(|c| c.name == "id");
    users.constraints.clear();
    let mut s = b.clone();
    let users = tbl(&mut s, "users");
    column(users, "code").comment = Some("s".to_string());
    column(users, "email").comment = Some("s".to_string());
    tbl(&mut s, "orders").constraints.push(fk(
        16493,
        "orders_pair_fk",
        vec![2, 1],
        (16420, "users"),
        vec![3, 2],
        "FOREIGN KEY (user_id, id) REFERENCES public.users(code, email)",
    ));
    let a = run(&b, &t, &s);
    let dep = "dependency:constraint:name:table:16430/orders_pair_fk";
    let res = [
        resolve(dep, Choice::TakeSource),
        resolve("column:16420:3", Choice::TakeSource),
        resolve(
            "column:16420:2",
            Choice::OwnSql("ALTER TABLE public.users ADD email text".to_string()),
        ),
    ];
    let m = merged(&a, &res, &b, &t, &s).expect("own SQL takes part: the rehearsal checks it");
    assert!(
        mt(&m.model, "orders")
            .constraints
            .iter()
            .any(|k| k.name == "orders_pair_fk")
    );
}

// T14 M3: an index part that doesn't parse loses only its own references.
#[test]
fn index_references_survive_an_unparsable_part() {
    let b = base();
    let mut m = b.clone();
    tbl(&mut m, "users").indexes = vec![Index {
        keys: vec!["((( broken".to_string(), "email".to_string()],
        include: vec!["code".to_string()],
        predicate: Some("(id > 0)".to_string()),
        ..index(16700, "users_idx", "x")
    }];
    let map = IdentityMap::self_mapped(&b);
    let ctx = index::Ctx::new(&m, index::Who::Base, &map);
    let side = index::build(&ctx);
    let entry = side
        .get(&ItemId("index:name:table:16420/users_idx".to_string()))
        .unwrap();
    let mut refs: Vec<&str> = entry.text_refs.iter().map(|r| r.0.as_str()).collect();
    refs.sort();
    assert_eq!(refs, ["column:16420:1", "column:16420:2", "column:16420:3"]);
}

// T14 I2 (E31): a target object whose text names something the merge renames keeps the
// target's text in M; it's listed in `follows_rename`, so the compare checks it against what
// the rehearsal produced (Postgres rewrote its text), not against M's text.
#[test]
fn target_objects_naming_a_source_rename_follow_the_rehearsal() {
    let b = base();
    let mut t = b.clone();
    tbl(&mut t, "users").indexes = vec![index(16700, "users_email_idx", "email")];
    tbl(&mut t, "orders").policies[0].using = Some("(user_id > 0)".to_string());
    let mut s = b.clone();
    column(tbl(&mut s, "users"), "email").name = "mail".to_string();
    let a = run(&b, &t, &s);
    assert!(a.conflicts.is_empty(), "{:#?}", a.conflicts);
    let m = merged(&a, &[], &b, &t, &s).unwrap();
    assert_eq!(
        m.follows_rename,
        [FollowsRename {
            key: key(
                ObjKind::Index,
                Some("public"),
                Some("users"),
                "users_email_idx"
            ),
            renames: vec![NameChange {
                from: "email".to_string(),
                to: "mail".to_string(),
            }],
        }]
    );
    // The other direction stays a dependency conflict (E12a): the source's new index names the
    // column the target renamed.
    let mut t2 = b.clone();
    column(tbl(&mut t2, "users"), "email").name = "mail".to_string();
    let mut s2 = b.clone();
    tbl(&mut s2, "users").indexes = vec![index(16700, "users_email_idx", "email")];
    let a = run(&b, &t2, &s2);
    conflict(&a, "dependency:index:name:table:16420/users_email_idx");
    // Nothing renamed: nothing listed.
    let a = run(&b, &t, &b);
    assert!(merged(&a, &[], &b, &t, &b).unwrap().follows_rename.is_empty());
}

// T14 I3 / E30: role changes the roles path can't make are reported "apply by hand", never
// merged; the merged model keeps the target's values.
#[test]
fn role_changes_outside_u2_are_reported() {
    let b = base();
    let t = b.clone();
    let mut s = b.clone();
    let app = s.roles.iter_mut().find(|r| r.name == "app").unwrap();
    app.other_attributes = vec!["CREATEDB".to_string()];
    app.comment = Some("the app".to_string());
    app.member_of.push(Membership {
        role: nr(4200, "pg_read_all_data"),
        grantor: pg(),
        admin: false,
        inherit: true,
        set: true,
    });
    let owner = s.roles.iter_mut().find(|r| r.name == "postgres").unwrap();
    owner.connection_limit = 5;
    owner.comment = Some("me".to_string());
    // A new role with an attribute the roles path can't set.
    s.roles.push(Role {
        other_attributes: vec!["REPLICATION".to_string()],
        ..role(16520, "repl")
    });
    let a = run(&b, &t, &s);
    assert_eq!(
        ids(&a),
        [("role:name:repl".to_string(), Outcome::Apply)],
        "only the new role merges"
    );
    let app = reported_role(&a, "app").expect("app's changes are reported");
    assert_eq!(app.change, ReportedChangeKind::Altered);
    let sql = app.object.definition.as_deref().unwrap();
    assert!(sql.contains("ALTER ROLE app CREATEDB"), "{sql}");
    assert!(sql.contains("COMMENT ON ROLE app IS 'the app'"), "{sql}");
    assert!(sql.contains("GRANT pg_read_all_data TO app"), "{sql}");
    let owner = reported_role(&a, "postgres").expect("the owner's changes are reported");
    let sql = owner.object.definition.as_deref().unwrap();
    assert!(sql.contains("CONNECTION LIMIT 5"), "{sql}");
    let repl = reported_role(&a, "repl").expect("the new role's attribute is reported");
    assert_eq!(repl.change, ReportedChangeKind::Created);
    assert!(
        repl.object
            .definition
            .as_deref()
            .unwrap()
            .contains("ALTER ROLE repl REPLICATION")
    );

    let m = merged(&a, &[], &b, &t, &s).unwrap().model;
    assert_eq!(mrole(&m, "app"), mrole(&normalized(&t), "app"));
    assert_eq!(mrole(&m, "postgres").connection_limit, -1);
    assert_eq!(mrole(&m, "postgres").comment, None);
    let repl = mrole(&m, "repl");
    assert!(repl.other_attributes.is_empty());
    assert_eq!(repl.oid, NEW_OID);
}

// T14 I3 / E28: owner-level, created or turned on, needs an acknowledgement like BYPASSRLS.
#[test]
fn owner_level_is_flagged_security() {
    let b = base();
    let mut s = b.clone();
    let superuser = Membership {
        role: nr(4300, OWNER_LEVEL_ROLE),
        grantor: pg(),
        admin: false,
        inherit: true,
        set: true,
    };
    s.roles
        .iter_mut()
        .find(|r| r.name == "app")
        .unwrap()
        .member_of
        .push(superuser.clone());
    s.roles.push(Role {
        member_of: vec![superuser],
        ..role(16520, "boss")
    });
    s.roles.push(role(16521, "plain"));
    let a = run(&b, &b, &s);
    assert!(item(&a, "role:16400").security, "turned on");
    assert!(item(&a, "role:name:boss").security, "created with it");
    assert!(!item(&a, "role:name:plain").security);
    assert!(a.reported.is_empty(), "owner-level merges: {:#?}", a.reported);
}

/// The invariant behind T14 I3 (E29/E30) and the fix round's I2, exhaustively over every
/// combination of source changes to one role, with the target changing the same role's
/// unmerged parts or not:
/// - a change inside U2 (the roles path's attributes, memberships by role, owner-level, the
///   name) is an applied item with the source's values in M;
/// - a change outside it (other attributes, the comment, a membership the roles path refuses, a
///   membership's options, a new membership with options other than the defaults) is reported
///   with its SQL, and M keeps the target's values for it;
/// - the reported SQL makes only what the source changed: it never undoes the target's own
///   changes;
/// - BYPASSRLS and owner-level gained are flagged security (P9, E28).
#[test]
fn role_changes_split_into_merged_and_reported_exhaustively() {
    let mut b = rich();
    b.roles.push(role(16402, "web"));
    let id = "role:16401";
    type Mutation = fn(&mut Role);
    // (name, change, merged, reported)
    let all: [(&str, Mutation, bool, bool); 13] = [
        ("login", |r| r.login = false, true, false),
        ("inherit", |r| r.inherit = false, true, false),
        ("connection_limit", |r| r.connection_limit = 3, true, false),
        (
            "valid_until",
            |r| r.valid_until = Some("2030-01-01 00:00:00+00".to_string()),
            true,
            false,
        ),
        ("bypass_rls", |r| r.bypass_rls = true, true, false),
        (
            // A new membership with the roles path's options (INHERIT follows the member).
            "member_of",
            |r| {
                let inherit = r.inherit;
                r.member_of.push(Membership {
                    role: nr(16402, "web"),
                    grantor: pg(),
                    admin: false,
                    inherit,
                    set: true,
                })
            },
            true,
            false,
        ),
        (
            "owner_level",
            |r| {
                r.member_of.push(Membership {
                    role: nr(4300, OWNER_LEVEL_ROLE),
                    grantor: pg(),
                    admin: false,
                    inherit: true,
                    set: true,
                })
            },
            true,
            false,
        ),
        ("name", |r| r.name = "auditors".to_string(), true, false),
        (
            "other_attributes",
            |r| r.other_attributes = vec!["CREATEDB".to_string()],
            false,
            true,
        ),
        ("comment", |r| r.comment = Some("c".to_string()), false, true),
        (
            "reserved membership",
            |r| {
                r.member_of.push(Membership {
                    role: nr(4200, "pg_read_all_data"),
                    grantor: pg(),
                    admin: false,
                    inherit: true,
                    set: true,
                })
            },
            false,
            true,
        ),
        ("membership options", |r| r.member_of[0].admin = true, false, true),
        (
            // A new membership with options the roles path doesn't make: merged by role, its
            // options reported.
            "new membership options",
            |r| {
                r.member_of.push(Membership {
                    role: nr(16403, "app2"),
                    grantor: pg(),
                    admin: true,
                    inherit: false,
                    set: false,
                })
            },
            true,
            true,
        ),
    ];
    b.roles.push(role(16403, "app2"));
    // The target's own changes to the role's unmerged parts.
    let target_changes: fn(&mut Role) = |r| {
        r.other_attributes = vec!["REPLICATION".to_string()];
        r.comment = Some("t".to_string());
        r.member_of.push(Membership {
            role: nr(4201, "pg_monitor"),
            grantor: pg(),
            admin: false,
            inherit: true,
            set: true,
        });
    };
    for target_changed in [false, true] {
        let mut t = b.clone();
        if target_changed {
            target_changes(t.roles.iter_mut().find(|r| r.oid == Oid(16401)).unwrap());
        }
        let target_auditor = mrole(&normalized(&t), "auditor").clone();
        for mask in 0u32..(1 << all.len()) {
            let chosen: Vec<&(&str, Mutation, bool, bool)> = all
                .iter()
                .enumerate()
                .filter(|(k, _)| mask & (1 << k) != 0)
                .map(|(_, m)| m)
                .collect();
            let mut s = b.clone();
            let r = s.roles.iter_mut().find(|r| r.oid == Oid(16401)).unwrap();
            for (_, f, _, _) in &chosen {
                f(r);
            }
            let source_auditor = r.clone();
            let names: Vec<&str> = chosen.iter().map(|c| c.0).collect();
            let ctx = format!("{names:?}, target changed: {target_changed}");
            let merged_part = chosen.iter().any(|c| c.2);
            let reported_part = chosen.iter().any(|c| c.3);
            let a = run(&b, &t, &s);
            let found = a.items.iter().find(|i| i.id.0 == id);
            assert_eq!(found.is_some(), merged_part, "{ctx}: {:#?}", ids(&a));
            if let Some(i) = found {
                assert_eq!(i.outcome, Outcome::Apply, "{ctx}");
                let gains = names.contains(&"bypass_rls") || names.contains(&"owner_level");
                assert_eq!(i.security, gains, "{ctx}");
            }
            let report = reported_role(&a, &source_auditor.name);
            assert_eq!(report.is_some(), reported_part, "{ctx}: {:#?}", a.reported);
            if let Some(report) = report {
                let sql = report.object.definition.as_deref().unwrap();
                // Only what the source changed; never the target's own changes undone.
                assert!(!sql.contains("REPLICATION"), "{ctx}: {sql}");
                assert!(!sql.contains("pg_monitor"), "{ctx}: {sql}");
                assert!(!sql.contains("REVOKE"), "{ctx}: {sql}");
                assert_eq!(sql.contains("COMMENT"), names.contains(&"comment"), "{ctx}: {sql}");
                assert_eq!(
                    sql.contains("CREATEDB"),
                    names.contains(&"other_attributes"),
                    "{ctx}: {sql}"
                );
                assert_eq!(
                    sql.contains("pg_read_all_data"),
                    names.contains(&"reserved membership"),
                    "{ctx}: {sql}"
                );
                assert_eq!(
                    sql.contains("GRANT app2"),
                    names.contains(&"new membership options"),
                    "{ctx}: {sql}"
                );
            }
            let m = merged(&a, &[], &b, &t, &s).unwrap().model;
            let got = mrole(&m, &source_auditor.name);
            // Outside U2: the target's.
            assert_eq!(got.other_attributes, target_auditor.other_attributes, "{ctx}");
            assert_eq!(got.comment, target_auditor.comment, "{ctx}");
            assert!(
                !got.member_of.iter().any(|x| x.role.name == "pg_read_all_data"),
                "{ctx}"
            );
            assert_eq!(
                got.member_of.iter().any(|x| x.role.name == "pg_monitor"),
                target_changed,
                "{ctx}"
            );
            let app = got.member_of.iter().find(|x| x.role.name == "app").unwrap();
            assert_eq!(app, &target_auditor.member_of[0], "{ctx}");
            // Inside U2: the source's.
            assert_eq!(
                (
                    got.login,
                    got.inherit,
                    got.connection_limit,
                    &got.valid_until,
                    got.bypass_rls,
                    got.owner_level(),
                    got.member_of.iter().any(|x| x.role.name == "web"),
                    got.member_of.iter().any(|x| x.role.name == "app2"),
                ),
                (
                    source_auditor.login,
                    source_auditor.inherit,
                    source_auditor.connection_limit,
                    &source_auditor.valid_until,
                    source_auditor.bypass_rls,
                    source_auditor.owner_level(),
                    source_auditor.member_of.iter().any(|x| x.role.name == "web"),
                    source_auditor.member_of.iter().any(|x| x.role.name == "app2"),
                ),
                "{ctx}"
            );
            assert_eq!(got.oid, Oid(16401), "{ctx}: the target's role, renamed or not");
        }
    }
}

// Fix round I1: the owner administers the roles it created (E22); a rename of one of them
// reads back without drift (the expected owner names it as `M` does).
#[test]
fn a_role_rename_the_owner_administers_reads_back_without_drift() {
    let mut b = base();
    b.roles
        .iter_mut()
        .find(|r| r.name == "postgres")
        .unwrap()
        .member_of = owner_grants(&nr(16400, "app"), &pg());
    // These fixtures give `postgres` OID 10, the bootstrap superuser's; on a compute that's
    // `cloud_admin`, which the model leaves out. Keep the two apart here.
    for r in &mut b.roles {
        for ms in &mut r.member_of {
            if ms.grantor.name == "cloud_admin" {
                ms.grantor.oid = Oid(1);
            }
        }
    }
    let t = b.clone();
    let renamed = |m: &SchemaModel| {
        m.rename_roles(|oid, _| (oid == Oid(16400)).then(|| "web".to_string()))
    };
    let s = renamed(&b);
    let a = run(&b, &t, &s);
    let m = merged(&a, &[], &b, &t, &s).unwrap();
    let expected = crate::compare::expected_model(&t, &m, &SchemaModel::empty(17));
    let mut actual = renamed(&t);
    actual.normalize();
    assert_eq!(
        crate::compare::compare(&expected, &actual, &[]),
        Vec::new(),
        "{:#?}",
        mrole(&expected, "postgres")
    );
}
