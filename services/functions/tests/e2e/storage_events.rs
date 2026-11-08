//! Edge functions m14 (#176): a storage trigger on bucket `avatars`, prefix `in/`, event
//! `object.created` delivers each matching upload to the function's `fetch` handler, marked
//! `chelabase-event: storage`, with the object event as the body; an upload outside the prefix
//! delivers nothing. The handler reads the uploaded object through the storage API and records
//! the event through the data API, both with its run-as role's `CHELABASE_SERVICE_KEY`.
//!
//! The database, the table and the storage policies are made through the management API and
//! SQL, as `db_events` makes them; the function, its run-as role and its trigger through the
//! management API; the uploads through the gateway with a secret key of the branch. The run-as
//! role is `authenticated`: the storage schema grants its tables to `anon` and `authenticated`
//! only, so a custom role reads no object whatever its policies say.

use chelabase_metadata::DEMO_TENANT_ID;
use chelabase_types::{BranchId, DatabaseId};
use serde_json::{Value, json};
use uuid::Uuid;

use super::app_users::{Sql, until};
use super::{Api, Stack, env, metadata};

/// Reads the object of each storage event it is sent and records the event with its contents.
const RECORDER: &str = r#"export default {
  async fetch(request, env) {
    if (request.headers.get("chelabase-event") !== "storage") return new Response("not an event", { status: 400 });
    const e = await request.json();
    const auth = { authorization: `Bearer ${env.CHELABASE_SERVICE_KEY}` };
    const path = e.name.split("/").map(encodeURIComponent).join("/");
    const obj = await env.CHELABASE.fetch(new URL(`/storage/v1/object/${e.bucket}/${path}`, env.CHELABASE_URL), { headers: auth });
    if (!obj.ok) return new Response(`read: ${obj.status} ${await obj.text()}`, { status: 500 });
    const res = await env.CHELABASE.fetch(new URL("/data/v1/query", env.CHELABASE_URL), {
      method: "POST",
      headers: { "content-type": "application/json", ...auth },
      body: JSON.stringify({ v: 1, op: "insert", into: "seen",
                             values: [{ event_id: e.id, kind: e.type, bucket: e.bucket, name: e.name,
                                        branch_id: e.branch_id, body: await obj.text() }] }),
    });
    if (!res.ok) return new Response(`insert: ${res.status} ${await res.text()}`, { status: 500 });
    return new Response("ok");
  },
};"#;

/// Uploads `body` to `bucket/name` through the gateway with `key`.
async fn upload(key: &str, bucket: &str, name: &str, body: &str) {
    let base = env("CHELABASE_ENTRY_API_URL");
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/storage/v1/object/{bucket}/{name}",
            base.trim_end_matches('/')
        ))
        .bearer_auth(key)
        .header("content-type", "text/plain")
        .body(body.to_owned())
        .send()
        .await
        .expect("the gateway");
    let status = resp.status().as_u16();
    assert_eq!(
        status,
        200,
        "upload {bucket}/{name}: {}",
        resp.text().await.unwrap_or_default()
    );
}

async fn flow(api: &Api, project: &str) {
    let stamp = &Uuid::new_v4().simple().to_string()[..8];
    let created = api
        .ok(
            reqwest::Method::POST,
            "/v1/databases",
            Some(json!({ "tenant_id": DEMO_TENANT_ID, "project_id": project, "name": format!("fn_st_{stamp}") })),
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
            "CREATE TABLE seen (id bigint generated always as identity primary key, event_id text NOT NULL,
                 kind text NOT NULL, bucket text NOT NULL, name text NOT NULL, branch_id text NOT NULL,
                 body text, at timestamptz NOT NULL DEFAULT now());
             GRANT SELECT, INSERT ON seen TO authenticated;
             SELECT storage.create_policy('e2e_buckets', 'buckets', 'ALL', array['authenticated']::name[], 'true', 'true');
             SELECT storage.create_policy('e2e_objects', 'objects', 'ALL', array['authenticated']::name[], 'true', 'true');
             SELECT current_database();",
        )
        .await;
    let pg_database = rows.last().unwrap()[0].clone().unwrap();
    let branch = BranchId(db.as_uuid());
    let key = api
        .ok(
            reqwest::Method::POST,
            &format!("/v1/databases/{db}/branches/{branch}/keys"),
            Some(json!({ "name": "storage-events-e2e" })),
            &[201],
        )
        .await["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let st = reqwest::Client::new();
    let base = env("CHELABASE_ENTRY_API_URL");
    let resp = st
        .post(format!("{}/storage/v1/bucket", base.trim_end_matches('/')))
        .bearer_auth(&key)
        .json(&json!({ "name": "avatars" }))
        .send()
        .await
        .expect("the gateway");
    assert_eq!(
        resp.status().as_u16(),
        201,
        "bucket: {}",
        resp.text().await.unwrap_or_default()
    );

    let s = Stack::with(metadata().await, db, branch, String::new());
    s.deploy("recorder", RECORDER).await;
    let set = s
        .patch(
            "recorder",
            json!({ "run_as_role": "authenticated", "enabled": true }),
        )
        .await;
    assert_eq!(set["run_as_active"], json!(true), "{set}");
    let triggers = s.functions("/recorder/triggers");
    let made = api
        .ok(
            reqwest::Method::POST,
            &triggers,
            Some(
                json!({ "source": "storage", "pg_database": pg_database, "bucket": "avatars",
                         "prefix": "in/", "events": ["object.created"] }),
            ),
            &[201],
        )
        .await;
    assert_eq!(made["state"], "installed", "{made}");
    let id = made["id"].as_str().unwrap().to_owned();
    // The worker reads the triggers every 10 s; its consumer starts at the trigger's
    // enabled_since, so an upload made before the consumer exists is still delivered.
    upload(&key, "avatars", "out/b.txt", "not mine").await;
    upload(&key, "avatars", "in/a.txt", "hello a").await;
    let seen = until(40, "the event to be delivered", || async {
        let r = api
            .sql(
                db,
                "SELECT kind, bucket, name, branch_id, body FROM seen ORDER BY id",
            )
            .await;
        (!r.is_empty()).then_some(r)
    })
    .await;
    let row = |r: &Vec<Option<String>>| {
        r.iter()
            .map(|c| c.clone().unwrap_or_default())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        row(&seen[0]),
        [
            "object.created",
            "avatars",
            "in/a.txt",
            &branch.to_string(),
            "hello a"
        ],
        "{seen:?}"
    );

    // Once the consumer runs: within 2 s (plus the uploads' and SQL round trips).
    upload(&key, "avatars", "in/c.txt", "hello c").await;
    let started = std::time::Instant::now();
    until(10, "the second event", || async {
        (api.sql(db, "SELECT 1 FROM seen WHERE name = 'in/c.txt'")
            .await
            .len()
            == 1)
            .then_some(())
    })
    .await;
    assert!(
        started.elapsed().as_secs_f64() <= 5.0,
        "{:?}",
        started.elapsed()
    );
    let names = api.sql(db, "SELECT name FROM seen ORDER BY id").await;
    let names: Vec<_> = names.iter().map(|r| r[0].clone().unwrap()).collect();
    assert_eq!(
        names,
        ["in/a.txt", "in/c.txt"],
        "out/ is never delivered, nothing twice"
    );

    let list = api.ok(reqwest::Method::GET, &triggers, None, &[200]).await;
    let t: Value = list
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id.as_str())
        .cloned()
        .unwrap();
    assert_eq!(
        (
            t["dropped_count"].as_i64(),
            t["last_delivery_error"].clone()
        ),
        (Some(0), Value::Null),
        "{t}"
    );
    assert!(t["last_delivered_at"].is_string(), "{t}");
}

#[tokio::test]
#[ignore = "needs the workbench stack running this branch's worker, functions service and runtime: just functions-e2e"]
async fn an_upload_on_the_branch_reaches_the_storage_trigger() {
    let api = Api::new();
    let name = format!("functions-st-{}", &Uuid::new_v4().simple().to_string()[..6]);
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
