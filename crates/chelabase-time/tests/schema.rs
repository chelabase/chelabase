//! The OpenAPI schema utoipa derives for a struct with timestamp and date fields is the same
//! with either backing: `string`/`date-time` and `string`/`date`, inline, no `$ref`.

use utoipa::{PartialSchema, ToSchema};

mod chrono_backed {
    use chelabase_time::{Date, Timestamp};

    #[derive(utoipa::ToSchema)]
    #[allow(dead_code)]
    pub struct Row {
        pub created_at: Timestamp,
        pub expires_at: Option<Timestamp>,
        pub day: Date,
        pub seen: Vec<Timestamp>,
    }
}

mod jiff_backed {
    use chelabase_time::jiff_backed::{Date, Timestamp};

    #[derive(utoipa::ToSchema)]
    #[allow(dead_code)]
    pub struct Row {
        pub created_at: Timestamp,
        pub expires_at: Option<Timestamp>,
        pub day: Date,
        pub seen: Vec<Timestamp>,
    }
}

fn json<T: PartialSchema + ToSchema>() -> serde_json::Value {
    serde_json::to_value(T::schema()).unwrap()
}

#[test]
fn utoipa_schema_is_inline_date_time() {
    let chrono = json::<chrono_backed::Row>();
    let jiff = json::<jiff_backed::Row>();
    assert_eq!(jiff, chrono);
    let props = &jiff["properties"];
    assert_eq!(props["created_at"]["type"], "string");
    assert_eq!(props["created_at"]["format"], "date-time");
    assert_eq!(props["day"]["type"], "string");
    assert_eq!(props["day"]["format"], "date");
    assert_eq!(props["seen"]["items"]["format"], "date-time");
    assert!(
        !jiff.to_string().contains("$ref"),
        "inline, no $ref: {jiff}"
    );
    let required = jiff["required"].as_array().unwrap();
    assert!(required.contains(&"created_at".into()));
    assert!(!required.contains(&"expires_at".into()));
}
