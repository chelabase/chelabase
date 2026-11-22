//! MCP core m11, live: `execute` end to end through the real `mcp-runtime`, the gateway and the
//! orchestrator on the workbench (m12, #356). Ignored until m12 puts mcp and mcp-runtime on
//! the workbench; run with
//! `just workbench test -p mcp --test execute_live -- --run-ignored only`.
//!
//! The workbench provides:
//! - `CHELABASE_MCP_LIVE_URL`: the gateway's `/mcp` URL;
//! - `CHELABASE_MCP_LIVE_TOKEN`: a member token of a project Admin;
//! - `CHELABASE_MCP_LIVE_VIEWER_TOKEN`: a member token of a Viewer of the same project;
//! - `CHELABASE_MCP_LIVE_BRANCH`: `/v1/databases/<id>/branches/<branch_id>` of a branch the
//!   Admin owns (each SQL test makes its own table, named per run).

use serde_json::{Value, json};

const VERSION: &str = "2026-07-28";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name}: run through `just workbench test`"))
}

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
    let mut request = chelabase_http::client_builder()
        .build()
        .unwrap()
        .post(env("CHELABASE_MCP_LIVE_URL"))
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

fn value(result: &Value) -> Value {
    assert_ne!(result["isError"], true, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn tool_error(result: &Value) -> Value {
    assert_eq!(result["isError"], true, "{result}");
    serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap()["error"]
        .clone()
}

fn table() -> String {
    format!("mcp_live_{}", uuid::Uuid::new_v4().simple())
}

/// Runs `sql` on the live branch with `destructive` (as the Admin): the NDJSON text.
fn run_sql_code(sql: &str) -> String {
    let branch = env("CHELABASE_MCP_LIVE_BRANCH");
    format!(
        "async () => {{ const r = await chelabase.request({{ method: 'POST', path: '{branch}/sql', \
         body: {{ sql: {} }} }}); return {{ status: r.status, body: r.body }}; }}",
        serde_json::to_string(sql).unwrap()
    )
}

/// Whether `table` exists, read through `execute` (a read-only run).
async fn exists(table: &str) -> bool {
    let result = execute(
        &env("CHELABASE_MCP_LIVE_TOKEN"),
        &run_sql_code(&format!(
            "select to_regclass('public.{table}') is not null as e"
        )),
        false,
        false,
    )
    .await;
    let v = value(&result);
    assert_eq!(v["status"], 200, "{v}");
    v["body"].as_str().unwrap().contains("true")
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn execute_lists_and_reads_as_the_member() {
    let code = "async () => { \
        const list = await chelabase.request({ method: 'GET', path: '/v1/projects' }); \
        const id = list.body[0].id; \
        const grants = await chelabase.request({ method: 'GET', path: `/v1/projects/${id}/grants` }); \
        return { listed: list.status, read: grants.status, id }; }";
    let result = execute(&env("CHELABASE_MCP_LIVE_TOKEN"), code, false, false).await;
    let v = value(&result);
    assert_eq!(v["listed"], 200, "{v}");
    assert_eq!(v["read"], 200, "{v}");
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn a_viewer_cannot_write_through_execute() {
    let branch = env("CHELABASE_MCP_LIVE_BRANCH");
    let code = format!(
        "async () => {{ const r = await chelabase.request({{ method: 'DELETE', path: '{branch}' }}); \
         return {{ status: r.status, code: r.body?.error?.code }}; }}"
    );
    let result = execute(&env("CHELABASE_MCP_LIVE_VIEWER_TOKEN"), &code, true, false).await;
    let v = value(&result);
    assert_eq!(v["status"], 403, "the API's answer, passed through: {v}");
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn delete_needs_the_flag() {
    let branch = env("CHELABASE_MCP_LIVE_BRANCH");
    let code = format!(
        "async () => {{ try {{ await chelabase.request({{ method: 'DELETE', path: '{branch}' }}); }} \
         catch (e) {{}} return 'swallowed'; }}"
    );
    let result = execute(&env("CHELABASE_MCP_LIVE_TOKEN"), &code, false, false).await;
    let error = tool_error(&result);
    assert_eq!(error["code"], "destructive_flag_required");
    assert_eq!(error["operations"], json!(["deleteBranch"]));
}

/// Statements that write, each refused by Postgres in a read-only run.
fn writes(table: &str) -> Vec<String> {
    vec![
        format!("create table public.{table} (id int)"),
        format!("insert into public.{table} values (1)"),
        "select pg_catalog.nextval('public.mcp_live_seq')".to_owned(),
        format!(
            "select pg_catalog.set_config('mcp.live', '{table}', false); create table public.{table} (id int)"
        ),
    ]
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn sql_without_the_flag_cannot_write() {
    let t = table();
    for sql in writes(&t) {
        let result = execute(
            &env("CHELABASE_MCP_LIVE_TOKEN"),
            &run_sql_code(&sql),
            false,
            false,
        )
        .await;
        let v = value(&result);
        let text = v["body"].to_string();
        assert!(
            v["status"] != 200 || text.contains("25006") || text.contains("read_only"),
            "{sql}: {v}"
        );
    }
    assert!(!exists(&t).await, "the table was created");
}

#[tokio::test]
#[ignore = "live: needs mcp and mcp-runtime on the workbench (m12, #356)"]
async fn read_only_connection_cannot_write_through_sql() {
    let t = table();
    for sql in writes(&t) {
        // The flag is ignored on a read-only connection.
        let result = execute(
            &env("CHELABASE_MCP_LIVE_TOKEN"),
            &run_sql_code(&sql),
            true,
            true,
        )
        .await;
        let v = value(&result);
        let text = v["body"].to_string();
        assert!(
            v["status"] != 200 || text.contains("25006") || text.contains("read_only"),
            "{sql}: {v}"
        );
    }
    // A write operation is refused before the API.
    let branch = env("CHELABASE_MCP_LIVE_BRANCH");
    let code = format!(
        "async () => (await chelabase.request({{ method: 'DELETE', path: '{branch}' }})).status"
    );
    let result = execute(&env("CHELABASE_MCP_LIVE_TOKEN"), &code, true, true).await;
    assert_eq!(tool_error(&result)["code"], "read_only_connection");
    assert!(!exists(&t).await, "the table was created");
}
