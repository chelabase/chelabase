import assert from "node:assert/strict";
import { admin, type Check } from "../lib.ts";

export const check: Check = {
  name: "search-finds-an-operation",
  async run(e) {
    const s = await admin(e);
    try {
      const v = await s.find(
        `async () => Object.entries(spec.paths).flatMap(([path, ops]) =>
           Object.entries(ops)
             .filter(([, op]) => /branch/i.test(op.summary ?? "") && op.operationId === "createBranch")
             .map(([method, op]) => ({
               method, path, class: op["x-class"],
               schema: op.requestBody?.content["application/json"].schema })))`,
      );
      assert.equal(v.length, 1, JSON.stringify(v));
      assert.equal(v[0].method, "post");
      assert.equal(v[0].path, "/v1/databases/{id}/branches");
      assert.ok(v[0].schema.properties.name, `the request schema has no name: ${JSON.stringify(v[0].schema)}`);
      assert.ok(v[0].schema.required.includes("name"));
      // search has no `chelabase` host: it can't call the API.
      const out = await s.search("async () => typeof chelabase");
      assert.equal(out.value, "undefined");
    } finally {
      await s.close();
    }
  },
};
