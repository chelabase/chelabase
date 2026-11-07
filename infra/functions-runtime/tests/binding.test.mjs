// The gateway binding (#183): every loaded worker gets `env.CHELABASE`, which reaches only the
// gateway (here a stub as `gateway-functions`), always on the API host, carrying nothing of
// its own. Direct fetches of the gateway (and every other private address) are still refused.
//
//   node --test infra/functions-runtime/tests/
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { after, before, describe, test } from "node:test";
import { invoke, manifest, startBundleStore, startRuntime } from "./harness.mjs";

const DB = "db_binding";
const API_HOST = "api.stack.chelabase.localhost:8100";

describe("the gateway binding", () => {
  let store;
  let gateway;
  let rt;
  const seen = [];
  before(async () => {
    store = await startBundleStore();
    gateway = createServer((req, res) => {
      seen.push({ method: req.method, url: req.url, headers: req.headers });
      res.writeHead(200, { "content-type": "text/plain" });
      res.end(`gateway saw ${req.headers.host} ${req.url}`);
    });
    await new Promise((resolve) => gateway.listen(0, "0.0.0.0", resolve));
    rt = await startRuntime(store, { gateway: gateway.address().port, apiHost: API_HOST });
  });
  after(async () => {
    await rt?.stop();
    await store?.close();
    await new Promise((resolve) => gateway?.close(resolve));
  });

  const call = async (query, headers = {}) => {
    const b = store.add(DB, manifest("binding"));
    const res = await invoke(rt, DB, b, { path: `/?${new URLSearchParams(query)}`, headers, timeout: 15_000 });
    assert.equal(res.status, 200);
    return res.json();
  };

  test("reaches the gateway on the API host with the function's own headers", async () => {
    seen.length = 0;
    const out = await call({ path: "/data/v1/query?x=1", method: "POST" }, { authorization: "Bearer chk_pub_x" });
    assert.equal(out.status, 200, JSON.stringify(out));
    assert.equal(out.body, `gateway saw ${API_HOST} /data/v1/query?x=1`);
    assert.equal(seen.length, 1);
    assert.equal(seen[0].method, "POST");
    assert.equal(seen[0].headers.host, API_HOST);
    assert.equal(seen[0].headers.authorization, "Bearer chk_pub_x");
    assert.deepEqual(out.bindings, ["CHELABASE"], "the binding is the only non-string env entry");
  });

  test("a Host header or another URL host never changes the host the gateway sees", async () => {
    for (const host of ["studio.stack.chelabase.localhost:8100", "auth.stack.chelabase.localhost:8100", "identity:8080"]) {
      seen.length = 0;
      const out = await call({ path: "/auth/v1/studio-config", host });
      assert.equal(seen.length, 1, host);
      assert.equal(seen[0].headers.host, API_HOST, host);
      assert.equal(out.body, `gateway saw ${API_HOST} /auth/v1/studio-config`);
    }
    seen.length = 0;
    const out = await call({ url: "http://studio.stack.chelabase.localhost:8100/x?y=2" });
    assert.equal(seen[0].headers.host, API_HOST);
    assert.equal(out.body, `gateway saw ${API_HOST} /x?y=2`);
  });

  test("the binding carries no credential and no internal header of its own", async () => {
    seen.length = 0;
    await call({ path: "/data/v1/schema" });
    const h = seen[0].headers;
    assert.equal(h.authorization ?? "", "");
    for (const k of Object.keys(h)) {
      assert.ok(!k.startsWith("x-chelabase-") && !k.startsWith("chelabase-"), k);
    }
  });

  test("a direct fetch of the gateway is still refused", async () => {
    const g = store.add(DB, manifest("egress"));
    for (const url of [`http://gateway-functions:${gateway.address().port}/`, `http://${API_HOST}/`]) {
      const out = await (await invoke(rt, DB, g, { path: `/?url=${encodeURIComponent(url)}`, timeout: 15_000 })).json();
      assert.equal(out.reached, false, `${url}: ${JSON.stringify(out)}`);
    }
  });
});
