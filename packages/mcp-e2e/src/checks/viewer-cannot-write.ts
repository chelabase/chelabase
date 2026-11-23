import assert from "node:assert/strict";
import { admin, branchPath, consoleCode, ids, viewer, type Check } from "../lib.ts";

export const check: Check = {
  name: "viewer-cannot-write",
  async run(e) {
    const v = await viewer(e);
    const a = await admin(e);
    try {
      const out = await v.run(
        consoleCode(branchPath(e), {
          v: 1,
          op: "insert",
          into: e.table,
          values: [{ id: 3, label: "from the viewer" }],
        }),
        true,
      );
      // The API's own answer, passed through: the tool call itself succeeded.
      assert.equal(out.status, 403, JSON.stringify(out));
      assert.equal(out.body.error.code, "forbidden", JSON.stringify(out));
      assert.ok(!(await ids(a, e)).includes(3), "the viewer's row was not written");
    } finally {
      await v.close();
      await a.close();
    }
  },
};
