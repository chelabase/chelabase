//! The two tools, `search` and `execute`: their definitions, their argument checks and how a
//! call reaches the sandbox.

use std::sync::{Arc, LazyLock};
use std::time::Instant;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::app::{Audit, audit};
use crate::auth::Caller;
use crate::sandbox::{ErrorCode, Kind, Outcome, Run, Sandbox};

/// The largest `code` a tool takes (64 KiB).
pub const MAX_CODE_BYTES: usize = 64 * 1024;

/// The committed spec bundle the server describes (m1, #345).
const BUNDLE: &str = include_str!("../../../openapi/mcp.json");

struct BundleFacts {
    hash: String,
    tags: Vec<String>,
}

static FACTS: LazyLock<BundleFacts> = LazyLock::new(|| {
    let bundle: Value = serde_json::from_str(BUNDLE).expect("openapi/mcp.json is JSON");
    let hash = bundle["info"]["x-spec-hash"]
        .as_str()
        .expect("openapi/mcp.json has info.x-spec-hash")
        .to_owned();
    let mut tags: Vec<String> = bundle["paths"]
        .as_object()
        .into_iter()
        .flat_map(|paths| paths.values())
        .filter_map(Value::as_object)
        .flat_map(|ops| ops.values())
        .filter_map(|op| op["tags"].as_array())
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_owned))
        .collect();
    tags.sort();
    tags.dedup();
    BundleFacts { hash, tags }
});

/// The bundle's `info.x-spec-hash`.
pub fn spec_hash() -> &'static str {
    &FACTS.hash
}

const SEARCH_INTRO: &str = "Search the Chelabase API before you call it. `spec` is the Chelabase API as an \
OpenAPI 3.1 object with every `$ref` resolved inline. Write an async arrow function that returns only what \
you need: the whole spec is far too large to return.";

const SEARCH_SHAPE: &str = r#"What your code sees:

declare const spec: {
  openapi: string;
  info: { title: string; version: string; "x-spec-hash": string };
  paths: Record<string, Partial<Record<"get" | "post" | "put" | "patch" | "delete", {
    operationId: string;
    summary?: string;
    description?: string;
    tags: string[];
    "x-class": "read" | "write" | "destructive";
    parameters?: { name: string; in: "path" | "query" | "header"; required?: boolean; schema: object }[];
    requestBody?: { content: Record<string, { schema: object }> };
    responses: Record<string, { description: string; content?: Record<string, { schema: object }> }>;
    "x-errors"?: Record<string, string[]>;
  }>>>;
  "x-error-envelope": object;
};"#;

const SEARCH_EXAMPLES: &str = r#"Examples:

// Every operation under one tag, with its class
async () => Object.entries(spec.paths).flatMap(([path, ops]) =>
  Object.entries(ops)
    .filter(([, op]) => op.tags.includes("branches"))
    .map(([method, op]) => ({ method: method.toUpperCase(), path, summary: op.summary, class: op["x-class"] })))

// One operation's parameters and request schema
async () => {
  const op = spec.paths["/v1/databases/{id}/branches"].post;
  return { parameters: op.parameters, body: op.requestBody?.content["application/json"].schema };
}

// Operations whose summary mentions a word
async () => Object.entries(spec.paths).flatMap(([path, ops]) =>
  Object.entries(ops)
    .filter(([, op]) => /role/i.test(op.summary ?? ""))
    .map(([method, op]) => `${method.toUpperCase()} ${path}: ${op.summary}`))"#;

const EXECUTE_INTRO: &str = "Call the Chelabase API as the signed-in member. Write an async arrow function \
that calls `chelabase.request` and returns only what you need.";

const EXECUTE_SHAPE: &str = r#"What your code sees:

declare const chelabase: {
  request(req: {
    method: "GET" | "POST" | "PUT" | "PATCH" | "DELETE";
    path: string;                    // a path template from `search`, with its parameters filled in
    query?: Record<string, string | number | boolean>;
    body?: unknown;                  // sent as JSON
  }): Promise<{ status: number; ok: boolean; body: unknown }>;
};

`request` does not throw on 4xx or 5xx: check `ok` and read the error from `body.error`. Find paths, \
parameters and request schemas with `search` first. Read and write table data through \
`/v1/databases/{id}/branches/{branch_id}/console/query`. Run SQL through \
`/v1/databases/{id}/branches/{branch_id}/sql`: it is read-only in Postgres unless `destructive` is true. \
A read-only script may hold several statements, but no transaction control and no `SET`. Operations \
whose `x-class` is `destructive` also need `destructive: true`. Set `destructive` only when the user \
asked for that change."#;

const EXECUTE_EXAMPLES: &str = r#"Examples:

// List the databases
async () => (await chelabase.request({ method: "GET", path: "/v1/databases" })).body

// Read-only SQL on a branch
async () => {
  const r = await chelabase.request({
    method: "POST",
    path: "/v1/databases/db_123/branches/br_456/sql",
    body: { sql: "select count(*) from orders; select max(created_at) from orders" },
  });
  return r.ok ? r.body : r.body.error;
}

// Create a branch, then report its id
async () => {
  const r = await chelabase.request({ method: "POST", path: "/v1/databases/db_123/branches", body: { name: "preview" } });
  return r.ok ? r.body.id : r.body.error;
}"#;

const READ_ONLY_NOTE: &str =
    "This connection is read-only: every write is refused, whatever `destructive` says.";

fn search_description() -> String {
    format!(
        "{SEARCH_INTRO}\n\n{SEARCH_SHAPE}\n\nTags: {}. `x-class` says what an operation does: `read`, \
`write` or `destructive`; `execute` needs `destructive: true` for a `destructive` one.\n\n{SEARCH_EXAMPLES}",
        FACTS.tags.join(", ")
    )
}

fn execute_description(read_only: bool) -> String {
    let note = if read_only {
        format!("{READ_ONLY_NOTE}\n\n")
    } else {
        String::new()
    };
    format!("{note}{EXECUTE_INTRO}\n\n{EXECUTE_SHAPE}\n\n{EXECUTE_EXAMPLES}")
}

/// The `initialize` (and discovery) instructions.
pub fn instructions(read_only: bool) -> String {
    let mut text = format!(
        "Use `search` to find the Chelabase API operation you need in the OpenAPI spec (x-spec-hash {}), \
then `execute` to call it as the signed-in member with `chelabase.request`. Both tools take an async \
arrow function as `code` and return what it returns, so return only what you need. Set `destructive: true` \
on `execute` only for a destructive change the user asked for.",
        spec_hash()
    );
    if read_only {
        text.push(' ');
        text.push_str(READ_ONLY_NOTE);
    }
    text
}

fn object(value: Value) -> Arc<JsonObject> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => unreachable!("a schema literal is an object"),
    }
}

fn tool(value: Value) -> Tool {
    serde_json::from_value(value).expect("a tool literal matches the protocol's Tool")
}

/// The tool list for a connection.
pub fn definitions(read_only: bool) -> Vec<Tool> {
    let search = tool(json!({
        "name": "search",
        "description": search_description(),
        "inputSchema": object(json!({
            "type": "object",
            "properties": {"code": {"type": "string"}},
            "required": ["code"],
            "additionalProperties": false
        })),
        "annotations": {"readOnlyHint": true}
    }));
    let annotations = if read_only {
        json!({"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false})
    } else {
        json!({"destructiveHint": true, "openWorldHint": false})
    };
    let execute = tool(json!({
        "name": "execute",
        "description": execute_description(read_only),
        "inputSchema": object(json!({
            "type": "object",
            "properties": {"code": {"type": "string"}, "destructive": {"type": "boolean"}},
            "required": ["code"],
            "additionalProperties": false
        })),
        "annotations": annotations
    }));
    vec![search, execute]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    code: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteArgs {
    code: String,
    #[serde(default)]
    destructive: bool,
}

/// A checked call: the tool and its arguments.
struct Call {
    kind: Kind,
    code: String,
    destructive: bool,
}

fn parse(name: &str, arguments: Option<JsonObject>) -> Result<Call, ErrorData> {
    let arguments = Value::Object(arguments.unwrap_or_default());
    // A fixed message: serde's would echo the offending value.
    let shape = if name == "execute" {
        "{code: string, destructive?: boolean}"
    } else {
        "{code: string}"
    };
    let bad = |_: serde_json::Error| {
        ErrorData::invalid_params(format!("{name} takes {shape} and no other keys"), None)
    };
    match name {
        "search" => {
            let a: SearchArgs = serde_json::from_value(arguments).map_err(bad)?;
            Ok(Call {
                kind: Kind::Search,
                code: a.code,
                destructive: false,
            })
        }
        "execute" => {
            let a: ExecuteArgs = serde_json::from_value(arguments).map_err(bad)?;
            Ok(Call {
                kind: Kind::Execute,
                code: a.code,
                destructive: a.destructive,
            })
        }
        _ => Err(ErrorData::invalid_params("unknown tool", None)),
    }
}

fn error_body(code: ErrorCode, message: &str) -> String {
    json!({"error": {"code": code.as_str(), "message": message}}).to_string()
}

/// Runs one `tools/call`: checks the arguments, hands the code to the sandbox and writes the
/// audit line. A bad tool name or argument is a protocol error (`-32602`) and nothing runs.
pub async fn call(
    sandbox: &dyn Sandbox,
    caller: &Caller,
    request_id: &str,
    log_code: bool,
    name: &str,
    arguments: Option<JsonObject>,
) -> Result<CallToolResult, ErrorData> {
    let call = parse(name, arguments)?;
    let started = Instant::now();
    let outcome = if call.code.len() > MAX_CODE_BYTES {
        Outcome::Error {
            code: ErrorCode::InvalidCode,
            message: format!("code is over {MAX_CODE_BYTES} bytes"),
        }
    } else {
        sandbox
            .run(Run {
                kind: call.kind,
                code: call.code.clone(),
                destructive: call.destructive,
                caller: caller.clone(),
            })
            .await
    };
    let (result, outcome_name, result_bytes) = match outcome {
        Outcome::Value(value) => {
            let text = value.to_string();
            let bytes = text.len();
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            if value.is_object() {
                result.structured_content = Some(value);
            }
            (result, "ok", bytes)
        }
        Outcome::Error { code, message } => {
            let body = error_body(code, &message);
            let bytes = body.len();
            (
                CallToolResult::error(vec![ContentBlock::text(body)]),
                code.as_str(),
                bytes,
            )
        }
    };
    audit(
        &Audit {
            request_id,
            caller,
            tool: call.kind.as_str(),
            code_sha256: hex(&Sha256::digest(call.code.as_bytes())),
            code: &call.code,
            destructive: call.destructive,
            duration_ms: started.elapsed().as_millis() as u64,
            outcome: outcome_name,
            result_bytes,
        },
        log_code,
    );
    Ok(result)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
