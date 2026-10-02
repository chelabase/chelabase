mod common;

use common::{FakeCatalog, op};
use cheladb_query::shape::shape_hash;
use cheladb_query::sql::plan;

fn hash(json: &str) -> u64 {
    shape_hash(&op(json))
}

fn sql(json: &str) -> Vec<String> {
    plan(&op(json), &FakeCatalog::new(), "public")
        .unwrap()
        .into_iter()
        .map(|p| p.sql)
        .collect()
}

#[test]
fn values_dont_change_the_hash_or_sql() {
    let pairs = [
        (
            r#"{"v":1,"op":"select","from":"Order","where":{"and":[{"col":"total","gte":100},{"col":"select","like":"a%"}]},"limit":5,"offset":1}"#,
            r#"{"v":1,"op":"select","from":"Order","where":{"and":[{"col":"total","gte":"7.25"},{"col":"select","like":"zz"}]},"limit":900,"offset":40}"#,
        ),
        (
            r#"{"v":1,"op":"update","table":"Order","set":{"total":1,"mood":"happy"},"where":{"col":"id","eq":1}}"#,
            r#"{"v":1,"op":"update","table":"Order","set":{"mood":"sad","total":"2.5"},"where":{"col":"id","eq":"99"}}"#,
        ),
        (
            r#"{"v":1,"op":"call","fn":"add_points","args":{"user_id":"a","n":1}}"#,
            r#"{"v":1,"op":"call","fn":"add_points","args":{"n":50,"user_id":"b"}}"#,
        ),
        (
            r#"{"v":1,"op":"batch","ops":[{"op":"delete","from":"Order","where":{"col":"meta","eq":{"a":1}}}]}"#,
            r#"{"v":1,"op":"batch","ops":[{"op":"delete","from":"Order","where":{"col":"meta","eq":[1,2]}}]}"#,
        ),
    ];
    for (a, b) in pairs {
        assert_eq!(hash(a), hash(b), "{a} vs {b}");
        assert_eq!(sql(a), sql(b), "{a} vs {b}");
    }
}

#[test]
fn row_count_and_in_length_dont_change_the_hash() {
    let a = r#"{"v":1,"op":"insert","into":"Order","values":[{"id":1,"total":2}]}"#;
    let b = r#"{"v":1,"op":"insert","into":"Order","values":[{"id":1,"total":2},{"total":3,"id":4},{"id":5,"total":6}]}"#;
    assert_eq!(hash(a), hash(b));
    assert_eq!(sql(a), sql(b));

    let a = r#"{"v":1,"op":"select","from":"Order","where":{"col":"id","in":[]}}"#;
    let b = r#"{"v":1,"op":"select","from":"Order","where":{"col":"id","in":[1,2,3,4]}}"#;
    assert_eq!(hash(a), hash(b));
    assert_eq!(sql(a), sql(b));
}

#[test]
fn different_columns_change_the_hash() {
    let distinct = [
        r#"{"v":1,"op":"select","from":"Order"}"#,
        r#"{"v":1,"op":"select","from":"public.Order"}"#,
        r#"{"v":1,"op":"select","from":"billing.invoices"}"#,
        r#"{"v":1,"op":"select","from":"Order","select":["id"]}"#,
        r#"{"v":1,"op":"select","from":"Order","select":["total"]}"#,
        r#"{"v":1,"op":"select","from":"Order","select":["id","total"]}"#,
        r#"{"v":1,"op":"select","from":"Order","select":["total","id"]}"#,
        r#"{"v":1,"op":"select","from":"Order","select":[{"col":"id","as":"x"}]}"#,
        r#"{"v":1,"op":"select","from":"Order","where":{"col":"id","eq":1}}"#,
        r#"{"v":1,"op":"select","from":"Order","where":{"col":"total","eq":1}}"#,
        r#"{"v":1,"op":"select","from":"Order","where":{"col":"id","neq":1}}"#,
        r#"{"v":1,"op":"select","from":"Order","where":{"col":"id","is":null}}"#,
        r#"{"v":1,"op":"select","from":"Order","where":{"col":"id","is":true}}"#,
        r#"{"v":1,"op":"select","from":"Order","where":{"not":{"col":"id","eq":1}}}"#,
        r#"{"v":1,"op":"select","from":"Order","where":{"and":[{"col":"id","eq":1}]}}"#,
        r#"{"v":1,"op":"select","from":"Order","where":{"or":[{"col":"id","eq":1}]}}"#,
        r#"{"v":1,"op":"select","from":"Order","orderBy":[{"col":"id"}]}"#,
        r#"{"v":1,"op":"select","from":"Order","orderBy":[{"col":"id","dir":"desc"}]}"#,
        r#"{"v":1,"op":"select","from":"Order","orderBy":[{"col":"id","nulls":"first"}]}"#,
        r#"{"v":1,"op":"select","from":"Order","limit":1}"#,
        r#"{"v":1,"op":"select","from":"Order","offset":1}"#,
        r#"{"v":1,"op":"insert","into":"Order","values":[{"id":1}]}"#,
        r#"{"v":1,"op":"insert","into":"Order","values":[{"total":1}]}"#,
        r#"{"v":1,"op":"insert","into":"Order","values":[{"id":1}],"returning":"*"}"#,
        r#"{"v":1,"op":"insert","into":"Order","values":[{"id":1}],"returning":["id"]}"#,
        r#"{"v":1,"op":"update","table":"Order","set":{"id":1},"all":true}"#,
        r#"{"v":1,"op":"update","table":"Order","set":{"total":1},"all":true}"#,
        r#"{"v":1,"op":"delete","from":"Order","all":true}"#,
        r#"{"v":1,"op":"delete","from":"Order","where":{"col":"id","eq":1}}"#,
        r#"{"v":1,"op":"call","fn":"add_points","args":{"user_id":"u"}}"#,
        r#"{"v":1,"op":"call","fn":"add_points","args":{"user_id":"u","n":1}}"#,
        r#"{"v":1,"op":"batch","ops":[{"op":"delete","from":"Order","all":true}]}"#,
        r#"{"v":1,"op":"batch","ops":[{"op":"delete","from":"Order","all":true},{"op":"delete","from":"Order","all":true}]}"#,
    ];
    let mut seen = std::collections::HashMap::new();
    for json in distinct {
        if let Some(prev) = seen.insert(hash(json), json) {
            panic!("same hash:\n{prev}\n{json}");
        }
    }
}
