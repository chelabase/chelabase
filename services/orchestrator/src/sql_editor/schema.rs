//! The schema read behind the editor's completion.
//!
//! Runs the 12 introspection queries of pgls 0.26.0 (`pgls/*.sql`, see its README) as the
//! caller's own connection, so the language server only learns what that role can see, and
//! turns the rows into the JSON `Workspace.setSchema` takes.

use serde_json::{Map, Value, json};

use super::MAX_SCHEMA_BYTES;

/// Bounds each introspection query and its lock waits (the route bounds the whole read).
pub const SCHEMA_SETTINGS: &str = "SET statement_timeout = '30s'; SET lock_timeout = '5s'";

#[derive(Debug)]
pub enum SchemaError {
    Query(String),
    TooLarge(usize),
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(e) => f.write_str(e),
            Self::TooLarge(n) => write!(f, "schema too large ({n} bytes)"),
        }
    }
}

impl std::error::Error for SchemaError {}

/// The per-query JSON arrays (`versions` is one object), exactly as Postgres returned them.
pub struct RawParts {
    pub versions: Value,
    pub schemas: Value,
    pub tables: Value,
    pub columns: Value,
    pub functions: Value,
    pub types: Value,
    pub policies: Value,
    pub extensions: Value,
    pub triggers: Value,
    pub roles: Value,
    pub indexes: Value,
    pub sequences: Value,
}

const VERSIONS: &str = include_str!("pgls/versions.sql");
const SCHEMAS: &str = include_str!("pgls/schemas.sql");
const TABLES: &str = include_str!("pgls/tables.sql");
const COLUMNS: &str = include_str!("pgls/columns.sql");
const FUNCTIONS: &str = include_str!("pgls/functions.sql");
const TYPES: &str = include_str!("pgls/types.sql");
const POLICIES: &str = include_str!("pgls/policies.sql");
const EXTENSIONS: &str = include_str!("pgls/extensions.sql");
const TRIGGERS: &str = include_str!("pgls/triggers.sql");
const ROLES: &str = include_str!("pgls/roles.sql");
const INDEXES: &str = include_str!("pgls/indexes.sql");
const SEQUENCES: &str = include_str!("pgls/sequences.sql");

fn wrap(sql: &str, single: bool) -> String {
    let body = sql.trim_end().trim_end_matches(';').trim_end();
    if single {
        format!("SELECT row_to_json(q)::text FROM ({body}\n) q")
    } else {
        format!("SELECT coalesce(json_agg(q), '[]')::text FROM ({body}\n) q")
    }
}

fn query_error(name: &str, e: impl std::fmt::Display) -> SchemaError {
    SchemaError::Query(format!("schema query {name}.sql failed: {e}"))
}

async fn run(
    client: &tokio_postgres::Client,
    name: &str,
    sql: &str,
    single: bool,
) -> Result<Value, SchemaError> {
    let messages = client
        .simple_query(&wrap(sql, single))
        .await
        .map_err(|e| query_error(name, e))?;
    for m in messages {
        if let tokio_postgres::SimpleQueryMessage::Row(row) = m {
            let text = row.get(0).unwrap_or("null");
            return serde_json::from_str(text).map_err(|e| query_error(name, e));
        }
    }
    Err(query_error(name, "no row returned"))
}

/// Runs pgls 0.26.0's introspection on `client` (as whoever it's connected as) and returns
/// the JSON `Workspace.setSchema` takes, serialized; `TooLarge` beyond `MAX_SCHEMA_BYTES`.
pub async fn introspect(client: &tokio_postgres::Client) -> Result<String, SchemaError> {
    client
        .batch_execute(SCHEMA_SETTINGS)
        .await
        .map_err(|e| SchemaError::Query(format!("settings: {e}")))?;
    let parts = RawParts {
        versions: run(client, "versions", VERSIONS, true).await?,
        schemas: run(client, "schemas", SCHEMAS, false).await?,
        tables: run(client, "tables", TABLES, false).await?,
        columns: run(client, "columns", COLUMNS, false).await?,
        functions: run(client, "functions", FUNCTIONS, false).await?,
        types: run(client, "types", TYPES, false).await?,
        policies: run(client, "policies", POLICIES, false).await?,
        extensions: run(client, "extensions", EXTENSIONS, false).await?,
        triggers: run(client, "triggers", TRIGGERS, false).await?,
        roles: run(client, "roles", ROLES, false).await?,
        indexes: run(client, "indexes", INDEXES, false).await?,
        sequences: run(client, "sequences", SEQUENCES, false).await?,
    };
    let out =
        serde_json::to_string(&assemble(&parts)).map_err(|e| SchemaError::Query(e.to_string()))?;
    if out.len() > MAX_SCHEMA_BYTES {
        return Err(SchemaError::TooLarge(out.len()));
    }
    Ok(out)
}

/// Removes the `!` that sqlx-style nullability hints leave on column names.
fn strip_bangs(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    (
                        k.strip_suffix('!').map(str::to_owned).unwrap_or(k),
                        strip_bangs(v),
                    )
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_bangs).collect()),
        other => other,
    }
}

fn rows(v: &Value) -> Vec<Map<String, Value>> {
    match strip_bangs(v.clone()) {
        Value::Array(a) => a
            .into_iter()
            .filter_map(|r| {
                if let Value::Object(m) = r {
                    Some(m)
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn map_str(row: &mut Map<String, Value>, key: &str, f: impl Fn(Option<&str>) -> &'static str) {
    let mapped = f(row.get(key).and_then(Value::as_str));
    row.insert(key.to_owned(), json!(mapped));
}

fn out(rows: Vec<Map<String, Value>>) -> Value {
    Value::Array(rows.into_iter().map(Value::Object).collect())
}

fn trigger_parts(bitmask: i64) -> (&'static str, &'static str, Vec<&'static str>) {
    let affected = if bitmask & 1 == 1 { "Row" } else { "Statement" };
    let timing = if bitmask & 64 == 64 {
        "Instead"
    } else if bitmask & 2 == 2 {
        "Before"
    } else {
        "After"
    };
    let events = [
        (4, "Insert"),
        (8, "Delete"),
        (16, "Update"),
        (32, "Truncate"),
    ]
    .into_iter()
    .filter(|(m, _)| bitmask & m == *m)
    .map(|(_, n)| n)
    .collect();
    (affected, timing, events)
}

/// The pure part, tested without Postgres: per-query JSON arrays in, `setSchema` JSON out.
pub fn assemble(parts: &RawParts) -> Value {
    let mut columns = rows(&parts.columns);
    for r in &mut columns {
        map_str(r, "class_kind", |c| match c {
            Some("v") => "View",
            Some("m") => "MaterializedView",
            Some("f") => "ForeignTable",
            Some("p") => "PartitionedTable",
            _ => "OrdinaryTable",
        });
    }

    let mut tables = rows(&parts.tables);
    for r in &mut tables {
        map_str(r, "table_kind", |c| match c {
            Some("v") => "View",
            Some("m") => "MaterializedView",
            Some("p") => "Partitioned",
            _ => "Ordinary",
        });
        map_str(r, "replica_identity", |c| match c {
            Some("INDEX") => "Index",
            Some("FULL") => "Full",
            Some("NOTHING") => "Nothing",
            _ => "Default",
        });
    }

    let mut functions = rows(&parts.functions);
    for r in &mut functions {
        map_str(r, "kind", |c| match c {
            Some("a") => "Aggregate",
            Some("w") => "Window",
            Some("p") => "Procedure",
            _ => "Function",
        });
        map_str(r, "behavior", |c| match c {
            Some("IMMUTABLE") => "Immutable",
            Some("STABLE") => "Stable",
            _ => "Volatile",
        });
        let args = match r.remove("args") {
            Some(Value::Array(a)) => a,
            Some(Value::Object(o)) if o.contains_key("args") => {
                r.insert("args".into(), Value::Object(o));
                continue;
            }
            _ => Vec::new(),
        };
        r.insert("args".into(), json!({ "args": args }));
    }

    let mut types = rows(&parts.types);
    for r in &mut types {
        let values = match r.remove("enums") {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        };
        r.insert("enums".into(), json!({ "values": values }));
        let attrs = match r.remove("attributes") {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        };
        r.insert("attributes".into(), json!({ "attrs": attrs }));
    }

    let mut policies = rows(&parts.policies);
    for r in &mut policies {
        map_str(r, "command", |c| match c {
            Some("INSERT") => "Insert",
            Some("UPDATE") => "Update",
            Some("DELETE") => "Delete",
            Some("ALL") => "All",
            _ => "Select",
        });
        let permissive = r.get("is_permissive").and_then(Value::as_str) == Some("PERMISSIVE");
        r.insert("is_permissive".into(), json!(permissive));
        if !matches!(r.get("role_names"), Some(Value::Array(_))) {
            r.insert("role_names".into(), json!([]));
        }
    }

    let mut triggers = rows(&parts.triggers);
    for r in &mut triggers {
        let bitmask = r
            .remove("details_bitmask")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let (affected, timing, events) = trigger_parts(bitmask);
        r.insert("affected".into(), json!(affected));
        r.insert("timing".into(), json!(timing));
        r.insert("events".into(), json!(events));
    }

    json!({
        "schemas": out(rows(&parts.schemas)),
        "tables": out(tables),
        "functions": out(functions),
        "types": out(types),
        "version": strip_bangs(parts.versions.clone()),
        "columns": out(columns),
        "policies": out(policies),
        "extensions": out(rows(&parts.extensions)),
        "triggers": out(triggers),
        "roles": out(rows(&parts.roles)),
        "indexes": out(rows(&parts.indexes)),
        "sequences": out(rows(&parts.sequences)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RawParts {
        RawParts {
            versions: json!({"version": "PostgreSQL 17.5", "version_num": 170005, "major_version": 17, "active_connections": 4, "max_connections": 100}),
            schemas: json!([{"id!": 2200, "name": "public", "owner!": "pg", "comment": null, "allowed_users!": ["app"], "allowed_creators!": [], "table_count!": 1, "view_count!": 0, "function_count!": 1, "total_size!": "16 kB"}]),
            tables: json!([{"id!": 16400, "schema": "public", "name": "notes", "table_kind": "r", "rls_enabled": true, "rls_forced": false, "replica_identity!": "FULL", "bytes!": 16384, "size!": "16 kB", "live_rows_estimate!": 3, "dead_rows_estimate!": 0, "comment": null}]),
            columns: json!([{"name": "id", "number": 1, "table_name": "notes", "table_oid!": 16400, "class_kind!": "r", "schema_name": "public", "type_id!": 20, "type_name": "int8", "is_nullable!": false, "varchar_length": null, "default_expr": null, "is_primary_key!": true, "is_unique!": true, "comment": null}]),
            functions: json!([{"id!": 16500, "schema!": "public", "name!": "touch", "language!": "plpgsql", "kind!": "f", "body": "begin return new; end", "definition": "create function ...", "args": [{"mode": "in", "name": "x", "type_id": 23, "has_default": false}], "argument_types": "x integer", "identity_argument_types": "x integer", "return_type_id": 2279, "return_type": "trigger", "return_type_relation_id": null, "is_set_returning_function!": false, "behavior": null, "security_definer!": false},
                {"id!": 16501, "schema!": "public", "name!": "total", "language!": "internal", "kind!": "a", "body": null, "definition": null, "args": "[]", "argument_types": null, "identity_argument_types": null, "return_type_id": 20, "return_type": "bigint", "return_type_relation_id": null, "is_set_returning_function!": false, "behavior": "IMMUTABLE", "security_definer!": true}]),
            types: json!([{"id!": 16600, "name": "mood", "schema!": "public", "format!": "mood", "enums": ["sad", "ok"], "attributes": [], "comment": null},
                {"id!": 16601, "name": "pair", "schema!": "public", "format!": "pair", "enums": [], "attributes": [{"name": "a", "type_id": 23}], "comment": "c"}]),
            policies: json!([{"schema_name!": "public", "table_name!": "notes", "name!": "own", "is_permissive!": "PERMISSIVE", "role_names!": ["app"], "command!": "ALL", "security_qualification": "true", "with_check": null},
                {"schema_name!": "public", "table_name!": "notes", "name!": "no", "is_permissive!": "RESTRICTIVE", "role_names!": null, "command!": "DELETE", "security_qualification": null, "with_check": null}]),
            extensions: json!([{"name": "plpgsql", "schema": "pg_catalog", "default_version": "1.0", "installed_version": "1.0", "comment": null}]),
            triggers: json!([{"name!": "t", "table_name!": "notes", "proc_name!": "touch", "proc_schema!": "public", "table_schema!": "public", "details_bitmask!": 23}]),
            roles: json!([{"name!": "app", "is_super_user!": false, "can_create_db!": false, "can_login!": true, "can_bypass_rls!": false, "can_create_roles!": false, "comment": null, "member_of!": [], "has_member!": []}]),
            indexes: json!([{"id!": 16410, "schema": "public", "name": "notes_pkey", "table_name": "notes"}]),
            sequences: json!([{"id!": 16420, "schema": "public", "name": "notes_id_seq"}]),
        }
    }

    #[test]
    fn assemble_maps_codes_and_wraps_lists() {
        let expected: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/pgls_schema_0.26.0.json"))
                .unwrap();
        assert_eq!(assemble(&sample()), expected);
    }

    #[test]
    fn trigger_bitmask_decodes() {
        assert_eq!(
            trigger_parts(1 | 2 | 4 | 16),
            ("Row", "Before", vec!["Insert", "Update"])
        );
        assert_eq!(
            trigger_parts(64 | 8),
            ("Statement", "Instead", vec!["Delete"])
        );
        assert_eq!(trigger_parts(1 | 32), ("Row", "After", vec!["Truncate"]));
    }

    #[test]
    fn bang_suffix_is_stripped_everywhere() {
        let v = strip_bangs(json!({"a!": 1, "b": [{"c!": {"d!": 2}}]}));
        assert_eq!(v, json!({"a": 1, "b": [{"c": {"d": 2}}]}));
        let all = assemble(&sample()).to_string();
        assert!(!all.contains("!\":"), "{all}");
    }

    #[test]
    fn pgls_sql_files_are_embedded() {
        for sql in [
            VERSIONS, SCHEMAS, TABLES, COLUMNS, FUNCTIONS, TYPES, POLICIES, EXTENSIONS, TRIGGERS,
            ROLES, INDEXES, SEQUENCES,
        ] {
            assert!(!sql.trim().is_empty());
            assert!(!sql.contains("$1"));
        }
    }

    #[test]
    fn query_errors_name_their_file() {
        let e = query_error("functions", "boom");
        assert_eq!(e.to_string(), "schema query functions.sql failed: boom");
    }

    #[test]
    fn wrap_strips_trailing_semicolon() {
        let w = wrap("select 1 as \"a!\";\n", false);
        assert!(w.ends_with("select 1 as \"a!\"\n) q"), "{w}");
    }
}
