//! The OpenAPI schema utoipa derives for a struct with timestamp and date fields:
//! `string`/`date-time` and `string`/`date`, inline, no `$ref`. The expected JSON is what utoipa
//! derived for chrono's types, frozen.

use chelabase_time::{Date, Timestamp};
use utoipa::{PartialSchema, ToSchema};

#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
struct Row {
    created_at: Timestamp,
    expires_at: Option<Timestamp>,
    day: Date,
    seen: Vec<Timestamp>,
}

fn json<T: PartialSchema + ToSchema>() -> serde_json::Value {
    serde_json::to_value(T::schema()).unwrap()
}

#[test]
fn utoipa_schema_is_inline_date_time() {
    let schema = json::<Row>();
    let frozen: serde_json::Value = serde_json::from_str(
        r#"{"type":"object","required":["created_at","day","seen"],"properties":{"created_at":{"type":"string","format":"date-time"},"day":{"type":"string","format":"date"},"expires_at":{"type":["string","null"],"format":"date-time"},"seen":{"type":"array","items":{"type":"string","format":"date-time"}}}}"#,
    )
    .unwrap();
    assert_eq!(schema, frozen);
    assert!(
        !schema.to_string().contains("$ref"),
        "inline, no $ref: {schema}"
    );
}
