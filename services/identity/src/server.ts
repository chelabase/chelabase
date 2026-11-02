import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { CallerAuth } from "./caller-auth.ts";
import type { Config } from "./config.ts";
import { readBody, toHttpError, type Json } from "./http.ts";
import { PoolRegistry } from "./registry.ts";
import { ALLOW, match, resolvePool } from "./routes.ts";

export interface RunningServer {
  url: string;
  close(): Promise<void>;
}

function send(res: ServerResponse, status: number, body?: Json): void {
  if (body === undefined) {
    res.writeHead(status).end();
    return;
  }
  const text = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(text) }).end(text);
}

export async function startServer(config: Config): Promise<RunningServer> {
  const callers = new CallerAuth(config.callerTokens);
  const registry = new PoolRegistry(config.databaseUrl, config.secret, config.poolCacheSize);

  async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const path = (req.url ?? "/").split("?")[0]!;
    if (req.method === "GET" && path === "/health") return send(res, 200, { status: "ok" });
    // Caller auth runs first, so an unauthenticated caller learns nothing about routes or pools.
    const caller = callers.identify(req.headers);
    if (!caller) return send(res, 401, { error: "unauthorized", message: "missing or unknown caller token" });
    const m = match(req.method ?? "", path);
    if (!m) return send(res, 404, { error: "not_found", message: "no such route" });
    if (!ALLOW[caller].has(m.route.id)) {
      return send(res, 403, { error: "forbidden", message: `${caller} may not call ${m.route.id}` });
    }
    try {
      const pool = resolvePool(m.poolSegment);
      const body = await readBody(req);
      const reply = await m.route.handle({ registry, pool, body });
      send(res, reply.status, reply.body);
    } catch (err) {
      const e = toHttpError(err);
      if (e.status >= 500) console.error(`identity: ${m.route.id} failed:`, err);
      send(res, e.status, { error: e.code, message: e.message });
    }
  }

  const server = createServer((req, res) => {
    handle(req, res).catch((err) => {
      console.error("identity: unhandled", err);
      if (!res.headersSent) send(res, 500, { error: "internal", message: "internal error" });
    });
  });
  await new Promise<void>((resolve) => server.listen(config.port, config.host, resolve));
  const { port } = server.address() as AddressInfo;
  return {
    url: `http://${config.host}:${port}`,
    async close() {
      await new Promise<void>((resolve) => server.close(() => resolve()));
      await registry.close();
    },
  };
}

