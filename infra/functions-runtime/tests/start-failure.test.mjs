// A bundle that can't start (m1 audit N2): the supervisor answers 502 `bundle failed to start`
// with `x-chelabase-error` saying so (the functions service records it as the function's
// `last_error`), distinct from a handler that throws (500) and from a manifest it refuses
// before loading (502 with its own reason).
//
//   node --test infra/functions-runtime/tests/
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { COMPAT_DATE, invoke, startBundleStore, startRuntime } from "./harness.mjs";

const DB = "db_startfailure";

// A manifest of js modules ({name: source}), main.js the main module.
function js(modules, { date = COMPAT_DATE, flags = [] } = {}) {
  return {
    compatibilityDate: date,
    compatibilityFlags: flags,
    mainModule: "main.js",
    modules: Object.entries(modules).map(([name, src]) => ({
      name,
      type: "js",
      base64: Buffer.from(src).toString("base64"),
    })),
  };
}

const OK = 'export default { fetch() { return new Response("ok") } }';

describe("bundle start failures", () => {
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

  const cases = {
    "a syntax error": js({ "main.js": "export default { fetch( {" }),
    "an unknown compatibility flag": js({ "main.js": OK }, { flags: ["no_such_flag_m4"] }),
    "a compatibility date in the future": js({ "main.js": OK }, { date: "2999-01-01" }),
    "an import of a module it doesn't have": js({ "main.js": `import "./missing.js";\n${OK}` }),
  };
  for (const [what, m] of Object.entries(cases)) {
    test(`${what}: 502 bundle failed to start`, async () => {
      const sha = store.add(DB, m);
      for (let i = 0; i < 2; i++) {
        const res = await invoke(rt, DB, sha);
        assert.equal(res.status, 502, what);
        assert.equal(await res.text(), "bundle failed to start\n", what);
        assert.equal(res.headers.get("x-chelabase-error"), "bundle failed to start", what);
      }
      // The runtime still serves other bundles.
      const ok = store.add(DB, js({ "main.js": `${OK}\n// ${what}` }));
      assert.equal(await (await invoke(rt, DB, ok)).text(), "ok");
    });
  }

  test("a handler that throws is a 500, not a start failure", async () => {
    const sha = store.add(DB, js({ "main.js": 'export default { fetch() { throw new Error("boom") } }' }));
    const res = await invoke(rt, DB, sha);
    assert.equal(res.status, 500);
    assert.notEqual(await res.text(), "bundle failed to start\n");
    assert.equal(res.headers.get("x-chelabase-error"), null);
  });

  test("a refused manifest names its reason", async () => {
    const sha = store.addRaw(DB, "a".repeat(64), Buffer.from("{}"));
    const res = await invoke(rt, DB, sha);
    assert.equal(res.status, 502);
    const body = await res.text();
    assert.match(body, /^bundle manifest: does not match its sha256/);
    assert.equal(res.headers.get("x-chelabase-error"), body.trim());
  });
});
