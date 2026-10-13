//! Request and response types of the table API: structured schema changes (`Operation`), the
//! scoped catalog reads (objects, types) and the row helpers (default row, single cell), plus the
//! failure classification the change and row endpoints share.
//!
//! The serde contract: enums are externally tagged with snake_case names (no `untagged`, `tag`
//! or `flatten`), structs refuse unknown fields, and every optional or list field has a default.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::api_error::ErrorCode;

/// A hint carried by the DDL guard's refusal on a protected branch.
pub const BRANCH_PROTECTED_HINT: &str = "chelabase:branch_protected";
/// The marker role the guard's event triggers look for.
pub const SCHEMA_LOCKED_ROLE: &str = "chelabase_schema_locked";
/// The schema holding the guard's objects.
pub const GUARD_SCHEMA: &str = "chelabase_guard";

fn yes() -> bool {
    true
}

/// Maps a Postgres failure (SQLSTATE and the error's hint) to the HTTP status and error code
/// the change and row endpoints answer with.
pub fn classify_failure(sqlstate: &str, hint: Option<&str>) -> (u16, ErrorCode) {
    if hint == Some(BRANCH_PROTECTED_HINT) {
        return (409, ErrorCode::BranchProtected);
    }
    match sqlstate {
        "42501" => (403, ErrorCode::PermissionDenied),
        "55P03" => (409, ErrorCode::LockTimeout),
        "57014" => (504, ErrorCode::Timeout),
        // Deadlocks and serialization failures: retryable, like the other conflicts.
        "23505" | "23503" | "23P01" | "40P01" | "40001" => (409, ErrorCode::Conflict),
        "23502" | "23514" => (400, ErrorCode::ConstraintViolation),
        s if s.starts_with("22") => (400, ErrorCode::ConstraintViolation),
        _ => (400, ErrorCode::SchemaChangeFailed),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TypeSpec {
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub modifiers: Vec<i32>,
    #[serde(default)]
    pub array: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CollationRef {
    pub schema: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct OperatorRef {
    pub schema: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum IdentityKind {
    Always,
    ByDefault,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ColumnSpec {
    pub name: String,
    pub data_type: TypeSpec,
    #[serde(default)]
    pub not_null: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub identity: Option<IdentityKind>,
    #[serde(default)]
    pub generated: Option<String>,
    #[serde(default)]
    pub collation: Option<CollationRef>,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum DefaultChange {
    Set(String),
    Drop,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum IdentityChange {
    Add(IdentityKind),
    Set(IdentityKind),
    Drop,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum CommentChange {
    Set(String),
    Clear,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ColumnChange {
    #[serde(default)]
    pub data_type: Option<TypeSpec>,
    #[serde(default)]
    pub using: Option<String>,
    #[serde(default)]
    pub collation: Option<CollationRef>,
    #[serde(default)]
    pub default: Option<DefaultChange>,
    #[serde(default)]
    pub not_null: Option<bool>,
    #[serde(default)]
    pub identity: Option<IdentityChange>,
    #[serde(default)]
    pub comment: Option<CommentChange>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum FkAction {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ExclusionElement {
    pub column: String,
    pub operator: OperatorRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ConstraintKindSpec {
    PrimaryKey {
        columns: Vec<String>,
    },
    Unique {
        columns: Vec<String>,
        #[serde(default)]
        nulls_not_distinct: bool,
    },
    ForeignKey {
        columns: Vec<String>,
        references: TableRef,
        referenced_columns: Vec<String>,
        #[serde(default)]
        on_delete: FkAction,
        #[serde(default)]
        on_update: FkAction,
        #[serde(default)]
        deferrable: bool,
        #[serde(default)]
        initially_deferred: bool,
    },
    Check {
        expression: String,
    },
    Exclusion {
        method: String,
        elements: Vec<ExclusionElement>,
        #[serde(default)]
        predicate: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ConstraintSpec {
    #[serde(default)]
    pub name: Option<String>,
    pub kind: ConstraintKindSpec,
    #[serde(default)]
    pub not_valid: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum NullsOrder {
    First,
    Last,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IndexKey {
    Column {
        name: String,
        #[serde(default)]
        descending: bool,
        #[serde(default)]
        nulls: Option<NullsOrder>,
    },
    Expression {
        sql: String,
        #[serde(default)]
        descending: bool,
        #[serde(default)]
        nulls: Option<NullsOrder>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct IndexSpec {
    pub table: TableRef,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub nulls_not_distinct: bool,
    #[serde(default)]
    pub keys: Vec<IndexKey>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub concurrently: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PolicyCommandSpec {
    #[default]
    All,
    Select,
    Insert,
    Update,
    Delete,
}

/// `roles` empty means PUBLIC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    pub name: String,
    #[serde(default)]
    pub command: PolicyCommandSpec,
    #[serde(default = "yes")]
    pub permissive: bool,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub using: Option<String>,
    #[serde(default)]
    pub with_check: Option<String>,
}

/// A table or column privilege. Its OpenAPI name is `TablePrivilege`: the data API's schema
/// has a `Privilege` of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema), schema(as = TablePrivilege))]
#[serde(rename_all = "snake_case")]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    References,
    Trigger,
    Maintain,
}

/// JSON: `"public"` or `{"role": "anon"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Grantee {
    Public,
    Role(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TableGrant {
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub privileges: Vec<Privilege>,
    pub grantee: Grantee,
    #[serde(default)]
    pub with_grant_option: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RlsSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub forced: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateTable {
    pub table: TableRef,
    pub columns: Vec<ColumnSpec>,
    #[serde(default)]
    pub primary_key: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<ConstraintSpec>,
    #[serde(default)]
    pub rls: Option<RlsSpec>,
    #[serde(default)]
    pub policies: Vec<PolicySpec>,
    #[serde(default)]
    pub grants: Vec<TableGrant>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum EnumPosition {
    Before(String),
    After(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    CreateTable(CreateTable),
    RenameTable {
        table: TableRef,
        to: String,
    },
    SetTableSchema {
        table: TableRef,
        schema: String,
    },
    DropTable {
        table: TableRef,
    },
    SetTableComment {
        table: TableRef,
        #[serde(default)]
        comment: Option<String>,
    },
    SetTableOwner {
        table: TableRef,
        owner: String,
    },
    AddColumn {
        table: TableRef,
        column: ColumnSpec,
    },
    AlterColumn {
        table: TableRef,
        column: String,
        change: ColumnChange,
    },
    RenameColumn {
        table: TableRef,
        column: String,
        to: String,
    },
    DropColumn {
        table: TableRef,
        column: String,
    },
    AddConstraint {
        table: TableRef,
        constraint: ConstraintSpec,
    },
    ValidateConstraint {
        table: TableRef,
        name: String,
    },
    RenameConstraint {
        table: TableRef,
        name: String,
        to: String,
    },
    DropConstraint {
        table: TableRef,
        name: String,
    },
    CreateIndex(IndexSpec),
    DropIndex {
        schema: String,
        name: String,
        #[serde(default)]
        concurrently: bool,
    },
    RenameIndex {
        schema: String,
        name: String,
        to: String,
    },
    SetRls {
        table: TableRef,
        enabled: bool,
        forced: bool,
    },
    CreatePolicy {
        table: TableRef,
        policy: PolicySpec,
    },
    AlterPolicy {
        table: TableRef,
        name: String,
        #[serde(default)]
        roles: Option<Vec<String>>,
        #[serde(default)]
        using: Option<String>,
        #[serde(default)]
        with_check: Option<String>,
    },
    RenamePolicy {
        table: TableRef,
        name: String,
        to: String,
    },
    DropPolicy {
        table: TableRef,
        name: String,
    },
    Grant {
        table: TableRef,
        grant: TableGrant,
    },
    Revoke {
        table: TableRef,
        grant: TableGrant,
    },
    CreateEnum {
        schema: String,
        name: String,
        values: Vec<String>,
    },
    AddEnumValue {
        schema: String,
        name: String,
        value: String,
        #[serde(default)]
        position: Option<EnumPosition>,
    },
    RenameEnumValue {
        schema: String,
        name: String,
        from: String,
        to: String,
    },
    RenameEnum {
        schema: String,
        name: String,
        to: String,
    },
    DropEnum {
        schema: String,
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CascadeConfirm {
    pub dependents_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct SchemaChangeRequest {
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub operations: Vec<Operation>,
    #[serde(default)]
    pub cascade: Option<CascadeConfirm>,
    #[serde(default)]
    pub lock_timeout_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum LockLevel {
    None,
    ShareUpdateExclusive,
    Share,
    ShareRowExclusive,
    AccessExclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    LargeTableRewrite,
    Concurrently,
    AfterSkipped,
    /// The drop's target is already gone: an earlier drop of the same request removed it (by
    /// cascading to it, or as one of its own parts). Not a failure (a dry run can still succeed).
    AlreadyDropped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct StatementResult {
    pub sql: String,
    pub op_index: usize,
    pub executed: bool,
    #[serde(default)]
    pub skipped: Option<SkipReason>,
    #[serde(default)]
    pub notices: Vec<String>,
    #[serde(default)]
    pub rewrites_table: bool,
    pub lock: LockLevel,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct Dependent {
    pub kind: String,
    #[serde(default)]
    pub schema: Option<String>,
    pub name: String,
    #[serde(default)]
    pub parent: Option<String>,
    /// Postgres's own description (`pg_describe_object`), the text its
    /// `drop cascades to …` notices print.
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct Dependents {
    #[serde(default)]
    pub objects: Vec<Dependent>,
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct SchemaChangeResponse {
    pub dry_run: bool,
    pub committed: bool,
    #[serde(default)]
    pub statements: Vec<StatementResult>,
    #[serde(default)]
    pub dependents: Option<Dependents>,
    #[serde(default)]
    pub would_succeed: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ObjectSummary {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    pub oid: u32,
    pub owner: String,
    #[serde(default)]
    pub comment: Option<String>,
    pub rls_enabled: bool,
    pub rls_forced: bool,
    pub data_api_exposed: bool,
    #[serde(default)]
    pub row_estimate: Option<i64>,
    pub total_bytes: i64,
    pub editable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ObjectList {
    #[serde(default)]
    pub objects: Vec<ObjectSummary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum TypeKind {
    Base,
    Enum,
    Domain,
    Composite,
    Range,
    Multirange,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TypeEntry {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub kind: TypeKind,
    pub category: String,
    pub preferred: bool,
    pub takes_modifiers: bool,
    #[serde(default)]
    pub array_oid: Option<u32>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub base_type_oid: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct TypeCatalog {
    #[serde(default)]
    pub types: Vec<TypeEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DefaultRowRequest {
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct DefaultRow {
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub row: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CellRequest {
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub key: BTreeMap<String, serde_json::Value>,
    pub column: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CellValue {
    #[serde(default)]
    pub value: Option<String>,
    pub bytes: i64,
    pub data_type: String,
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_error::ErrorCode;
    use serde_json::json;

    #[test]
    fn operations_are_externally_tagged_snake_case() {
        let op: Operation = serde_json::from_value(json!({
            "rename_column": { "table": { "schema": "public", "name": "notes" }, "column": "body", "to": "text" }
        }))
        .unwrap();
        assert_eq!(
            op,
            Operation::RenameColumn {
                table: TableRef { schema: "public".into(), name: "notes".into() },
                column: "body".into(),
                to: "text".into()
            }
        );
        let back = serde_json::to_value(&op).unwrap();
        assert!(back.get("rename_column").is_some(), "{back}");
    }

    #[test]
    fn a_create_table_needs_only_its_name_and_columns() {
        let op: Operation = serde_json::from_value(json!({ "create_table": {
            "table": { "schema": "public", "name": "t" },
            "columns": [{ "name": "id", "data_type": { "schema": "pg_catalog", "name": "int8" } }]
        }}))
        .unwrap();
        let Operation::CreateTable(c) = op else { panic!("{op:?}") };
        assert_eq!(
            c.columns[0].data_type,
            TypeSpec { schema: "pg_catalog".into(), name: "int8".into(), modifiers: vec![], array: false }
        );
        assert!(!c.columns[0].not_null && c.primary_key.is_empty() && c.rls.is_none() && c.owner.is_none());
    }

    #[test]
    fn unknown_fields_are_refused() {
        let r: Result<SchemaChangeRequest, _> = serde_json::from_value(json!({ "operations": [], "dryrun": true }));
        assert!(r.is_err());
        let r: Result<Operation, _> =
            serde_json::from_value(json!({ "drop_table": { "table": { "schema": "a", "name": "b", "x": 1 } } }));
        assert!(r.is_err());
    }

    #[test]
    fn grantee_and_policy_defaults() {
        assert_eq!(serde_json::from_value::<Grantee>(json!("public")).unwrap(), Grantee::Public);
        assert_eq!(
            serde_json::from_value::<Grantee>(json!({ "role": "anon" })).unwrap(),
            Grantee::Role("anon".into())
        );
        let p: PolicySpec = serde_json::from_value(json!({ "name": "p" })).unwrap();
        assert_eq!((p.command, p.permissive, p.roles.len()), (PolicyCommandSpec::All, true, 0));
        let fk: ConstraintKindSpec = serde_json::from_value(json!({ "foreign_key": {
            "columns": ["a"], "references": { "schema": "s", "name": "t" }, "referenced_columns": ["id"]
        }}))
        .unwrap();
        let ConstraintKindSpec::ForeignKey { on_delete, on_update, deferrable, .. } = fk else { panic!() };
        assert_eq!((on_delete, on_update, deferrable), (FkAction::NoAction, FkAction::NoAction, false));
    }

    #[test]
    fn changes_and_positions_round_trip() {
        for v in [json!({ "set": "0" }), json!("drop")] {
            let d: DefaultChange = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(serde_json::to_value(&d).unwrap(), v);
        }
        let p: EnumPosition = serde_json::from_value(json!({ "before": "ok" })).unwrap();
        assert_eq!(p, EnumPosition::Before("ok".into()));
        let i: IdentityChange = serde_json::from_value(json!({ "add": "by_default" })).unwrap();
        assert_eq!(i, IdentityChange::Add(IdentityKind::ByDefault));
    }

    #[test]
    fn dependents_sort_by_kind_schema_name_parent() {
        let d = |k: &str, n: &str| Dependent { kind: k.into(), schema: Some("public".into()), name: n.into(), parent: None, description: format!("{k} {n}") };
        let mut v = [d("view", "b"), d("constraint", "z"), d("view", "a")];
        v.sort();
        assert_eq!(v.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["z", "a", "b"]);
    }

    #[test]
    fn failures_classify_per_p8() {
        let c = |s: &str, h: Option<&str>| classify_failure(s, h);
        assert_eq!(c("42501", Some(BRANCH_PROTECTED_HINT)), (409, ErrorCode::BranchProtected));
        assert_eq!(c("42501", None), (403, ErrorCode::PermissionDenied));
        assert_eq!(c("42501", Some("some other hint")), (403, ErrorCode::PermissionDenied));
        assert_eq!(c("55P03", None), (409, ErrorCode::LockTimeout));
        assert_eq!(c("57014", None), (504, ErrorCode::Timeout));
        for s in ["23505", "23503", "23P01"] {
            assert_eq!(c(s, None), (409, ErrorCode::Conflict), "{s}");
        }
        for s in ["23502", "23514", "22001", "22P02"] {
            assert_eq!(c(s, None), (400, ErrorCode::ConstraintViolation), "{s}");
        }
        // End-of-step review, finding 7: deadlocks and serialization failures are retryable.
        for s in ["40P01", "40001"] {
            assert_eq!(c(s, None), (409, ErrorCode::Conflict), "{s}");
        }
        for s in ["2BP01", "42P07", "42703", "XX000", ""] {
            assert_eq!(c(s, None), (400, ErrorCode::SchemaChangeFailed), "{s}");
        }
    }
}
