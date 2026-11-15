//! Edge functions m7 (#169): a `* * * * *` schedule runs the function's `scheduled` handler
//! once a minute through the worker; the handler writes a counter row through the data API
//! with its run-as role's `CHELABASE_SERVICE_KEY`. Each tick runs once (no two rows of one
//! `scheduledTime`), within seconds of its time; a disabled function's schedule moves on with no
//! run.
//!
//! The database and the table are made through the management API and SQL, as `app_users`
//! makes them; the function, its run-as role and its schedule through the management API.

use std::time::Duration;

use chelabase_metadata::DEMO_TENANT_ID;
use chelabase_types::{BranchId, DatabaseId};
use serde_json::{Value, json};
use uuid::Uuid;

use super::app_users::{Sql, until};
use super::{Api, Stack, metadata};

/// Writes one row per run: the controller's cron and scheduledTime.
const TICKER: &str = r#"export default {
  async fetch() {
    return new Response("ticker");
  },
  async scheduled(controller, env, ctx) {
    const res = await env.CHELABASE.fetch(new URL("/data/v1/query", env.CHELABASE_URL), {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${env.CHELABASE_SERVICE_KEY}` },
      body: JSON.stringify({ v: 1, op: "insert", into: "ticks",
                             values: [{ cron: controller.cron, scheduled_ms: controller.scheduledTime }] }),
    });
    if (!res.ok) throw new Error(`insert: ${res.status} ${await res.text()}`);
  },
};"#;

/// The ticks so far: (scheduled_ms, seconds between the tick and the insert), oldest first.
async fn ticks(api: &Api, db: DatabaseId) -> Vec<(i64, f64)> {
    api.sql(
        db,
        "SELECT scheduled_ms::text,
                (extract(epoch FROM at) * 1000 - scheduled_ms)::float8 / 1000
         FROM ticks ORDER BY scheduled_ms",
    )
    .await
    .into_iter()
    .map(|r| {
        (
            r[0].as_deref().unwrap().parse().unwrap(),
            r[1].as_deref().unwrap().parse().unwrap(),
        )
    })
    .collect()
}

async fn flow(api: &Api, project: &str) {
    let stamp = &Uuid::new_v4().simple().to_string()[..8];
    let created = api
        .ok(
            reqwest::Method::POST,
            "/v1/databases",
            Some(json!({ "tenant_id": DEMO_TENANT_ID, "project_id": project, "name": format!("fn_cron_{stamp}") })),
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
    api.sql(
        db,
        "CREATE TABLE ticks (id bigint generated always as identity primary key,
             cron text NOT NULL, scheduled_ms bigint NOT NULL, at timestamptz NOT NULL DEFAULT now());
         CREATE ROLE fn_ticker NOLOGIN; GRANT fn_ticker TO authenticator;
         GRANT USAGE ON SCHEMA public TO fn_ticker; GRANT SELECT, INSERT ON ticks TO fn_ticker;
         SELECT 1;",
    )
    .await;
    // No key is needed to deploy: the management API takes the dev member's token.
    let s = Stack::with(metadata().await, db, BranchId(db.as_uuid()), String::new());
    s.deploy("ticker", TICKER).await;
    let set = s
        .patch("ticker", json!({ "run_as_role": "fn_ticker" }))
        .await;
    assert_eq!(set["run_as_active"], json!(true), "{set}");

    let schedules = s.functions("/ticker/schedules");
    let made = api
        .ok(
            reqwest::Method::POST,
            &schedules,
            Some(json!({ "cron": "* * * * *" })),
            &[201],
        )
        .await;
    assert_eq!(made["timezone"], "UTC", "{made}");
    let (status, err) = api
        .call(
            reqwest::Method::POST,
            &schedules,
            Some(json!({ "cron": "* * *" })),
        )
        .await;
    assert_eq!(
        (status, &err["error"]["field"]),
        (422, &json!("cron")),
        "{err}"
    );

    // Two ticks, a minute apart, each once and within seconds of its time.
    let got = until(150, "two scheduled runs", || async {
        let t = ticks(api, db).await;
        (t.len() >= 2).then_some(t)
    })
    .await;
    let times: Vec<i64> = got.iter().map(|t| t.0).collect();
    let mut unique = times.clone();
    unique.dedup();
    assert_eq!(unique, times, "a tick ran twice: {got:?}");
    assert!(
        times.iter().all(|t| t % 60_000 == 0),
        "ticks on the minute: {got:?}"
    );
    assert_eq!(times[1] - times[0], 60_000, "consecutive minutes: {got:?}");
    assert!(
        got.iter().all(|t| t.1 >= 0.0 && t.1 <= 10.0),
        "within seconds of the tick: {got:?}"
    );
    let rows = api.sql(db, "SELECT DISTINCT cron FROM ticks").await;
    assert_eq!(rows, vec![vec![Some("* * * * *".to_owned())]]);

    // Disabled: the schedule's next run moves on, and nothing runs.
    s.patch("ticker", json!({ "enabled": false })).await;
    let before = ticks(api, db).await.len();
    let next = |list: Value| list[0]["next_run_at"].as_str().unwrap().to_owned();
    let first = next(api.ok(reqwest::Method::GET, &schedules, None, &[200]).await);
    until(
        75,
        "the disabled function's schedule to move on",
        || async {
            let now = next(api.ok(reqwest::Method::GET, &schedules, None, &[200]).await);
            (now != first).then_some(())
        },
    )
    .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        ticks(api, db).await.len(),
        before,
        "a disabled function ran"
    );
}

#[tokio::test]
#[ignore = "needs the workbench stack running this branch's worker and runtime: just functions-e2e"]
async fn a_schedule_runs_the_scheduled_handler_once_a_minute() {
    let api = Api::new();
    let name = format!(
        "functions-cron-{}",
        &Uuid::new_v4().simple().to_string()[..6]
    );
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
