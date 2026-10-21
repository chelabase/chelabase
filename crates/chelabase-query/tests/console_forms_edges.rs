//! RT1 T4 (coder's edge cases beyond the RED set): the console mode never leaks past its
//! own parse, `text` is only ever `true`, and an `in` list binds as a real `text[]`.

use chelabase_query::query::{Op, ParseMode, parse, parse_with};
use chelabase_query::sql::{
    BindValue, Catalog, ColumnInfo, FnLookupError, FunctionInfo, ParamType, TableInfo, plan,
    resolve_bind,
};

const SELECT_TEXT: &str =
    r#"{"v":1,"op":"select","from":"t","where":{"col":"id","in":["a","b"],"text":true}}"#;

#[test]
fn console_mode_does_not_outlive_its_parse() {
    // Alternating modes on one thread, and an early error inside a console parse, leave
    // the next data API parse strict.
    for _ in 0..3 {
        parse_with(SELECT_TEXT.as_bytes(), ParseMode::Console).unwrap();
        assert!(parse(SELECT_TEXT.as_bytes()).is_err());
        assert!(
            parse_with(
                br#"{"v":1,"op":"select","from":"t","where":{"col":"id","eq":1,"text":true}}"#,
                ParseMode::Console
            )
            .is_err()
        );
        assert!(parse(SELECT_TEXT.as_bytes()).is_err());
        assert!(parse_with(b"not json", ParseMode::Console).is_err());
        assert!(parse(SELECT_TEXT.as_bytes()).is_err());
    }
}

#[test]
fn text_is_true_or_refused() {
    for v in ["false", "0", "\"\"", "[]", "{}"] {
        let body = format!(
            r#"{{"v":1,"op":"select","from":"t","where":{{"col":"id","eq":"1","text":{v}}}}}"#
        );
        assert!(
            parse_with(body.as_bytes(), ParseMode::Console).is_err(),
            "{v}"
        );
    }
}

#[test]
fn an_empty_text_in_list_parses_and_plans_to_a_text_array() {
    let body = br#"{"v":1,"op":"select","from":"t","where":{"col":"id","in":[],"text":true}}"#;
    let op = parse_with(body, ParseMode::Console).unwrap().op;
    let p = plan(&op, &One, "public").unwrap().remove(0);
    assert_eq!(p.param_types()[0], ParamType::TextArray);
    assert!(matches!(
        resolve_bind(&op, &p.binds[0], 1000),
        Some(BindValue::TextArray(v)) if v.is_empty()
    ));
}

#[test]
fn a_text_in_list_binds_its_strings_in_order() {
    let op = parse_with(SELECT_TEXT.as_bytes(), ParseMode::Console)
        .unwrap()
        .op;
    let p = plan(&op, &One, "public").unwrap().remove(0);
    assert_eq!(
        resolve_bind(&op, &p.binds[0], 1000),
        Some(BindValue::TextArray(vec!["a", "b"]))
    );
    assert!(matches!(&op, Op::Select(_)));
}

struct One;

impl Catalog for One {
    fn table(&self, name: &chelabase_query::query::Name) -> Option<&TableInfo> {
        use std::sync::OnceLock;
        static T: OnceLock<TableInfo> = OnceLock::new();
        let t = T.get_or_init(|| TableInfo {
            schema: "public".into(),
            name: "t".into(),
            columns: vec![ColumnInfo {
                name: "id".into(),
                type_sql: "bigint".into(),
                type_qualified: String::new(),
                array: false,
                text_like: false,
                generated: false,
            }],
        });
        (name.name == t.name).then_some(t)
    }

    fn function(
        &self,
        _: &chelabase_query::query::Name,
        _: &[&str],
    ) -> Result<&FunctionInfo, FnLookupError> {
        Err(FnLookupError::NotFound)
    }
}
