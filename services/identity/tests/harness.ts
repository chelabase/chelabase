// Shared test harness: a real Postgres from CHELABASE_IDENTITY_TEST_DB_URL (the suites skip
// without one) and an in-process identity server with fresh caller tokens.
import { randomBytes, randomUUID } from "node:crypto";
import pg from "pg";
import { describe } from "vitest";
import type { Caller, Config } from "../src/config.ts";
import { startServer, type RunningServer } from "../src/server.ts";
import { ADMIN_FIXTURE } from "./admin-fixture.ts";

export const TEST_DB_URL = process.env.CHELABASE_IDENTITY_TEST_DB_URL;
export const describeDb = describe.skipIf(!TEST_DB_URL);


export interface Harness {
  server: RunningServer;
  tokens: Record<Caller, string>;
  db: pg.Pool;
  close(): Promise<void>;
}

export async function startHarness(overrides: Partial<Config> = {}): Promise<Harness> {
  const db = new pg.Pool({ connectionString: TEST_DB_URL, max: 2 });
  await db.query(ADMIN_FIXTURE);
  const tokens = {
    auth: randomBytes(32).toString("hex"),
    orchestrator: randomBytes(32).toString("hex"),
    worker: randomBytes(32).toString("hex"),
  };
  const server = await startServer({
    databaseUrl: TEST_DB_URL!,
    secret: randomBytes(32).toString("hex"),
    callerTokens: new Map(Object.entries(tokens) as [Caller, string][]),
    poolCacheSize: 16,
    host: "127.0.0.1",
    port: 0,
    ...overrides,
  });
  return {
    server,
    tokens,
    db,
    async close() {
      await server.close();
      await db.end();
    },
  };
}

export interface Reply {
  status: number;
  body: any;
}

/** One request. `auth` is a caller name (its token is used), a raw header value, or null. */
export async function call(
  h: Harness,
  method: string,
  path: string,
  options: { as?: Caller | null; authorization?: string; headers?: Record<string, string>; body?: unknown } = {},
): Promise<Reply> {
  const headers: Record<string, string> = { ...options.headers };
  const as = options.as === undefined ? "auth" : options.as;
  if (options.authorization !== undefined) headers.authorization = options.authorization;
  else if (as !== null) headers.authorization = `Bearer ${h.tokens[as]}`;
  const body = method === "GET" || options.body === undefined ? undefined : JSON.stringify(options.body);
  if (body !== undefined) headers["content-type"] = "application/json";
  const res = await fetch(`${h.server.url}${path}`, {
    method,
    headers,
    body,
  });
  const text = await res.text();
  return { status: res.status, body: text ? JSON.parse(text) : null };
}

export function newAppPool(): { pool: string; schema: string } {
  const id = randomUUID();
  return { pool: `app-${id}`, schema: `identity_app_${id.replaceAll("-", "")}` };
}

export async function schemaExists(db: pg.Pool, schema: string): Promise<boolean> {
  const r = await db.query("SELECT 1 FROM pg_namespace WHERE nspname = $1", [schema]);
  return r.rowCount === 1;
}

/** Creates a pool as the worker would and fails the test if it doesn't answer 2xx. */
export async function createPool(h: Harness, pool: string): Promise<void> {
  const r = await call(h, "POST", `/internal/v1/pools/${pool}`, { as: "worker" });
  if (r.status !== 201 && r.status !== 200) {
    throw new Error(`create pool ${pool}: ${r.status} ${JSON.stringify(r.body)}`);
  }
}
