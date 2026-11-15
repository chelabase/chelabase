// Scheduled runs (edge-functions m7): a request the functions service marks with
// `x-chelabase-event: scheduled` runs the bundle's `scheduled(controller, env, ctx)` handler
// with the body's `cron` and `scheduledTime`, never its `fetch`. The same body without the
// header is an ordinary request. A missing or throwing handler is a 500 naming why.
//
//   node --test infra/functions-runtime/tests/
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { after, before, describe, test } from "node:test";
import { COMPAT_DATE, invoke, startBundleStore, startRuntime } from "./harness.mjs";

const DB = "db_scheduled";
const EVENT = { "x-chelabase-event": "scheduled", "content-type": "application/json" };

function js(src) {
  return {
    compatibilityDate: COMPAT_DATE,
    compatibilityFlags: [],
    mainModule: "main.js",
    modules: [{ name: "main.js", type: "js", base64: Buffer.from(src).toString("base64") }],
  };
}

// Reports what its handlers saw to the gateway stub through env.CHELABASE.
const REPORTER = `export default {
  async fetch(request, env) {
    await env.CHELABASE.fetch("http://x/seen?handler=fetch");
    return new Response("fetched");
  },
  async scheduled(controller, env, ctx) {
    const q = new URLSearchParams({
      handler: "scheduled",
      cron: controller.cron,
      time: String(controller.scheduledTime),
      waitUntil: typeof ctx.waitUntil,
    });
    await env.CHELABASE.fetch("http://x/seen?" + q);
  },
};`;

describe("scheduled events", () => {
  let store;
  let gateway;
  let rt;
  const seen = [];
  before(async () => {
    store = await startBundleStore();
    gateway = createServer((req, res) => {
      seen.push(Object.fromEntries(new URL(req.url, "http://x").searchParams));
      res.writeHead(200);
      res.end("ok");
    });
    await new Promise((resolve) => gateway.listen(0, "0.0.0.0", resolve));
    rt = await startRuntime(store, { gateway: gateway.address().port });
  });
  after(async () => {
    await rt?.stop();
    await store?.close();
    await new Promise((resolve) => gateway?.close(resolve));
  });

  const post = (sha, body, headers = EVENT) =>
    invoke(rt, DB, sha, { headers, method: "POST", body: JSON.stringify(body), timeout: 15_000 });

  test("runs the scheduled handler with cron and scheduledTime", async () => {
    seen.length = 0;
    const sha = store.add(DB, js(REPORTER));
    const res = await post(sha, { type: "scheduled", cron: "*/5 * * * *", scheduledTime: 1767225600000 });
    const text = await res.text();
    assert.equal(res.status, 200, text);
    assert.deepEqual(JSON.parse(text), { outcome: "ok" });
    assert.deepEqual(seen, [
      { handler: "scheduled", cron: "*/5 * * * *", time: "1767225600000", waitUntil: "function" },
    ]);
  });

  test("the same body without the event header is a plain request", async () => {
    seen.length = 0;
    const sha = store.add(DB, js(REPORTER));
    const res = await post(sha, { type: "scheduled", cron: "* * * * *", scheduledTime: 1 }, {});
    assert.equal(res.status, 200);
    assert.equal(await res.text(), "fetched");
    assert.deepEqual(seen, [{ handler: "fetch" }]);
  });

  test("a bundle without a scheduled handler: 500 naming why", async () => {
    const sha = store.add(DB, js('export default { fetch() { return new Response("x") } }'));
    const res = await post(sha, { type: "scheduled", cron: "* * * * *", scheduledTime: 1 });
    assert.equal(res.status, 500);
    assert.match(res.headers.get("x-chelabase-error") ?? "", /^scheduled handler: /);
  });

  test("a handler that throws: 500 naming the outcome", async () => {
    const sha = store.add(DB, js('export default { scheduled() { throw new Error("boom") } }'));
    const res = await post(sha, { type: "scheduled", cron: "* * * * *", scheduledTime: 1 });
    assert.equal(res.status, 500);
    assert.match(res.headers.get("x-chelabase-error") ?? "", /^scheduled handler: exception/);
  });

  test("a bad event body: 400", async () => {
    const sha = store.add(DB, js(REPORTER));
    for (const body of [{ type: "scheduled" }, { type: "scheduled", cron: 5, scheduledTime: 1 }, "nope"]) {
      const res = await post(sha, body);
      assert.equal(res.status, 400, JSON.stringify(body));
      assert.match(res.headers.get("x-chelabase-error") ?? "", /scheduled event/);
    }
  });
});
