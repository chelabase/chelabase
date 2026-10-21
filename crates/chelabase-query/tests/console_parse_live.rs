//! RT1 final fix round (batch B, review I3): a by-key condition with `"parse": true` uses the
//! primary key's index, and finds the row whose key texts a `text_of` read printed in the same
//! session. Against the dev metadata Postgres, in a scratch database dropped afterwards. Run
//! through the live queue:
//! just workbench test -p chelabase-query --test console_parse_live -- --run-ignored only

use chelabase_query::query::{Op, ParseMode, parse_with};
use chelabase_query::schema::{introspect, visibility};
use chelabase_query::sql::{BindValue, ParamType, Plan, plan, resolve_bind};
use serde_json::Value;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    })
}

async fn connect_to(database: Option<&str>) -> Client {
    let mut config: tokio_postgres::Config = url().parse().unwrap();
    if let Some(database) = database {
        config.dbname(database);
    }
    let (client, conn) = config
        .connect(NoTls)
        .await
        .expect("the metadata Postgres (is the workbench stack up?)");
    tokio::spawn(conn);
    client
}

fn console(body: &str) -> Op {
    parse_with(body.as_bytes(), ParseMode::Console)
        .unwrap_or_else(|e| panic!("{body}: {e}"))
        .op
}

/// Runs `sql` (one of `p`'s statements, or `EXPLAIN` of it) with `op`'s values bound.
async fn query(c: &Client, sql: &str, op: &Op, p: &Plan) -> Vec<tokio_postgres::Row> {
    let values: Vec<BindValue<'_>> = p
        .binds
        .iter()
        .map(|b| resolve_bind(op, b, 1000).expect("the plan's own tree"))
        .collect();
    let mut texts: Vec<Option<String>> = Vec::new();
    let mut arrays: Vec<Vec<String>> = Vec::new();
    let mut ints: Vec<i64> = Vec::new();
    let mut jsons: Vec<Value> = Vec::new();
    for v in &values {
        match v {
            BindValue::Text(t) => texts.push(t.as_ref().map(|c| c.to_string())),
            BindValue::TextArray(a) => arrays.push(a.iter().map(|s| s.to_string()).collect()),
            BindValue::Int(i) => ints.push(*i),
            BindValue::Json(j) => jsons.push((*j).clone()),
        }
    }
    let (mut ti, mut ai, mut ii, mut ji) = (0, 0, 0, 0);
    let mut params: Vec<(&(dyn ToSql + Sync), Type)> = Vec::new();
    for (v, ty) in values.iter().zip(p.param_types()) {
        match (v, ty) {
            (BindValue::Text(_), ParamType::Text) => {
                params.push((&texts[ti], Type::TEXT));
                ti += 1;
            }
            (BindValue::TextArray(_), ParamType::TextArray) => {
                params.push((&arrays[ai], Type::TEXT_ARRAY));
                ai += 1;
            }
            (BindValue::Int(_), ParamType::Int8) => {
                params.push((&ints[ii], Type::INT8));
                ii += 1;
            }
            (BindValue::Json(_), ParamType::Jsonb) => {
                params.push((&jsons[ji], Type::JSONB));
                ji += 1;
            }
            other => panic!("a bind and its type disagree: {other:?}"),
        }
    }
    c.query_typed(sql, &params)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The plan's node types and index names, from `EXPLAIN (FORMAT JSON)`.
fn scans(plan: &Value, out: &mut Vec<(String, Option<String>)>) {
    if let Some(node) = plan.get("Node Type").and_then(Value::as_str) {
        out.push((
            node.to_owned(),
            plan.get("Index Name")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ));
    }
    if let Some(children) = plan.get("Plans").and_then(Value::as_array) {
        for child in children {
            scans(child, out);
        }
    }
}

async fn explain(c: &Client, op: &Op, p: &Plan) -> Vec<(String, Option<String>)> {
    let rows = query(c, &format!("EXPLAIN (FORMAT JSON) {}", p.sql), op, p).await;
    let json: Value = rows[0].get(0);
    let mut out = Vec::new();
    scans(&json[0]["Plan"], &mut out);
    out
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_parse_condition_uses_the_primary_key_index() {
    let suffix = &Uuid::new_v4().simple().to_string()[..10];
    let database = format!("fb_parse_{suffix}");
    let cluster = connect_to(None).await;
    cluster
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .await
        .unwrap();
    let c = connect_to(Some(&database)).await;
    // Keys of several P6 types, one composite; enough rows that a scan of the table loses.
    c.batch_execute(
        r#"CREATE TYPE public.mood AS ENUM ('ok', 'meh');
           CREATE TABLE public.k (
             n numeric(12,3), at timestamptz, u uuid, m public.mood, s text, payload text,
             PRIMARY KEY (n, at, u, m, s));
           INSERT INTO public.k
             SELECT i / 7.0, timestamptz '2026-01-01 00:00:00.123456+00' + i * interval '1 minute',
                    pg_catalog.md5(i::text)::uuid, (ARRAY['ok','meh']::public.mood[])[1 + i % 2],
                    'key ' || i, pg_catalog.repeat('x', 50)
             FROM pg_catalog.generate_series(1, 20000) AS i;
           ANALYZE public.k;"#,
    )
    .await
    .unwrap();
    // The console session: the owner, a time zone other than UTC.
    c.batch_execute("SET TimeZone = 'Asia/Bangkok'")
        .await
        .unwrap();
    let user: String = c
        .query_one("SELECT current_user::text", &[])
        .await
        .unwrap()
        .get(0);
    let schema = introspect(&c, "v1".into()).await.unwrap();
    let view = schema.view(visibility(&c, &user).await.unwrap());

    // The key texts of two rows, as the session prints them (what Studio holds).
    let read = console(
        r#"{"v":1,"op":"select","from":"k","select":[{"text_of":"n","as":"n"},{"text_of":"at","as":"at"},{"text_of":"u","as":"u"},{"text_of":"m","as":"m"},{"text_of":"s","as":"s"}],"where":{"col":"s","in":["key 4242","key 777"]}}"#,
    );
    let p = plan(&read, &view, "public").unwrap().remove(0);
    let rows: Value =
        serde_json::from_str(&query(&c, &p.sql, &read, &p).await[0].get::<_, String>(0)).unwrap();
    let rows = rows.as_array().unwrap().clone();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let text = |r: &Value, k: &str| r[k].as_str().unwrap().to_owned();

    // By key, one row: every key column `parse`d.
    let r = &rows[0];
    let by_key = console(&format!(
        r#"{{"v":1,"op":"select","from":"k","where":{{"and":[{{"col":"n","eq":{n},"parse":true}},{{"col":"at","eq":{at},"parse":true}},{{"col":"u","eq":{u},"parse":true}},{{"col":"m","eq":{m},"parse":true}},{{"col":"s","eq":{s},"parse":true}}]}}}}"#,
        n = Value::from(text(r, "n")),
        at = Value::from(text(r, "at")),
        u = Value::from(text(r, "u")),
        m = Value::from(text(r, "m")),
        s = Value::from(text(r, "s")),
    ));
    let p = plan(&by_key, &view, "public").unwrap().remove(0);
    let found: Value =
        serde_json::from_str(&query(&c, &p.sql, &by_key, &p).await[0].get::<_, String>(0)).unwrap();
    assert_eq!(found.as_array().map(Vec::len), Some(1), "{found}");
    assert_eq!(found[0]["s"], r["s"], "the row the text_of read printed");
    let nodes = explain(&c, &by_key, &p).await;
    assert!(
        nodes
            .iter()
            .any(|(n, i)| n.contains("Index") && i.as_deref() == Some("k_pkey")),
        "by key: {nodes:?}"
    );
    assert!(
        !nodes.iter().any(|(n, _)| n == "Seq Scan"),
        "by key: {nodes:?}"
    );

    // A list of keys (`in`) on a single-column key, here a uuid:
    c.batch_execute(
        "CREATE TABLE public.one (id uuid PRIMARY KEY, v int);
         INSERT INTO public.one SELECT pg_catalog.md5(i::text)::uuid, i
           FROM pg_catalog.generate_series(1, 20000) AS i;
         ANALYZE public.one;",
    )
    .await
    .unwrap();
    let schema = introspect(&c, "v2".into()).await.unwrap();
    let view = schema.view(visibility(&c, &user).await.unwrap());
    let ids: Vec<String> = c
        .query(
            "SELECT id::text FROM public.one WHERE v IN (5, 6000, 19999) ORDER BY v",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let listed = console(&format!(
        r#"{{"v":1,"op":"select","from":"one","where":{{"col":"id","in":{},"parse":true}}}}"#,
        serde_json::to_string(&ids).unwrap()
    ));
    let p = plan(&listed, &view, "public").unwrap().remove(0);
    let found: Value =
        serde_json::from_str(&query(&c, &p.sql, &listed, &p).await[0].get::<_, String>(0)).unwrap();
    assert_eq!(found.as_array().map(Vec::len), Some(3), "{found}");
    let nodes = explain(&c, &listed, &p).await;
    assert!(
        nodes
            .iter()
            .any(|(n, i)| n.contains("Index") && i.as_deref() == Some("one_pkey")),
        "in: {nodes:?}"
    );
    assert!(!nodes.iter().any(|(n, _)| n == "Seq Scan"), "in: {nodes:?}");

    // The `text` form of the same condition can't use the index (why `parse` exists).
    let as_text = console(&format!(
        r#"{{"v":1,"op":"select","from":"one","where":{{"col":"id","eq":{},"text":true}}}}"#,
        Value::from(ids[0].clone())
    ));
    let p = plan(&as_text, &view, "public").unwrap().remove(0);
    let nodes = explain(&c, &as_text, &p).await;
    assert!(
        !nodes.iter().any(|(_, i)| i.as_deref() == Some("one_pkey")),
        "text: {nodes:?}"
    );

    // A text the type can't read is Postgres's error (22P02), not a silent miss.
    let bad = console(
        r#"{"v":1,"op":"select","from":"one","where":{"col":"id","eq":"not-a-uuid","parse":true}}"#,
    );
    let p = plan(&bad, &view, "public").unwrap().remove(0);
    let values: Vec<Option<String>> = vec![Some("not-a-uuid".into())];
    let err = c
        .query_typed(
            &p.sql,
            &[
                (&values[0], Type::TEXT),
                (&1000_i64, Type::INT8),
                (&0_i64, Type::INT8),
            ],
        )
        .await
        .expect_err("not a uuid");
    assert_eq!(
        err.as_db_error().map(|d| d.code().code()),
        Some("22P02"),
        "{err}"
    );

    drop(c);
    cluster
        .batch_execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
        .await
        .unwrap();
}
