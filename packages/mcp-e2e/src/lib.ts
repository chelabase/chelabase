// What every check shares: the stack's addresses (set by scripts/mcp-e2e.sh), an MCP session over
// `chelabase mcp`, and builders for the code `search` and `execute` run.
import assert from "node:assert/strict";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

function need(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`${name} is unset: run through \`just mcp-e2e\``);
  return v;
}

export interface Env {
  api: string;
  cli: string;
  database: string;
  /** The main branch's id (a path is `/v1/databases/<database>/branches/<branch>`). */
  branch: string;
  table: string;
  adminConfig: string;
  viewerConfig: string;
}

export function env(): Env {
  return {
    api: need("MCP_E2E_API"),
    cli: need("MCP_E2E_CLI"),
    database: need("MCP_E2E_DATABASE"),
    branch: need("MCP_E2E_BRANCH"),
    table: need("MCP_E2E_TABLE"),
    adminConfig: need("MCP_E2E_ADMIN_CONFIG"),
    viewerConfig: need("MCP_E2E_VIEWER_CONFIG"),
  };
}

/** What a tool call answered. */
export interface Outcome {
  isError: boolean;
  /** The first content block's text. */
  text: string;
  /** The text as JSON when it parses, else undefined. */
  value: any;
  /** `error` of an error answer: `{code, message, ...}`. */
  error: any;
  /** Every content block's text (the first is the result, a later one the sandbox's console). */
  blocks: string[];
}

export class Session {
  readonly client: Client;

  constructor(client: Client) {
    this.client = client;
  }

  /** A tool call: errors the tool reports come back as an Outcome, not as a throw. */
  async call(
    name: "search" | "execute",
    args: { code: string; destructive?: boolean },
  ): Promise<Outcome> {
    const result = await this.client.callTool({ name, arguments: args }, undefined, {
      timeout: 120_000,
    });
    const blocks = (result.content as { type: string; text?: string }[])
      .filter((b) => b.type === "text")
      .map((b) => b.text ?? "");
    const text = blocks[0] ?? "";
    let value: any;
    try {
      value = JSON.parse(text);
    } catch {
      value = undefined;
    }
    const isError = result.isError === true;
    return { isError, text, value, error: isError ? value?.error : undefined, blocks };
  }

  search(code: string): Promise<Outcome> {
    return this.call("search", { code });
  }

  execute(code: string, destructive = false): Promise<Outcome> {
    return this.call("execute", { code, destructive });
  }

  /** `search` that must succeed: the value the code returned. */
  async find(code: string): Promise<any> {
    const out = await this.search(code);
    assert.equal(out.isError, false, `search failed: ${out.text}`);
    return out.value;
  }

  /** `execute` that must succeed: the value the code returned. */
  async run(code: string, destructive = false): Promise<any> {
    const out = await this.execute(code, destructive);
    assert.equal(out.isError, false, `execute failed: ${out.text}`);
    return out.value;
  }

  close(): Promise<void> {
    return this.client.close();
  }
}

/** The CLI's bridge as an MCP server over stdio, signed in from `configDir`. */
export async function connect(
  e: Env,
  configDir: string,
  opts: { readOnly?: boolean } = {},
): Promise<Session> {
  const transport = new StdioClientTransport({
    command: e.cli,
    args: ["mcp", "--url", e.api, ...(opts.readOnly ? ["--read-only"] : [])],
    env: { CHELABASE_CONFIG_DIR: configDir },
    stderr: "inherit",
  });
  const client = new Client({ name: "mcp-e2e", version: "1.0.0" });
  await client.connect(transport);
  return new Session(client);
}

export const admin = (e: Env, opts?: { readOnly?: boolean }) => connect(e, e.adminConfig, opts);
export const viewer = (e: Env, opts?: { readOnly?: boolean }) => connect(e, e.viewerConfig, opts);

/** Code that makes one `chelabase.request` and returns `{status, body}`. */
export function request(req: {
  method: string;
  path: string;
  body?: unknown;
  query?: Record<string, string | number | boolean>;
}): string {
  return `async () => { const r = await chelabase.request(${JSON.stringify(req)}); return { status: r.status, body: r.body }; }`;
}

export const branchPath = (e: Env, branch = e.branch) =>
  `/v1/databases/${e.database}/branches/${branch}`;

/** Code for a data API request through the console (`consoleQuery`). */
export function consoleCode(path: string, tree: object): string {
  return request({ method: "POST", path: `${path}/console/query`, body: { tree } });
}

/** Code for `runSql`. */
export function sqlCode(path: string, sql: string): string {
  return request({
    method: "POST",
    path: `${path}/sql`,
    body: { run_id: crypto.randomUUID(), sql },
  });
}

/** The rows of the table on the branch, by id, read with a full connection. */
export async function ids(s: Session, e: Env, table = e.table): Promise<number[]> {
  const v = await s.run(
    consoleCode(branchPath(e), { v: 1, op: "select", from: table, orderBy: [{ col: "id" }] }),
  );
  assert.equal(v.status, 200, JSON.stringify(v));
  return (v.body as { id: number }[]).map((r) => r.id);
}

/** Inserts a row through the console as the session's member. */
export async function insert(s: Session, e: Env, id: number, label: string): Promise<any> {
  return s.run(
    consoleCode(branchPath(e), {
      v: 1,
      op: "insert",
      into: e.table,
      values: [{ id, label }],
    }),
  );
}

/** The events of a SQL run's NDJSON answer. */
export function events(body: unknown): any[] {
  assert.equal(typeof body, "string", `not an NDJSON body: ${JSON.stringify(body)}`);
  return (body as string)
    .split("\n")
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l));
}

export interface Check {
  name: string;
  run(e: Env): Promise<void>;
}
