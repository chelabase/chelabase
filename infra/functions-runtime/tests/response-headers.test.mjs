// A function's own response never carries an `x-chelabase-*` header out of the runtime (m4
// audit N1): the functions service reads `x-chelabase-error` on a 5xx as the function's
// `last_error`, so only the supervisor's own refusals may set it.
//
//   node --test infra/functions-runtime/tests/
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { COMPAT_DATE, invoke, startBundleStore, startRuntime } from "./harness.mjs";

const DB = "db_respheaders";

function bundle(src) {
  return {
    compatibilityDate: COMPAT_DATE,
    compatibilityFlags: [],
    mainModule: "main.js",
    modules: [{ name: "main.js", type: "js", base64: Buffer.from(src).toString("base64") }],
  };
}

// Answers `status` with headers of ours (any case) beside one of its own.
const answering = (status) => `export default {
  fetch() {
    const headers = new Headers();
    headers.append("x-chelabase-error", "chosen by the function");
    headers.append("X-Chelabase-Bundle", "forged/bundle");
    headers.append("x-chelabase-anything", "1");
    headers.append("x-kept", "yes");
    headers.append("set-cookie", "a=1");
    headers.append("set-cookie", "b=2");
    return new Response("body ${status}", { status: ${status}, statusText: "Custom", headers });
  },
};`;

describe("a function's response headers", () => {
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

  for (const status of [200, 503]) {
    test(`a ${status} keeps its own headers and loses every x-chelabase-*`, async () => {
      const sha = store.add(DB, bundle(answering(status)));
      const res = await invoke(rt, DB, sha);
      assert.equal(res.status, status);
      assert.equal(res.statusText, "Custom");
      assert.equal(await res.text(), `body ${status}`);
      assert.equal(res.headers.get("x-kept"), "yes");
      assert.deepEqual(res.headers.getSetCookie(), ["a=1", "b=2"]);
      for (const name of res.headers.keys()) {
        assert.ok(!name.startsWith("x-chelabase-"), `${name} reached the caller`);
      }
    });
  }

  test("the supervisor's own refusal still names its reason", async () => {
    const res = await invoke(rt, DB, "b".repeat(64));
    assert.equal(res.status, 502);
    assert.match(res.headers.get("x-chelabase-error") ?? "", /^bundle db_respheaders\/b+: the bundle store answered 404$/);
  });
});
