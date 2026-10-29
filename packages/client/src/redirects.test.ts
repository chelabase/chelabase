// Every request the client sends carries a data key (and maybe an app user's token or a
// refresh token): a redirect must never succeed, and the request is never sent again, on the
// same origin or another. Real HTTP servers, so `fetch`'s own redirect handling runs.
import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, beforeEach, describe, expect, test } from "vitest";
import { createClient, DataApiError, fetchTypes, type AuthStorage } from "./index.ts";

const secret = "chk_sec_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const pub = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

interface Seen {
  url: string;
  auth: string | undefined;
  user: string | undefined;
  body: string;
}

function listen(handle: (req: IncomingMessage, seen: Seen) => { status: number; headers?: Record<string, string>; body?: string }) {
  const seen: Seen[] = [];
  const server = createServer((req, res) => {
    let body = "";
    req.on("data", (c: Buffer) => (body += c.toString()));
    req.on("end", () => {
      const s = {
        url: req.url ?? "",
        auth: req.headers.authorization,
        user: req.headers["chelabase-user"] as string | undefined,
        body,
      };
      seen.push(s);
      const r = handle(req, s);
      res.writeHead(r.status, { "content-type": "application/json", ...r.headers });
      res.end(r.body ?? "");
    });
  });
  return { server, seen };
}

const start = (s: Server) =>
  new Promise<string>((ok) => s.listen(0, "127.0.0.1", () => ok(`http://127.0.0.1:${(s.address() as AddressInfo).port}`)));
const stop = (s: Server) => new Promise<void>((ok) => s.close(() => ok()));

// The other origin answers anything as if it were the data API.
const other = listen(() => ({ status: 200, body: JSON.stringify({ defaultSchema: "evil", schemas: {} }) }));
let otherUrl = "";
let mode: "same" | "cross" = "same";
const api = listen((req) => {
  if (req.url?.startsWith("/elsewhere")) {
    return { status: 200, body: JSON.stringify({ defaultSchema: "evil", schemas: {} }) };
  }
  return mode === "same"
    ? { status: 302, headers: { location: "/elsewhere/collect" } }
    : { status: 307, headers: { location: `${otherUrl}/x` } };
});
let apiUrl = "";

beforeAll(async () => {
  apiUrl = await start(api.server);
  otherUrl = await start(other.server);
});
afterAll(async () => {
  await stop(api.server);
  await stop(other.server);
});
beforeEach(() => {
  api.seen.length = 0;
  other.seen.length = 0;
});

function memory(initial: Record<string, string> = {}): AuthStorage {
  const m = new Map(Object.entries(initial));
  return { get: (k) => m.get(k) ?? null, set: (k, v) => void m.set(k, v), remove: (k) => void m.delete(k) };
}

const requests: [string, () => PromiseLike<unknown>][] = [
  ["fetchTypes", () => fetchTypes({ url: apiUrl, key: secret })],
  ["fetchTypes (all databases)", () => fetchTypes({ url: apiUrl, key: secret, allDatabases: true })],
  ["a query", () => createClient({ url: apiUrl, key: secret }).from("t").select("id")],
  ["a call", () => createClient({ url: apiUrl, key: secret }).call("f")],
  [
    "a batch",
    () =>
      createClient({ url: apiUrl, key: secret }).batch((tx) => [tx.from("t").select("id"), tx.from("u").select("id")]),
  ],
  ["sign-in", () => createClient({ url: apiUrl, key: pub, auth: { storage: memory() } }).auth.signIn({ email: "a@b.co", password: "pw" })],
  [
    "refresh",
    () =>
      createClient({
        url: apiUrl,
        key: pub,
        auth: { storage: memory({ [`chelabase.${pub.slice(0, 12)}.refresh`]: "rt-secret" }), autoRefresh: false },
      }).auth.refresh(),
  ],
];

describe("a redirect never succeeds and never carries the key on", () => {
  for (const m of ["same", "cross"] as const) {
    for (const [name, run] of requests) {
      test(`${name}: ${m === "same" ? "same-origin 302" : "cross-origin 307"}`, async () => {
        mode = m;
        const outcome = await Promise.resolve(run()).then(
          (v) => ({ ok: true as const, v }),
          (e: unknown) => ({ ok: false as const, e }),
        );
        expect(outcome.ok, JSON.stringify(outcome)).toBe(false);
        const e = (outcome as { e: unknown }).e;
        expect(e).toBeInstanceOf(DataApiError);
        expect((e as DataApiError).message).toContain("redirect");
        expect((e as DataApiError).message).not.toContain(secret);
        // One request, to the API, and nothing after it.
        expect(api.seen.map((s) => s.url.split("?")[0])).toHaveLength(1);
        expect(api.seen.some((s) => s.url.startsWith("/elsewhere"))).toBe(false);
        expect(other.seen).toEqual([]);
      });
    }
  }
});
