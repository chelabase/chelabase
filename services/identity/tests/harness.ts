// Shared test harness: a real Postgres from CHELABASE_IDENTITY_TEST_DB_URL (the suites skip
// without one) and an in-process identity server with fresh caller tokens.
import { randomBytes, randomUUID } from "node:crypto";
import { createServer, type Socket } from "node:net";
import type { AddressInfo } from "node:net";
import pg from "pg";
import { describe } from "vitest";
import type { Caller, Config } from "../src/config.ts";
import { startServer, type RunningServer } from "../src/server.ts";
import { ADMIN_FIXTURE, identityLoginUrl } from "./admin-fixture.ts";

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
    bootstrap: randomBytes(32).toString("hex"),
  };
  // The service connects as m5's login role, as on the stack; `db` stays the superuser's.
  const server = await startServer({
    databaseUrl: identityLoginUrl(TEST_DB_URL!),
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

/**
 * Creates a pool as its owner would (the bootstrap makes `members`, the worker app pools) and
 * fails the test if it doesn't answer 2xx.
 */
export async function createPool(h: Harness, pool: string): Promise<void> {
  const as = pool === "members" ? "bootstrap" : "worker";
  const r = await call(h, "POST", `/internal/v1/pools/${pool}`, { as });
  if (r.status !== 201 && r.status !== 200) {
    throw new Error(`create pool ${pool}: ${r.status} ${JSON.stringify(r.body)}`);
  }
}

export interface SmtpSink {
  port: number;
  /** Each delivered message: envelope recipients and the raw DATA. */
  messages: { to: string[]; data: string }[];
  close(): Promise<void>;
}

/**
 * A minimal in-process SMTP server for tests: plain text, no auth, accepts every message.
 * Just enough of RFC 5321 for nodemailer's EHLO / MAIL / RCPT / DATA / QUIT.
 */
export async function startSmtpSink(): Promise<SmtpSink> {
  const messages: SmtpSink["messages"] = [];
  const sockets = new Set<Socket>();
  const server = createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => undefined);
    let buffer = "";
    let inData = false;
    let to: string[] = [];
    const reply = (line: string) => socket.write(`${line}\r\n`);
    reply("220 sink ESMTP");
    socket.on("data", (chunk) => {
      buffer += chunk.toString("utf8");
      for (;;) {
        if (inData) {
          const end = buffer.indexOf("\r\n.\r\n");
          if (end === -1) return;
          messages.push({ to, data: buffer.slice(0, end) });
          buffer = buffer.slice(end + 5);
          inData = false;
          to = [];
          reply("250 queued");
          continue;
        }
        const eol = buffer.indexOf("\r\n");
        if (eol === -1) return;
        const line = buffer.slice(0, eol);
        buffer = buffer.slice(eol + 2);
        const verb = line.slice(0, 4).toUpperCase();
        if (verb === "EHLO") reply("250-sink\r\n250 8BITMIME");
        else if (verb === "RCPT") {
          to.push(/<([^>]*)>/.exec(line)?.[1] ?? "");
          reply("250 ok");
        } else if (verb === "DATA") {
          inData = true;
          reply("354 go ahead");
        } else if (verb === "QUIT") {
          reply("221 bye");
          socket.end();
          return;
        } else reply("250 ok");
      }
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    port: (server.address() as AddressInfo).port,
    messages,
    async close() {
      for (const s of sockets) s.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}
