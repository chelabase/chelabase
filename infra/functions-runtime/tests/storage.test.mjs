// Storage events (edge-functions m14): a request the functions service marks with
// `x-chelabase-event: storage` and the body {"type": "storage", "event": {...}} reaches the
// bundle's `fetch` as a POST whose body is the event alone, marked `chelabase-event: storage`
// (and no x-chelabase-* header). A malformed body is a 400 naming why; its status passes through.
//
//   node --test infra/functions-runtime/tests/
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { COMPAT_DATE, invoke, startBundleStore, startRuntime } from "./harness.mjs";

const DB = "db_storage";
const EVENT = { "x-chelabase-event": "storage", "content-type": "application/json" };

function js(src) {
  return {
    compatibilityDate: COMPAT_DATE,
    compatibilityFlags: [],
    mainModule: "main.js",
    modules: [{ name: "main.js", type: "js", base64: Buffer.from(src).toString("base64") }],
  };
}

// Answers what its fetch saw: method, the event header, any private header, and the body.
const ECHO = `export default {
  async fetch(request) {
    const priv = [...request.headers.keys()].filter((k) => k.startsWith("x-chelabase-"));
    return Response.json({
      method: request.method,
      event: request.headers.get("chelabase-event"),
      type: request.headers.get("content-type"),
      priv,
      body: await request.text(),
    }, { status: request.headers.get("chelabase-event") === "storage" ? 200 : 418 });
  },
  async scheduled() { throw new Error("never"); },
};`;

const OBJECT_EVENT = {
  id: "0b6f1c8e-1d3a-4c1e-9d2e-0e6a1c2b3d4f",
  type: "object.created",
  time: "2026-11-08T10:00:00Z",
  database_id: "7c9e6679-7425-40de-944b-e07fc1f90ae7",
  branch_id: "7c9e6679-7425-40de-944b-e07fc1f90ae7",
  pg_database: "app",
  bucket: "avatars",
  name: "in/a.png",
  version_id: "9b2d2c1e-0000-4000-8000-000000000001",
  size: 3,
  content_type: "image/png",
  actor: { role: "authenticated", sub: null },
};

describe("storage events", () => {
  let store;
  let rt;
  before(async () => {
    store = await startBundleStore();
    rt = await startRuntime(store);
  });
  after(async () => {
    await rt?.stop();
    await store?.close();
  });

  const post = (sha, body, headers = EVENT) =>
    invoke(rt, DB, sha, {
      headers,
      method: "POST",
      body: typeof body === "string" ? body : JSON.stringify(body),
      timeout: 15_000,
    });

  test("hands the event alone to fetch, marked chelabase-event: storage", async () => {
    const sha = store.add(DB, js(ECHO));
    const res = await post(sha, { type: "storage", event: OBJECT_EVENT });
    const text = await res.text();
    assert.equal(res.status, 200, text);
    const seen = JSON.parse(text);
    assert.equal(seen.method, "POST");
    assert.equal(seen.event, "storage");
    assert.equal(seen.type, "application/json");
    assert.deepEqual(seen.priv, []);
    assert.deepEqual(JSON.parse(seen.body), OBJECT_EVENT);
  });

  test("the function's own status passes through", async () => {
    const sha = store.add(DB, js(`export default { fetch() { return new Response("no", { status: 500 }) } };`));
    const res = await post(sha, { type: "storage", event: OBJECT_EVENT });
    assert.equal(res.status, 500);
    assert.equal(await res.text(), "no");
  });

  test("a malformed storage body is a 400 naming why", async () => {
    const sha = store.add(DB, js(ECHO));
    for (const body of ["not json", { type: "storage" }, { type: "storage", event: "x" }, { type: "scheduled", event: OBJECT_EVENT }]) {
      const res = await post(sha, body);
      assert.equal(res.status, 400, JSON.stringify(body));
      assert.match(res.headers.get("x-chelabase-error") ?? "", /^storage event: /);
      await res.body?.cancel();
    }
  });

  test("the same body without the event header is a plain request", async () => {
    const sha = store.add(DB, js(ECHO));
    const res = await post(sha, { type: "storage", event: OBJECT_EVENT }, { "content-type": "application/json" });
    assert.equal(res.status, 418);
    const seen = JSON.parse(await res.text());
    assert.equal(seen.event, null);
  });
});
