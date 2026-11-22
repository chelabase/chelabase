//! MCP core m11, live: `execute` end to end through the real `mcp-runtime`, the gateway and the
//! orchestrator on the workbench (m12, #356). Run with
//! `just workbench test -p mcp --test execute_live -- --run-ignored only`.
//!
//! Each test provisions what it uses, as the other workbench live tests do, and removes it
//! afterwards, pass or fail:
//! - the Admin's token: `chelabase dev token` (`CHELABASE_CLI`), the demo tenant's admin,
//!   fetched for every call (a run needs a token with at least a minute left);
//! - a branch: a project of the demo tenant and a database in it (its main branch);
//! - a Viewer: invited by link into that project with a `viewer` grant, the invite accepted
//!   and a token taken from the session.
//!
//! Setup and checks of the data go through the management API as the Admin, not through
//! `execute`, so they don't depend on what's under test.
//!
//! Overrides (each skips its provisioning): `CHELABASE_MCP_LIVE_TOKEN` (the Admin's token),
//! `CHELABASE_MCP_LIVE_BRANCH` (`/v1/databases/<id>/branches/<branch_id>` of a branch the
//! Admin can write) and `CHELABASE_MCP_LIVE_VIEWER_TOKEN` (a Viewer of that branch's project).
//! The runner sets `CHELABASE_MCP_LIVE_URL`, the gateway's `/mcp`.

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const VERSION: &str = "2026-07-28";
const DEMO_TENANT_ID: &str = "00000000-0000-4000-8000-000000000001";
/// Passes auth's password rule.
const STRONG: &str = "Live-pass-12!";
/// SQLSTATE `read_only_sql_transaction`.
const READ_ONLY: &str = "25006";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn need(name: &str) -> String {
    env(name).unwrap_or_else(|| panic!("{name} is unset: run through `just workbench test`"))
}

fn http() -> chelabase_http::Client {
    chelabase_http::client_builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap()
}

/// The gateway's API entry.
fn gateway() -> String {
    need("CHELABASE_ENTRY_API_URL")
        .trim_end_matches('/')
        .to_owned()
}

fn workspace_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A fresh token of the Admin: `CHELABASE_MCP_LIVE_TOKEN`, else `chelabase dev token`'s.
async fn admin_token() -> String {
    if let Some(token) = env("CHELABASE_MCP_LIVE_TOKEN") {
        return token;
    }
    let cli = need("CHELABASE_CLI");
    let checkout = env("CHELABASE_CHECKOUT").map_or_else(workspace_root, Into::into);
    let out = tokio::process::Command::new(cli)
        .args(["dev", "token"])
        .current_dir(checkout)
        .stdin(Stdio::null())
        .output()
        .await
        .expect("run chelabase dev token");
    assert!(
        out.status.success(),
        "chelabase dev token: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("a token")
        .trim()
        .to_owned()
}

/// A management API call as the Admin: the status and the body (JSON, else text).
async fn api(method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut request = http()
        .request(method, format!("{}{path}", gateway()))
        .bearer_auth(admin_token().await);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let resp = request.send().await.expect("the gateway answers");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, body)
}

/// One NDJSON line of a SQL run.
fn events(ndjson: &str) -> Vec<Value> {
    ndjson
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not NDJSON: {ndjson}")))
        .collect()
}

/// Runs `sql` on `branch` as the Admin, through the management API (not `execute`): the rows of
/// every statement, in order. Panics on any error.
async fn admin_sql(branch: &str, sql: &str) -> Vec<Vec<Option<String>>> {
    let (status, body) = api(
        Method::POST,
        &format!("{branch}/sql"),
        Some(json!({"run_id": Uuid::new_v4(), "sql": sql})),
    )
    .await;
    assert_eq!(status, 200, "{sql}: {body}");
    let text = body
        .as_str()
        .map_or_else(|| body.to_string(), str::to_owned);
    let mut rows = vec![];
    for event in events(&text) {
        assert_ne!(event["t"], "error", "{sql}: {text}");
        if event["t"] == "rows" {
            rows.extend(
                serde_json::from_value::<Vec<Vec<Option<String>>>>(event["rows"].clone()).unwrap(),
            );
        }
    }
    rows
}

/// Where a test runs: a branch and its project.
#[derive(Clone)]
struct Place {
    /// `/v1/databases/<id>/branches/<branch_id>`.
    branch: String,
    project: String,
}

/// Runs `checks` on a branch: `CHELABASE_MCP_LIVE_BRANCH`, else the main branch of a new
/// database in a new project, both deleted afterwards, pass or fail.
async fn with_branch<F, Fut>(checks: F)
where
    F: FnOnce(Place) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    if let Some(branch) = env("CHELABASE_MCP_LIVE_BRANCH") {
        let database = branch
            .strip_prefix("/v1/databases/")
            .and_then(|r| r.split('/').next())
            .expect("CHELABASE_MCP_LIVE_BRANCH: /v1/databases/<id>/branches/<branch_id>");
        let (status, d) = api(Method::GET, &format!("/v1/databases/{database}"), None).await;
        assert_eq!(status, 200, "the override's database: {d}");
        let project = d["project_id"].as_str().expect("its project").to_owned();
        return checks(Place { branch, project }).await;
    }
    let tag = &Uuid::new_v4().simple().to_string()[..8];
    let (status, project) = api(
        Method::POST,
        "/v1/projects",
        Some(json!({"name": format!("mcp-live-{tag}")})),
    )
    .await;
    assert_eq!(status, 201, "project: {project}");
    let project_id = project["id"].as_str().unwrap().to_owned();
    let slug = project["slug"].as_str().unwrap().to_owned();
    let (status, created) = api(
        Method::POST,
        "/v1/databases",
        Some(json!({"tenant_id": DEMO_TENANT_ID, "project_id": project_id, "name": format!("mcp_{tag}")})),
    )
    .await;
    let database = created["database_id"].as_str().map(str::to_owned);
    let setup = async {
        if status != 201 && status != 202 {
            return Err(format!("database: {status} {created}"));
        }
        let db = database.clone().unwrap();
        for _ in 0..180 {
            let (_, d) = api(Method::GET, &format!("/v1/databases/{db}"), None).await;
            match d["status"].as_str() {
                Some("ready") => return Ok(format!("/v1/databases/{db}/branches/{db}")),
                Some("failed") => return Err(format!("the database failed: {d}")),
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
        Err("the database isn't ready after 180 s".to_owned())
    }
    .await;
    let outcome = match setup {
        Ok(branch) => Ok(tokio::spawn(checks(Place {
            branch,
            project: project_id.clone(),
        }))
        .await),
        Err(e) => Err(e),
    };
    if let Some(db) = &database {
        let _ = api(Method::DELETE, &format!("/v1/databases/{db}"), None).await;
    }
    let _ = api(
        Method::DELETE,
        &format!("/v1/projects/{project_id}"),
        Some(json!({"confirm": slug})),
    )
    .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => std::panic::resume_unwind(panic.into_panic()),
        Err(e) => panic!("{e}"),
    }
}

fn query_param(url: &str, name: &str) -> String {
    let query = url.split_once('?').expect("the link has a query").1;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {url}"))
        .to_owned()
}

/// `POST /auth/v1/<path>` as Studio's session calls make it: the status, the body and the
/// `chelabase_session=…` cookie if one was set.
async fn session_call(
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (u16, Value, Option<String>) {
    let mut request = http()
        .post(format!("{}/auth/v1/{path}", gateway()))
        .header("origin", need("CHELABASE_ENTRY_STUDIO_URL"))
        .header("x-chelabase-studio", "1");
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let resp = request.send().await.expect("the gateway answers");
    let status = resp.status().as_u16();
    let cookie = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter(|c| c.starts_with("chelabase_session=") && !c.starts_with("chelabase_session=;"))
        .map(|c| c.split(';').next().unwrap().to_owned())
        .next();
    (status, resp.json().await.unwrap_or(Value::Null), cookie)
}

/// Runs `checks` with a Viewer's token for `place`'s project: `CHELABASE_MCP_LIVE_VIEWER_TOKEN`,
/// else a person invited by link with a `viewer` grant, removed afterwards, pass or fail.
async fn with_viewer<F, Fut>(place: Place, checks: F)
where
    F: FnOnce(Place, String) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if let Some(token) = env("CHELABASE_MCP_LIVE_VIEWER_TOKEN") {
        return checks(place, token).await;
    }
    let email = format!(
        "mcp-live-viewer-{}@example.com",
        &Uuid::new_v4().simple().to_string()[..12]
    );
    let (status, body) = api(
        Method::POST,
        "/v1/invites",
        Some(json!({
            "email": email,
            "grants": [{"project_id": place.project, "platform_role": "viewer", "pg_roles": []}],
            "delivery": "link",
        })),
    )
    .await;
    assert_eq!(status, 201, "invite {email}: {body}");
    let member = body["member"]["id"].as_str().unwrap().to_owned();
    let url = body["invite_url"]
        .as_str()
        .unwrap_or_else(|| panic!("no invite_url: {body}"))
        .to_owned();
    let outcome = tokio::spawn(async move {
        let accept = json!({
            "user": query_param(&url, "user"),
            "t": query_param(&url, "t"),
            "code": query_param(&url, "code"),
            "password": STRONG,
        });
        let (status, body, cookie) = session_call("accept-invite", None, Some(accept)).await;
        assert_eq!(status, 200, "accept the invite: {body}");
        let cookie = cookie.expect("the session cookie");
        let (status, body, _) = session_call("token", Some(&cookie), None).await;
        assert_eq!(status, 200, "the viewer's token: {body}");
        let token = body["access_token"].as_str().expect("a token").to_owned();
        checks(place, token).await;
    })
    .await;
    // Removing the member also bans its identity user (auth's rule); the address is unique.
    let _ = api(Method::DELETE, &format!("/v1/members/{member}"), None).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

// ---- execute ----

async fn execute(token: &str, code: &str, destructive: bool, read_only: bool) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "execute",
            "arguments": {"code": code, "destructive": destructive},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": VERSION,
                "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let mut request = http()
        .post(need("CHELABASE_MCP_LIVE_URL"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", VERSION)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "execute")
        .bearer_auth(token);
    if read_only {
        request = request.header("chelabase-mcp-read-only", "true");
    }
    let resp = request.body(body.to_string()).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    v["result"].clone()
}

/// `execute` as the Admin.
async fn execute_admin(code: &str, destructive: bool, read_only: bool) -> Value {
    execute(&admin_token().await, code, destructive, read_only).await
}

fn value(result: &Value) -> Value {
    assert_ne!(result["isError"], true, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn tool_error(result: &Value) -> Value {
    assert_eq!(result["isError"], true, "{result}");
    serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap()["error"]
        .clone()
}

/// Code that runs `sql` on `branch` through `chelabase.request` and returns the API's status and
/// body (NDJSON text, or the refusal's JSON).
fn run_sql_code(branch: &str, sql: &str) -> String {
    let body = json!({"run_id": Uuid::new_v4(), "sql": sql});
    format!(
        "async () => {{ const r = await chelabase.request({{ method: 'POST', path: '{branch}/sql', \
         body: {body} }}); return {{ status: r.status, body: r.body }}; }}"
    )
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn execute_lists_and_reads_as_the_member() {
    let code = "async () => { \
        const list = await chelabase.request({ method: 'GET', path: '/v1/projects' }); \
        const id = list.body[0].id; \
        const grants = await chelabase.request({ method: 'GET', path: `/v1/projects/${id}/grants` }); \
        return { listed: list.status, read: grants.status, id }; }";
    let v = value(&execute_admin(code, false, false).await);
    assert_eq!(v["listed"], 200, "{v}");
    assert_eq!(v["read"], 200, "{v}");
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn a_viewer_cannot_write_through_execute() {
    with_branch(|place| {
        with_viewer(place, |place, viewer| async move {
            let code = format!(
                "async () => {{ const r = await chelabase.request({{ method: 'DELETE', path: '{}' }}); \
                 return {{ status: r.status, code: r.body?.error?.code }}; }}",
                place.branch
            );
            let v = value(&execute(&viewer, &code, true, false).await);
            assert_eq!(v["status"], 403, "the API's answer, passed through: {v}");
            assert_eq!(v["code"], "forbidden", "{v}");
            let (status, _) = api(Method::GET, &place.branch, None).await;
            assert_eq!(status, 200, "the branch is still there");
        })
    })
    .await;
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn delete_needs_the_flag() {
    // Refused before the API: the branch needn't exist.
    let branch = format!(
        "/v1/databases/{}/branches/{}",
        Uuid::new_v4(),
        Uuid::new_v4()
    );
    let code = format!(
        "async () => {{ try {{ await chelabase.request({{ method: 'DELETE', path: '{branch}' }}); }} \
         catch (e) {{}} return 'swallowed'; }}"
    );
    let error = tool_error(&execute_admin(&code, false, false).await);
    assert_eq!(error["code"], "destructive_flag_required", "{error}");
    assert_eq!(error["operations"], json!(["deleteBranch"]));
}

/// A table `t` with one row and a sequence `s` nothing has called, made on `place` by the Admin
/// through the management API (`t` and `s` are named per run).
async fn seed(place: &Place) -> (String, String) {
    let tag = Uuid::new_v4().simple().to_string();
    let (t, s) = (format!("mcp_live_t_{tag}"), format!("mcp_live_s_{tag}"));
    admin_sql(
        &place.branch,
        &format!(
            "create table public.{t} (id int); insert into public.{t} values (1); \
             create sequence public.{s}"
        ),
    )
    .await;
    (t, s)
}

/// What a write must meet in a read-only run.
enum Refusal {
    /// The orchestrator's parse refuses the script before anything runs.
    Parse,
    /// Postgres refuses it inside the read-only transaction.
    Postgres,
}

/// Statements that write, each with how a read-only run refuses it.
fn writes(t: &str, s: &str) -> Vec<(String, Refusal)> {
    vec![
        (
            format!("create table public.{t}_new (id int)"),
            Refusal::Parse,
        ),
        (format!("insert into public.{t} values (2)"), Refusal::Parse),
        (format!("update public.{t} set id = 5"), Refusal::Parse),
        (
            format!(
                "select pg_catalog.set_config('mcp.live', 'x', false); \
                 create table public.{t}_two (id int)"
            ),
            Refusal::Parse,
        ),
        (
            format!("select pg_catalog.nextval('public.{s}')"),
            Refusal::Postgres,
        ),
        (
            format!("select pg_catalog.setval('public.{s}', 42)"),
            Refusal::Postgres,
        ),
    ]
}

/// The sandbox's view of a SQL run (`{status, body}`) shows `refusal`.
fn assert_refused(sql: &str, v: &Value, refusal: &Refusal) {
    match refusal {
        Refusal::Parse => {
            assert_eq!(v["status"], 422, "{sql}: {v}");
            assert_eq!(
                v["body"]["error"]["code"], "read_only_script_refused",
                "{sql}: {v}"
            );
        }
        Refusal::Postgres => {
            assert_eq!(v["status"], 200, "{sql}: {v}");
            let text = v["body"].as_str().unwrap_or_else(|| panic!("{sql}: {v}"));
            let error = events(text)
                .into_iter()
                .find(|e| e["t"] == "error")
                .unwrap_or_else(|| panic!("{sql}: no error: {text}"));
            assert_eq!(error["code"], READ_ONLY, "{sql}: {text}");
        }
    }
}

/// The data is as [`seed`] left it.
async fn assert_unchanged(place: &Place, t: &str, s: &str) {
    let rows = admin_sql(
        &place.branch,
        &format!(
            "select (select string_agg(id::text, ',') from public.{t}), \
             (select last_value || '/' || is_called from public.{s}), \
             to_regclass('public.{t}_new') is null and to_regclass('public.{t}_two') is null"
        ),
    )
    .await;
    assert_eq!(
        rows,
        vec![vec![
            Some("1".to_owned()),
            Some("1/false".to_owned()),
            Some("t".to_owned())
        ]],
        "the data changed"
    );
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn sql_without_the_flag_cannot_write() {
    with_branch(|place| async move {
        let (t, s) = seed(&place).await;
        for (sql, refusal) in writes(&t, &s) {
            let v = value(&execute_admin(&run_sql_code(&place.branch, &sql), false, false).await);
            assert_refused(&sql, &v, &refusal);
        }
        assert_unchanged(&place, &t, &s).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn read_only_connection_cannot_write_through_sql() {
    with_branch(|place| async move {
        let (t, s) = seed(&place).await;
        for (sql, refusal) in writes(&t, &s) {
            // The flag is ignored on a read-only connection.
            let v = value(&execute_admin(&run_sql_code(&place.branch, &sql), true, true).await);
            assert_refused(&sql, &v, &refusal);
        }
        // A write operation is refused before the API.
        let code = format!(
            "async () => (await chelabase.request({{ method: 'DELETE', path: '{}' }})).status",
            place.branch
        );
        let error = tool_error(&execute_admin(&code, true, true).await);
        assert_eq!(error["code"], "read_only_connection", "{error}");
        assert_unchanged(&place, &t, &s).await;
        let (status, _) = api(Method::GET, &place.branch, None).await;
        assert_eq!(status, 200, "the branch is still there");
    })
    .await;
}
