//! Model fixtures for the planner, export and compare tests: small, semantically valid objects
//! (unlike `model::sample`, which is a serialization fixture).

use uuid::Uuid;

use crate::analysis::{
    Analysis, Change, Item, ItemId, MergedModel, ObjKey, ObjKind, ObjSnapshot, Outcome,
};
use crate::model::*;
use crate::plan::{Phase, Plan, PlanOptions, Step};

pub(crate) fn named(oid: u32, name: &str) -> NamedRole {
    NamedRole {
        oid: Oid(oid),
        name: name.to_string(),
    }
}

pub(crate) fn postgres() -> NamedRole {
    named(10, "postgres")
}

pub(crate) fn rref(oid: u32, name: &str) -> RoleRef {
    RoleRef::Role {
        oid: Oid(oid),
        name: name.to_string(),
    }
}

pub(crate) fn grant(grantee: RoleRef, privilege: &str) -> Grant {
    Grant {
        grantor: postgres(),
        grantee,
        privilege: privilege.to_string(),
        grantable: false,
    }
}

/// A nullable column without default.
pub(crate) fn col(attnum: i16, name: &str, data_type: &str) -> Column {
    Column {
        attnum,
        name: name.to_string(),
        data_type: data_type.to_string(),
        type_oid: Oid(23),
        collation: None,
        default: None,
        not_null: false,
        identity: None,
        generated: None,
        comment: None,
        grants: Vec::new(),
    }
}

pub(crate) fn not_null(mut c: Column) -> Column {
    c.not_null = true;
    c
}

/// A table owned by `postgres`, without grants.
pub(crate) fn table(oid: u32, schema: &str, name: &str, columns: Vec<Column>) -> Table {
    Table {
        oid: Oid(oid),
        schema: schema.to_string(),
        name: name.to_string(),
        owner: postgres(),
        comment: None,
        rls_enabled: false,
        rls_forced: false,
        unlogged: false,
        options: Vec::new(),
        columns,
        constraints: Vec::new(),
        indexes: Vec::new(),
        policies: Vec::new(),
        grants: Vec::new(),
    }
}

pub(crate) fn schema(oid: u32, name: &str) -> Schema {
    Schema {
        oid: Oid(oid),
        name: name.to_string(),
        owner: postgres(),
        comment: None,
        grants: Vec::new(),
    }
}

pub(crate) fn constraint(
    oid: u32,
    name: &str,
    kind: ConstraintKind,
    definition: &str,
    columns: &[i16],
) -> Constraint {
    Constraint {
        oid: Oid(oid),
        name: name.to_string(),
        kind,
        definition: definition.to_string(),
        columns: columns.to_vec(),
        references: None,
        deferrable: false,
        initially_deferred: false,
        validated: true,
    }
}

pub(crate) fn fk(
    oid: u32,
    name: &str,
    definition: &str,
    columns: &[i16],
    references: (&str, &str, &[i16]),
) -> Constraint {
    Constraint {
        references: Some(ForeignKeyTarget {
            table_oid: Oid(0),
            schema: references.0.to_string(),
            table: references.1.to_string(),
            columns: references.2.to_vec(),
        }),
        ..constraint(oid, name, ConstraintKind::ForeignKey, definition, columns)
    }
}

pub(crate) fn index(oid: u32, name: &str, keys: &[&str]) -> Index {
    Index {
        oid: Oid(oid),
        name: name.to_string(),
        method: "btree".to_string(),
        unique: false,
        nulls_not_distinct: false,
        keys: keys.iter().map(|k| k.to_string()).collect(),
        include: Vec::new(),
        predicate: None,
        options: Vec::new(),
        valid: true,
        comment: None,
    }
}

pub(crate) fn policy(oid: u32, name: &str, using: &str) -> Policy {
    Policy {
        oid: Oid(oid),
        name: name.to_string(),
        command: PolicyCommand::All,
        permissive: true,
        roles: vec![RoleRef::Public],
        using: Some(using.to_string()),
        with_check: None,
        comment: None,
    }
}

pub(crate) fn enum_type(oid: u32, schema: &str, name: &str, labels: &[(u32, &str)]) -> EnumType {
    EnumType {
        oid: Oid(oid),
        schema: schema.to_string(),
        name: name.to_string(),
        owner: postgres(),
        comment: None,
        labels: labels
            .iter()
            .map(|(oid, label)| EnumLabel {
                oid: Oid(*oid),
                label: label.to_string(),
            })
            .collect(),
    }
}

pub(crate) fn seq_params() -> SequenceParams {
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

pub(crate) fn sequence(oid: u32, schema: &str, name: &str) -> Sequence {
    Sequence {
        oid: Oid(oid),
        schema: schema.to_string(),
        name: name.to_string(),
        owner: postgres(),
        comment: None,
        params: seq_params(),
        owned_by: None,
        grants: Vec::new(),
    }
}

pub(crate) fn role(oid: u32, name: &str) -> Role {
    Role {
        oid: Oid(oid),
        name: name.to_string(),
        login: false,
        inherit: true,
        connection_limit: -1,
        valid_until: None,
        bypass_rls: false,
        member_of: Vec::new(),
        other_attributes: Vec::new(),
        comment: None,
    }
}

pub(crate) fn membership(oid: u32, name: &str) -> Membership {
    Membership {
        role: named(oid, name),
        grantor: postgres(),
        admin: false,
        inherit: true,
        set: true,
    }
}

/// A model with the `public` schema and these tables.
pub(crate) fn model(tables: Vec<Table>) -> SchemaModel {
    let mut m = SchemaModel::empty(17);
    m.schemas = vec![schema(2200, "public")];
    m.tables = tables;
    m
}

pub(crate) fn merged(model: SchemaModel) -> MergedModel {
    MergedModel {
        model,
        own_sql: Vec::new(),
        follows_rename: Vec::new(),
    }
}

pub(crate) fn no_analysis() -> Analysis {
    Analysis {
        items: Vec::new(),
        conflicts: Vec::new(),
        reported: Vec::new(),
    }
}

pub(crate) fn key(kind: ObjKind, schema: Option<&str>, table: Option<&str>, name: &str) -> ObjKey {
    ObjKey {
        kind,
        schema: schema.map(str::to_string),
        table: table.map(str::to_string),
        name: name.to_string(),
    }
}

/// A source-side change applied to the target.
pub(crate) fn applied(id: &str, key: ObjKey, change: Change) -> Item {
    Item {
        id: ItemId(id.to_string()),
        key,
        source: Some(change),
        target: None,
        outcome: Outcome::Apply,
        data_loss: false,
        security: false,
    }
}

/// A source-side rename (`Alter` whose name changes).
pub(crate) fn renamed(id: &str, key: ObjKey, before: ObjSnapshot, after: ObjSnapshot) -> Item {
    applied(
        id,
        key,
        Change::Alter {
            before: Box::new(before),
            after: Box::new(after),
        },
    )
}

pub(crate) fn opts() -> PlanOptions {
    PlanOptions {
        lock_timeout_ms: 5000,
        merge_id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
    }
}

pub(crate) fn sqls(plan: &Plan) -> Vec<&str> {
    plan.steps.iter().map(|s| s.sql.as_str()).collect()
}

/// The step whose SQL is exactly `sql`; panics with the plan's SQL when it's missing.
pub(crate) fn step<'p>(plan: &'p Plan, sql: &str) -> &'p Step {
    plan.steps
        .iter()
        .find(|s| s.sql == sql)
        .unwrap_or_else(|| panic!("no step {sql:?} in:\n{:#?}", sqls(plan)))
}

/// The position of the step whose SQL is exactly `sql`.
pub(crate) fn at(plan: &Plan, sql: &str) -> usize {
    plan.steps
        .iter()
        .position(|s| s.sql == sql)
        .unwrap_or_else(|| panic!("no step {sql:?} in:\n{:#?}", sqls(plan)))
}

pub(crate) fn phase_of(plan: &Plan, sql: &str) -> Phase {
    step(plan, sql).phase
}
