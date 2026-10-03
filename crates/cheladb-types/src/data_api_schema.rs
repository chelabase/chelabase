//! Wire types for the Data API schema document (`DataApiSchema`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The Data API schema: everything the asking roles can reach.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DataApiSchema {
    pub version: String,
    pub default_schema: String,
    pub schemas: BTreeMap<String, SchemaEntry>,
    /// The table app users are mirrored into, when the branch has one (left out otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_users: Option<AppUsersTable>,
}

/// The branch's app users table: where `cheladb.app_users_table` points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AppUsersTable {
    pub schema: String,
    pub table: String,
}

/// One schema's tables, functions (overloads by name) and enums (labels by name).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SchemaEntry {
    pub tables: BTreeMap<String, TableDesc>,
    pub functions: BTreeMap<String, Vec<FunctionDesc>>,
    pub enums: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
}

/// A table, view or foreign table. `privileges` (role to held privileges) is `null` unless asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TableDesc {
    pub kind: TableKind,
    pub rls: bool,
    pub security_invoker: bool,
    pub privileges: Option<BTreeMap<String, Vec<Privilege>>>,
    pub columns: BTreeMap<String, ColumnDesc>,
    /// The primary key's columns, in the key's order; left out when there is none (a view,
    /// or a table without a key).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary_key: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ColumnDesc {
    #[serde(rename = "type")]
    pub ty: String,
    pub array: bool,
    pub nullable: bool,
    pub has_default: bool,
    pub generated: bool,
    /// Qualified enum name (`schema.name`) when the column is an enum.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#enum: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FunctionDesc {
    pub args: Vec<ArgDesc>,
    pub returns: ReturnDesc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ArgDesc {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub array: bool,
    pub has_default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReturnDesc {
    #[serde(rename = "type")]
    pub ty: String,
    pub array: bool,
    pub set: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"version":"17.2","default_schema":"public","schemas":{"public":{"tables":{"Order":{"kind":"table","rls":false,"security_invoker":false,"privileges":{"anon":["select"],"authenticated":["select","insert","update","delete"]},"columns":{"id":{"type":"int8","array":false,"nullable":false,"has_default":true,"generated":true},"mood":{"type":"mood","array":false,"nullable":true,"has_default":false,"generated":false,"enum":"public.mood"}}},"v":{"kind":"materialized_view","rls":false,"security_invoker":true,"privileges":null,"columns":{}}},"functions":{"add_points":[{"args":[{"name":"user_id","type":"uuid","array":false,"has_default":false},{"name":"n","type":"int4","array":false,"has_default":true}],"returns":{"type":"void","array":false,"set":false}}]},"enums":{"mood":["sad","happy"]}}}}"#;

    #[test]
    fn wire_types_round_trip_todays_json() {
        let parsed: DataApiSchema = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), FIXTURE);
        let t = &parsed.schemas["public"].tables;
        assert_eq!(t["Order"].kind, TableKind::Table);
        assert_eq!(t["v"].kind, TableKind::MaterializedView);
        assert!(t["v"].privileges.is_none());
        assert_eq!(
            t["Order"].privileges.as_ref().unwrap()["anon"],
            [Privilege::Select]
        );
        assert_eq!(
            t["Order"].columns["mood"].r#enum.as_deref(),
            Some("public.mood")
        );
    }

    #[test]
    fn data_api_schema_omits_app_users_when_absent() {
        let parsed: DataApiSchema = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(parsed.app_users, None);
        assert!(!serde_json::to_string(&parsed).unwrap().contains("app_users"));
        let with = DataApiSchema {
            app_users: Some(AppUsersTable {
                schema: "public".into(),
                table: "users".into(),
            }),
            ..parsed
        };
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["app_users"], serde_json::json!({"schema":"public","table":"users"}));
        let back: DataApiSchema = serde_json::from_value(json).unwrap();
        assert_eq!(back, with);
    }
}
