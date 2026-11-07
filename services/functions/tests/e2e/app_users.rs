//! #183 (e2e step 6 and the run-as step): a function that queries the data API through
//! `env.CHELABASE` as its caller (the forwarded public key and user token) sees only that app
//! user's rows of an RLS-protected table; with `run_as_role` set, its `CHELABASE_SERVICE_KEY`
//! queries as that role.
//!
//! The database, its app users and the table are made through the management API and SQL, as
//! `scripts/app-users-e2e.sh` makes them (a project, a database in it, app users on, a table
//! with an own-row policy on `chelabase.user_id()`), and the users sign up through the gateway.
//! The function is deployed as in the other e2e (TODO(#165): the management API).

use std::time::{Duration, Instant};

use chelabase_metadata::{DEMO_TENANT_ID, RunAs};
use chelabase_types::data_key::{DataKeyKind, generate};
use chelabase_types::{BranchId, DatabaseId};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{Stack, env, metadata};

/// The function: the notes it can read through the binding, as its caller (`?as=caller`) or as
/// its run-as role (`?as=service`). `x-has-service-key` says whether it got a service key.
const NOTES: &str = r#"export default {
  async fetch(request, env) {
    const as = new URL(request.url).searchParams.get("as") ?? "caller";
    const headers = { "content-type": "application/json" };
    if (as === "service") {
      headers.authorization = `Bearer ${env.CHELABASE_SERVICE_KEY ?? "none"}`;
    } else {
      headers.authorization = `Bearer ${request.headers.get("chelabase-caller-key") ?? "none"}`;
      const token = request.headers.get("chelabase-caller-token");
      if (token) headers["chelabase-user"] = token;
    }
    const res = await env.CHELABASE.fetch("/data/v1/query", {
      method: "POST",
      headers,
      body: JSON.stringify({ v: 1, op: "select", from: "notes", select: ["body"] }),
    });
    return new Response(await res.text(), {
      status: res.status,
      headers: { "x-has-service-key": String(Boolean(env.CHELABASE_SERVICE_KEY)) },
    });
  },
};"#;

/// The management API as the workbench's dev member (`chelabase dev token`, per request: it is
/// short-lived).
struct Api {
    http: reqwest::Client,
    base: String,
}

impl Api {
    fn token() -> String {
        let out = std::process::Command::new(env("CHELABASE_CLI"))
            .args(["dev", "token"])
            .output()
            .expect("chelabase dev token");
        assert!(
            out.status.success(),
            "chelabase dev token: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(Self::token());
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.expect("the gateway");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn ok(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        want: &[u16],
    ) -> Value {
        let (status, body) = self.call(method.clone(), path, body).await;
        assert!(want.contains(&status), "{method} {path}: {status} {body}");
        body
    }

    /// The script's rows (each a list of text cells), as the owner; panics on an error event.
    async fn sql(&self, db: DatabaseId, sql: &str) -> Vec<Vec<Option<String>>> {
        let resp = self
            .http
            .post(format!("{}/v1/databases/{db}/branches/{db}/sql", self.base))
            .bearer_auth(Self::token())
            .json(&json!({ "run_id": Uuid::new_v4(), "sql": sql, "confirm": true }))
            .send()
            .await
            .expect("the gateway");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        assert_eq!(status, 200, "sql: {text}");
        let mut rows = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let event: Value =
                serde_json::from_str(line).unwrap_or_else(|_| panic!("sql event: {line}"));
            match event["t"].as_str() {
                Some("error") => panic!("sql: {event}"),
                Some("rows") => rows.extend(
                    serde_json::from_value::<Vec<Vec<Option<String>>>>(event["rows"].clone())
                        .unwrap(),
                ),
                _ => {}
            }
        }
        rows
    }
}

/// Polls `f` once a second until it returns `Some`, for at most `secs`.
async fn until<T, F: std::future::Future<Output = Option<T>>>(
    secs: u64,
    what: &str,
    f: impl Fn() -> F,
) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "{what} after {secs} s"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Signs an app user up through the gateway; their id and access token.
async fn sign_up(s: &Stack, email: &str) -> (String, String) {
    let resp = s
        .http
        .post(format!("{}/auth/v1/app/sign-up", s.api))
        .bearer_auth(&s.key)
        .json(&json!({ "email": email, "password": "Sup3r-Secret-pw!" }))
        .send()
        .await
        .expect("the gateway");
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 201, "sign-up {email}: {body}");
    let id = body["user"]["id"]
        .as_str()
        .expect("the user's id")
        .to_owned();
    let token = body["access_token"]
        .as_str()
        .expect("an access token (no verification)")
        .to_owned();
    (id, token)
}

/// The function's answer for the caller `token` (or the public key alone), or as `service`.
async fn notes(s: &Stack, query: &str, token: Option<&str>) -> (u16, String, String) {
    let mut req = s
        .http
        .get(format!("{}/functions/v1/notes{query}", s.api))
        .bearer_auth(&s.key);
    if let Some(t) = token {
        req = req.header("chelabase-user", t);
    }
    let resp = req.send().await.expect("the gateway");
    let status = resp.status().as_u16();
    let has_key = resp
        .headers()
        .get("x-has-service-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    (status, has_key, resp.text().await.unwrap_or_default())
}

async fn flow(api: &Api, project: &str) {
    let stamp = &Uuid::new_v4().simple().to_string()[..8];
    // A database in the project; its main branch's public key; app users on.
    let created = api
        .ok(
            reqwest::Method::POST,
            "/v1/databases",
            Some(json!({ "tenant_id": DEMO_TENANT_ID, "project_id": project, "name": format!("fn_{stamp}") })),
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
    let keys = api
        .ok(
            reqwest::Method::GET,
            &format!("/v1/databases/{db}/branches/{db}/keys"),
            None,
            &[200],
        )
        .await;
    let public = keys
        .as_array()
        .and_then(|ks| ks.iter().find(|k| k["kind"] == "public"))
        .and_then(|k| k["key"].as_str())
        .expect("the branch's public key")
        .to_owned();
    api.ok(
        reqwest::Method::PUT,
        &format!("/v1/projects/{project}/app-users"),
        Some(json!({ "database_id": db })),
        &[202],
    )
    .await;
    until(180, "app users aren't enabled", || async {
        let b = api
            .ok(
                reqwest::Method::GET,
                &format!("/v1/projects/{project}/app-users"),
                None,
                &[200],
            )
            .await;
        assert_ne!(b["status"], "failed", "{b}");
        (b["status"] == "enabled").then_some(())
    })
    .await;
    let s = Stack::with(metadata().await, db, BranchId(db.as_uuid()), public);

    // Two app users and a table each sees only their own rows of.
    let (a, token_a) = sign_up(&s, &format!("fn-a-{stamp}@example.com")).await;
    let (b, token_b) = sign_up(&s, &format!("fn-b-{stamp}@example.com")).await;
    api.sql(
        db,
        &format!(
            "CREATE TABLE notes (id bigint generated always as identity primary key,
                 owner uuid NOT NULL DEFAULT chelabase.user_id(), body text NOT NULL);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY notes_own ON notes FOR SELECT TO authenticated USING (owner = chelabase.user_id());
             GRANT SELECT ON notes TO authenticated;
             INSERT INTO notes (owner, body) VALUES ('{a}', 'note of a'), ('{b}', 'note of b');"
        ),
    )
    .await;

    // Step 6: as the caller, through the binding.
    s.deploy("notes", NOTES).await;
    let (status, has_key, body) = s_within(&s, Some(&token_a), |st, b| {
        st == 200 && b.contains("note of a")
    })
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("note of a") && !body.contains("note of b"),
        "app user a: {body}"
    );
    assert_eq!(has_key, "false", "no run-as role, no service key");
    let (status, _, body) = notes(&s, "", Some(&token_b)).await;
    assert!(
        status == 200 && body.contains("note of b") && !body.contains("note of a"),
        "app user b: {status} {body}"
    );
    let (status, _, body) = notes(&s, "", None).await;
    assert!(
        !body.contains("note of a") && !body.contains("note of b"),
        "the public key alone: {status} {body}"
    );

    // Run-as: a role that reads every note; the function's service key queries as it.
    let oid = api
        .sql(
            db,
            "CREATE ROLE fn_reporter NOLOGIN; GRANT fn_reporter TO authenticator;
             GRANT USAGE ON SCHEMA public TO fn_reporter; GRANT SELECT ON notes TO fn_reporter;
             CREATE POLICY notes_reporter ON notes FOR SELECT TO fn_reporter USING (true);
             SELECT oid::text FROM pg_roles WHERE rolname = 'fn_reporter';",
        )
        .await
        .into_iter()
        .find_map(|r| r.into_iter().next().flatten())
        .expect("fn_reporter's oid");
    let function = s
        .metadata
        .list_functions(db)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.name == "notes")
        .unwrap()
        .id;
    let key = generate(DataKeyKind::Secret);
    let run_as = RunAs {
        role: "fn_reporter".into(),
        role_oid: oid.parse().unwrap(),
        role_timeline: None,
        role_bypass_rls: false,
        key_id: Uuid::new_v4(),
        prefix: key.prefix.clone(),
        sha256: key.sha256,
        secret: key.key.clone(),
    };
    assert!(
        s.metadata
            .set_run_as(s.branch, function, Some(&run_as))
            .await
            .unwrap()
    );
    s.changed("notes").await;
    let (status, has_key, body) = s_within_query(&s, "?as=service", |st, b| {
        st == 200 && b.contains("note of a") && b.contains("note of b")
    })
    .await;
    assert_eq!(has_key, "true", "{status} {body}");
    assert!(
        status == 200 && body.contains("note of a") && body.contains("note of b"),
        "as fn_reporter: {status} {body}"
    );
}

async fn s_within(
    s: &Stack,
    token: Option<&str>,
    done: impl Fn(u16, &str) -> bool,
) -> (u16, String, String) {
    poll(|| notes(s, "", token), done).await
}

async fn s_within_query(
    s: &Stack,
    query: &str,
    done: impl Fn(u16, &str) -> bool,
) -> (u16, String, String) {
    poll(|| notes(s, query, None), done).await
}

/// Calls `f` until `done` holds on its answer, for at most 2 s (a change applies within that).
async fn poll<F: std::future::Future<Output = (u16, String, String)>>(
    f: impl Fn() -> F,
    done: impl Fn(u16, &str) -> bool,
) -> (u16, String, String) {
    let start = Instant::now();
    loop {
        let got = f().await;
        if done(got.0, &got.2) || start.elapsed() > super::WITHIN {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack with the gateway binding (#183): just functions-e2e"]
async fn app_user_rls_and_run_as_through_the_binding() {
    let api = Api {
        http: reqwest::Client::new(),
        base: env("CHELABASE_ENTRY_API_URL")
            .trim_end_matches('/')
            .to_owned(),
    };
    let name = format!(
        "functions-e2e-{}",
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
        let api = Api {
            http: api.http.clone(),
            base: api.base.clone(),
        };
        tokio::spawn(async move { flow(&api, &id).await })
    };
    let result = run.await;
    // Deleting the project deletes its databases and its app-user pool.
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
