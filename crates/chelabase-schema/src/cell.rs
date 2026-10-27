//! The table editor's full-cell read, shared by the orchestrator's member-role path and rest's
//! console path (Q1, P40): the primary key, the cell query, the key columns' unmodified types
//! and the 64 MiB backstop.

use std::collections::{BTreeMap, BTreeSet};

use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::table_api::CellValue;
use serde_json::Value;

use crate::model::{Column, ConstraintKind, Table};
use crate::sql::{ident, qualified};

/// The largest cell value the cell read returns.
pub const MAX_CELL_BYTES: i64 = 64 * 1024 * 1024;
/// `statement_timeout` (and the transaction timeout) of the cell read.
pub const CELL_STATEMENT_TIMEOUT_MS: u64 = 15_000;
/// A key column's type without its modifier (`$1`: the type's OID).
pub const KEY_TYPE_SQL: &str = "SELECT pg_catalog.format_type($1::pg_catalog.oid, NULL)";

/// The primary key's columns of `table`, in key order; `None` without a primary key.
pub fn primary_key(table: &Table) -> Option<Vec<&Column>> {
    let pk = table
        .constraints
        .iter()
        .find(|c| c.kind == ConstraintKind::PrimaryKey)?;
    pk.columns
        .iter()
        .map(|n| table.columns.iter().find(|c| c.attnum == *n))
        .collect()
}

/// A key value as the text it binds as: strings as is, numbers and booleans as their JSON text.
pub fn key_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The cell query for `table`'s primary key and the values to bind (in `$n` order), or the 400.
pub fn cell_query(
    table: &Table,
    key: &BTreeMap<String, Value>,
    column: &str,
) -> Result<(String, Vec<String>), ApiError> {
    let Some(pk) = primary_key(table) else {
        return Err(ApiError::invalid_request(400, "the table has no primary key"));
    };
    let want: BTreeSet<&str> = pk.iter().map(|c| c.name.as_str()).collect();
    let got: BTreeSet<&str> = key.keys().map(String::as_str).collect();
    if want != got {
        return Err(ApiError::invalid_request(
            400,
            format!(
                "`key` must name exactly the primary key's columns: {}",
                pk.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        ));
    }
    let Some(target) = table.columns.iter().find(|c| c.name == column) else {
        return Err(ApiError::new(
            400,
            ErrorCode::UnknownColumn,
            format!("the table has no column {column:?}"),
        ));
    };
    let mut conditions = Vec::new();
    let mut params = Vec::new();
    for (i, c) in pk.iter().enumerate() {
        let Some(text) = key_text(&key[&c.name]) else {
            return Err(ApiError::invalid_request(
                400,
                format!("the key value of {:?} must be a string, a number or a boolean", c.name),
            ));
        };
        params.push(text);
        conditions.push(format!(
            "t.{} = ${}::pg_catalog.text::{}",
            ident(&c.name),
            i + 1,
            c.data_type
        ));
    }
    let col = format!("t.{}", ident(&target.name));
    // The cap is on the text Postgres would send: the cast runs once, in a lateral subquery
    // (`OFFSET 0` keeps the planner from inlining it), and an over-cap text never leaves the
    // server (only the flag does).
    let sql = format!(
        "SELECT CASE WHEN pg_catalog.octet_length(l.v) > {MAX_CELL_BYTES} THEN NULL ELSE l.v END, \
         COALESCE(pg_catalog.octet_length(l.v) > {MAX_CELL_BYTES}, false), \
         pg_catalog.pg_column_size({col}), pg_catalog.format_type({}, NULL) \
         FROM {} AS t CROSS JOIN LATERAL (SELECT {col}::pg_catalog.text AS v OFFSET 0) AS l WHERE {}",
        target.type_oid.0,
        qualified(&table.schema, &table.name),
        conditions.join(" AND ")
    );
    Ok((sql, params))
}

/// The cell query's row as the answer, with the 64 MiB backstop (413 `too_large`): the flag is
/// the check; the size and text checks catch a value that slipped past it.
pub fn decode_cell(
    value: Option<String>,
    over: bool,
    bytes: Option<i32>,
    data_type: String,
) -> Result<CellValue, ApiError> {
    let bytes = i64::from(bytes.unwrap_or(0));
    if over
        || bytes > MAX_CELL_BYTES
        || value.as_ref().is_some_and(|v| v.len() as i64 > MAX_CELL_BYTES)
    {
        return Err(ApiError::new(413, ErrorCode::TooLarge, "the value is over 64 MiB"));
    }
    Ok(CellValue { value, bytes, data_type })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_types::api_error::ErrorCode;
    use serde_json::json;

    fn table() -> Table {
        serde_json::from_value(json!({
            "oid": 1, "schema": "public", "name": "t", "owner": {"name": "o", "oid": 2},
            "comment": null, "rls_enabled": false, "rls_forced": false, "unlogged": false,
            "options": [],
            "columns": [
                {"attnum": 1, "name": "id", "data_type": "bigint", "type_oid": 20, "collation": null,
                 "default": null, "not_null": true, "identity": null, "generated": null, "comment": null, "grants": []},
                {"attnum": 2, "name": "Body", "data_type": "text", "type_oid": 25, "collation": null,
                 "default": null, "not_null": false, "identity": null, "generated": null, "comment": null, "grants": []}
            ],
            "constraints": [
                {"oid": 3, "name": "t_pkey", "kind": "primary_key", "definition": "PRIMARY KEY (id)",
                 "columns": [1], "references": null, "deferrable": false, "initially_deferred": false, "validated": true}
            ],
            "indexes": [], "policies": [], "grants": []
        }))
        .unwrap()
    }

    fn key(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn cell_query_binds_text_and_quotes_names() {
        let (sql, params) = cell_query(&table(), &key(&[("id", json!(1))]), "Body").unwrap();
        assert_eq!(params, ["1"]);
        assert!(
            sql.contains("t.\"Body\"") && sql.contains("t.id = $1::pg_catalog.text::bigint"),
            "{sql}"
        );
        let (_, params) = cell_query(&table(), &key(&[("id", json!(true))]), "id").unwrap();
        assert_eq!(params, ["true"]);
    }

    #[test]
    fn cell_query_caps_the_text_form_server_side() {
        let (sql, _) = cell_query(&table(), &key(&[("id", json!(1))]), "Body").unwrap();
        assert!(
            sql.contains("octet_length(l.v) > 67108864 THEN NULL"),
            "{sql}"
        );
        assert!(
            sql.contains("LATERAL (SELECT t.\"Body\"::pg_catalog.text AS v OFFSET 0)"),
            "{sql}"
        );
    }

    #[test]
    fn cell_query_refuses_bad_keys_and_columns() {
        let t = table();
        for (k, c, code) in [
            (key(&[("id", json!([1]))]), "id", ErrorCode::InvalidRequest),
            (key(&[("id", json!(null))]), "id", ErrorCode::InvalidRequest),
            (
                key(&[("id", json!(1)), ("x", json!(1))]),
                "id",
                ErrorCode::InvalidRequest,
            ),
            (key(&[]), "id", ErrorCode::InvalidRequest),
            (key(&[("id", json!(1))]), "nope", ErrorCode::UnknownColumn),
        ] {
            let e = cell_query(&t, &k, c).unwrap_err();
            assert_eq!((e.status, e.code), (400, code), "{k:?} {c}");
        }
        let mut no_pk = table();
        no_pk.constraints.clear();
        let e = cell_query(&no_pk, &key(&[("id", json!(1))]), "id").unwrap_err();
        assert_eq!((e.status, e.code), (400, ErrorCode::InvalidRequest));
    }

    /// T5 (P40): the backstop both paths share. The server-side flag, a stored size over the
    /// cap and a text over the cap are each 413 `too_large`; anything else is the value as read.
    #[test]
    fn decode_cell_refuses_over_the_cap() {
        let cap = usize::try_from(MAX_CELL_BYTES).unwrap();
        let over_cap_bytes = i32::try_from(MAX_CELL_BYTES + 1).unwrap();
        for (value, over, bytes) in [
            (None, true, Some(10)),
            (Some("x".to_owned()), false, Some(over_cap_bytes)),
            (Some("x".repeat(cap + 1)), false, Some(10)),
        ] {
            let len = value.as_ref().map(String::len);
            let e = decode_cell(value, over, bytes, "text".into()).unwrap_err();
            assert_eq!(
                (e.status, e.code),
                (413, ErrorCode::TooLarge),
                "{len:?} {over} {bytes:?}"
            );
        }
        // At the cap exactly, and a SQL NULL (no stored size), the value comes back.
        let at_cap = "y".repeat(cap);
        let cell = decode_cell(Some(at_cap.clone()), false, Some(1234), "text".into()).unwrap();
        assert_eq!(
            cell,
            CellValue {
                value: Some(at_cap),
                bytes: 1234,
                data_type: "text".into()
            }
        );
        let cell = decode_cell(None, false, None, "integer".into()).unwrap();
        assert_eq!(
            cell,
            CellValue {
                value: None,
                bytes: 0,
                data_type: "integer".into()
            }
        );
        assert_eq!(
            (MAX_CELL_BYTES, CELL_STATEMENT_TIMEOUT_MS),
            (67_108_864, 15_000)
        );
    }
}
