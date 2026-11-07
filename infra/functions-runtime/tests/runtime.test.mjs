// The functions runtime against the built image (`docker buildx bake functions-runtime`):
// hot swap, WASM, the manifest hash, header stripping, env isolation and egress.
//
//   node --test infra/functions-runtime/tests/
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { after, before, describe, test } from "node:test";
import { INTERNAL_NAMES, invoke, manifest, sha256, startBundleStore, startRuntime } from "./harness.mjs";

const DB = "db_0123456789abcdef";

describe("functions runtime", () => {
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

  test("hot swap: a new hash loads new code with no restart", async () => {
    const a = store.add(DB, manifest("hello-a"));
    const b = store.add(DB, manifest("hello-b"));
    const before = await rt.inspect();
    assert.equal(await (await invoke(rt, DB, a)).text(), "A");
    assert.equal(await (await invoke(rt, DB, b)).text(), "B");
    // A bundle that didn't exist when the runtime started.
    const c = store.add(DB, manifest("hello-a", { "late.txt": "added after start" }));
    assert.notEqual(c, a);
    assert.equal(await (await invoke(rt, DB, c)).text(), "A");
    assert.equal(await (await invoke(rt, DB, a)).text(), "A");
    const now = await rt.inspect();
    assert.equal(now.State.StartedAt, before.State.StartedAt, "the process was not restarted");
    assert.equal(now.State.Pid, before.State.Pid);
    assert.equal(now.RestartCount, before.RestartCount);
  });

  test("a loaded bundle is cached by hash: the store is asked once", async () => {
    const a = store.add(DB, manifest("hello-b", { "cache.txt": "cache probe" }));
    for (let i = 0; i < 3; i++) assert.equal(await (await invoke(rt, DB, a)).text(), "B");
    assert.equal(store.hits.get(`/internal/bundles/${DB}/${a}`), 1);
  });

  test("a core wasm module in the bundle instantiates and runs", async () => {
    const w = store.add(DB, manifest("wasm"));
    const res = await invoke(rt, DB, w);
    assert.equal(res.status, 200);
    assert.equal(await res.text(), "2+3=5");
  });

  test("WebAssembly.instantiate(bytes) is refused", async () => {
    const w = store.add(DB, manifest("wasm-bytes"));
    const res = await invoke(rt, DB, w);
    const body = await res.text();
    assert.equal(res.status, 500, body);
    assert.match(body, /^refused: .*disallowed by embedder/);
  });

  test("a manifest that doesn't hash to the requested sha256 gets 502 and runs nothing", async () => {
    const real = Buffer.from(JSON.stringify(manifest("hello-a")));
    const claimed = sha256(Buffer.from("something else"));
    store.addRaw(DB, claimed, real);
    const res = await invoke(rt, DB, claimed);
    assert.equal(res.status, 502);
    assert.doesNotMatch(await res.text(), /^A$/);
    // Asked again, it is fetched and refused again: a refusal is never cached as loaded code.
    assert.equal((await invoke(rt, DB, claimed)).status, 502);
    assert.equal(store.hits.get(`/internal/bundles/${DB}/${claimed}`), 2);
  });

  test("a missing bundle gets 502; a malformed bundle header gets 400", async () => {
    assert.equal((await invoke(rt, DB, sha256(Buffer.from("absent")))).status, 502);
    for (const bad of ["", "nohash", `${DB}/abc`, `../x/${"a".repeat(64)}`, `${DB}/${"A".repeat(64)}`]) {
      const res = await fetch(rt.url, { headers: bad ? { "x-chelabase-bundle": bad } : {} });
      assert.equal(res.status, 400, `header ${JSON.stringify(bad)}`);
    }
    const a = store.add(DB, manifest("hello-a"));
    const res = await fetch(rt.url, { headers: { "x-chelabase-bundle": `${DB}/${a}`, "x-chelabase-env": "%%%" } });
    assert.equal(res.status, 400, "an env header that isn't base64 JSON");
  });

  test("user code sees no x-chelabase-* header", async () => {
    const e = store.add(DB, manifest("echo"));
    const res = await invoke(rt, DB, e, {
      env: { A: "1" },
      headers: { "X-Chelabase-Evil": "1", "x-chelabase-internal-token": "t", "x-other": "kept" },
    });
    const { headers } = await res.json();
    const leaked = Object.keys(headers).filter((h) => h.toLowerCase().startsWith("x-chelabase-"));
    assert.deepEqual(leaked, []);
    assert.equal(headers["x-other"], "kept");
  });

  test("the same bundle with two env sets: each request sees its own env, and only it", async () => {
    const e = store.add(DB, manifest("echo"));
    const one = { NAME: "one", SHARED: "x" };
    const two = { NAME: "two" };
    for (let i = 0; i < 2; i++) {
      const r1 = await (await invoke(rt, DB, e, { env: one })).json();
      const r2 = await (await invoke(rt, DB, e, { env: two })).json();
      const r0 = await (await invoke(rt, DB, e)).json();
      assert.deepEqual(r1.env, one);
      assert.deepEqual(r2.env, two);
      assert.deepEqual(r0.envKeys, [], "no env header: an empty env, never the supervisor's bindings");
    }
  });

  test("egress: every private target is refused", async () => {
    const g = store.add(DB, manifest("egress"));
    const targets = [
      ...INTERNAL_NAMES.map((n) => `http://${n}:${store.port}/probe-${n}`),
      `http://metadata-db:5432/`,
      `http://nats:4222/`,
      `http://orchestrator:8081/`,
      `http://169.254.169.254/latest/meta-data/`,
      `http://127.0.0.1:8080/`,
      `http://localhost:8080/`,
      `http://[::1]:8080/`,
      `http://0.0.0.0:8080/`,
      `http://[::ffff:127.0.0.1]:8080/`,
      `http://10.0.0.1/`,
      `http://172.17.0.1:${store.port}/probe-gateway-ip`,
      `http://192.168.0.1/`,
      `http://100.64.0.1/`,
    ];
    for (const url of targets) {
      const out = await (await invoke(rt, DB, g, { path: `/?url=${encodeURIComponent(url)}`, timeout: 15_000 })).json();
      assert.equal(out.reached, false, `${url} was reached: ${JSON.stringify(out)}`);
    }
    assert.deepEqual(store.other, [], "no egress probe reached the stub store");
  });

  test("egress: a public address is allowed", async (t) => {
    const g = store.add(DB, manifest("egress"));
    const out = await (await invoke(rt, DB, g, { path: `/?url=${encodeURIComponent("http://1.1.1.1/")}`, timeout: 15_000 })).json();
    if (out.reached) return;
    // No internet here: assert the egress service's config instead.
    t.diagnostic(`1.1.1.1 not reached (${out.error}); checking the network config`);
    const config = readFileSync(new URL("../config.capnp", import.meta.url), "utf8");
    assert.match(config, /name = "egress",\s*network = \(\s*allow = \["public"\]/);
  });
});
