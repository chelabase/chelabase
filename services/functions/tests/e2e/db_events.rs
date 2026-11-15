//! Edge functions m13 (#175): an insert trigger on `public.orders` delivers each committed row
//! to the function's `fetch` handler, marked `chelabase-event: db`, within seconds; a rolled-back
//! insert delivers nothing. The handler records each event through the data API with its run-as
//! role's `CHELABASE_SERVICE_KEY`. A trigger with mixed table and object events is 422, and a
//! storage trigger starts `installed`.
//!
//! The database, the tables and the role are made through the management API and SQL, as
//! `scheduled` makes them; the function, its run-as role and its triggers through the
//! management API.

use chelabase_metadata::DEMO_TENANT_ID;
use chelabase_types::{BranchId, DatabaseId};
use serde_json::{Value, json};
use uuid::Uuid;

use super::app_users::{Sql, until};
use super::{Api, Stack, metadata};

/// Records each database event it is sent: its id, type, record and txid.
const RECORDER: &str = r#"export default {
  async fetch(request, env) {
    if (request.headers.get("chelabase-event") !== "db") return new Response("not an event", { status: 400 });
    const e = await request.json();
    const res = await env.CHELABASE.fetch(new URL("/data/v1/query", env.CHELABASE_URL), {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${env.CHELABASE_SERVICE_KEY}` },
      body: JSON.stringify({ v: 1, op: "insert", into: "seen",
                             values: [{ event_id: e.id, kind: e.type, record: e.record, txid: e.txid }] }),
    });
    if (!res.ok) return new Response(`insert: ${res.status} ${await res.text()}`, { status: 500 });
    return new Response("ok");
  },
};"#;

async fn flow(api: &Api, project: &str) {
    let stamp = &Uuid::new_v4().simple().to_string()[..8];
    let created = api
        .ok(
            reqwest::Method::POST,
            "/v1/databases",
            Some(json!({ "tenant_id": DEMO_TENANT_ID, "project_id": project, "name": format!("fn_db_{stamp}") })),
            &[201, 202],
        )
        .await;
    let db = DatabaseId(created["database_id"].as_str().unwrap().parse().unwrap());
    until(180, "the database isn't ready", || async {
        let b = api
            .ok(
                reqwest::Method::GET,
                &format!("/v1/databases/{db}"),
                None,
                &[200],
            )
            .await;
        assert_ne!(b["status"], "failed", "{b}");
        (b["status"] == "ready").then_some(())
    })
    .await;
    let rows = api
        .sql(
            db,
            "CREATE TABLE orders (id int PRIMARY KEY, note text);
             CREATE TABLE seen (id bigint generated always as identity primary key, event_id text NOT NULL,
                 kind text NOT NULL, record jsonb, txid bigint, at timestamptz NOT NULL DEFAULT now());
             CREATE ROLE fn_recorder NOLOGIN; GRANT fn_recorder TO authenticator;
             GRANT USAGE ON SCHEMA public TO fn_recorder; GRANT SELECT, INSERT ON seen TO fn_recorder;
             SELECT current_database();",
        )
        .await;
    let pg_database = rows[0][0].clone().unwrap();
    let s = Stack::with(metadata().await, db, BranchId(db.as_uuid()), String::new());
    s.deploy("recorder", RECORDER).await;
    let set = s
        .patch(
            "recorder",
            json!({ "run_as_role": "fn_recorder", "enabled": true }),
        )
        .await;
    assert_eq!(set["run_as_active"], json!(true), "{set}");

    let triggers = s.functions("/recorder/triggers");
    let (status, err) = api
        .call(
            reqwest::Method::POST,
            &triggers,
            Some(json!({ "source": "table", "pg_database": pg_database, "schema": "public", "table": "orders",
                         "events": ["insert", "object.created"] })),
        )
        .await;
    assert_eq!(status, 422, "{err}");
    let storage = api
        .ok(
            reqwest::Method::POST,
            &triggers,
            Some(json!({ "source": "storage", "pg_database": pg_database, "events": ["object.created"] })),
            &[201],
        )
        .await;
    assert_eq!(storage["state"], "installed", "{storage}");
    let made = api
        .ok(
            reqwest::Method::POST,
            &triggers,
            Some(json!({ "source": "table", "pg_database": pg_database, "schema": "public", "table": "orders",
                         "events": ["insert"] })),
            &[201],
        )
        .await;
    assert_eq!(made["state"], "pending_install", "{made}");
    let id = made["id"].as_str().unwrap().to_owned();
    until(90, "the worker to install the trigger", || async {
        let list = api.ok(reqwest::Method::GET, &triggers, None, &[200]).await;
        let t = list
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id.as_str())
            .cloned()
            .unwrap();
        assert_ne!(t["state"], "failed", "{t}");
        (t["state"] == "installed").then_some(())
    })
    .await;

    api.sql(db, "BEGIN; INSERT INTO orders VALUES (9, 'no'); ROLLBACK; INSERT INTO orders VALUES (1, 'a'); SELECT 1;")
        .await;
    let started = std::time::Instant::now();
    let seen = until(30, "the event to be delivered", || async {
        let r = api
            .sql(
                db,
                "SELECT event_id, kind, record::text, txid::text FROM seen ORDER BY id",
            )
            .await;
        (!r.is_empty()).then_some(r)
    })
    .await;
    // The dispatcher may still be starting its first pass; after that it is seconds.
    assert!(started.elapsed().as_secs() <= 30, "{:?}", started.elapsed());
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0][1].as_deref(), Some("INSERT"));
    let record: Value = serde_json::from_str(seen[0][2].as_deref().unwrap()).unwrap();
    assert_eq!(record, json!({ "id": 1, "note": "a" }));
    assert!(seen[0][3].is_some(), "a txid");

    // A second insert, once the dispatcher is running: within 2 s (plus the SQL round trips).
    api.sql(db, "INSERT INTO orders VALUES (2, 'b'); SELECT 1;")
        .await;
    let started = std::time::Instant::now();
    until(10, "the second event", || async {
        (api.sql(db, "SELECT 1 FROM seen").await.len() >= 2).then_some(())
    })
    .await;
    assert!(
        started.elapsed().as_secs_f64() <= 5.0,
        "{:?}",
        started.elapsed()
    );
    let list = api.ok(reqwest::Method::GET, &triggers, None, &[200]).await;
    let t = list
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id.as_str())
        .cloned()
        .unwrap();
    assert_eq!(
        (t["dead_count"].as_i64(), t["last_delivery_error"].clone()),
        (Some(0), Value::Null),
        "{t}"
    );
    assert!(t["last_delivered_at"].is_string(), "{t}");
}

#[tokio::test]
#[ignore = "needs the workbench stack running this branch's worker and functions service: just functions-e2e"]
async fn an_insert_trigger_delivers_committed_rows_to_fetch() {
    let api = Api::new();
    let name = format!("functions-db-{}", &Uuid::new_v4().simple().to_string()[..6]);
    let project = api
        .ok(
            reqwest::Method::POST,
            "/v1/projects",
            Some(json!({ "name": name })),
            &[201],
        )
        .await;
    let (id, slug) = (
        project["id"].as_str().unwrap().to_owned(),
        project["slug"].as_str().unwrap().to_owned(),
    );
    let run = {
        let id = id.clone();
        let api = Api::new();
        tokio::spawn(async move { flow(&api, &id).await })
    };
    let result = run.await;
    let _ = api
        .call(
            reqwest::Method::DELETE,
            &format!("/v1/projects/{id}"),
            Some(json!({ "confirm": slug })),
        )
        .await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}
