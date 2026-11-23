import assert from "node:assert/strict";
import { admin, branchPath, consoleCode, ids, insert, type Check } from "../lib.ts";

export const check: Check = {
  name: "delete-needs-the-flag",
  async run(e) {
    const s = await admin(e);
    try {
      await insert(s, e, 2, "to delete");
      assert.ok((await ids(s, e)).includes(2));
      const del = consoleCode(branchPath(e), {
        v: 1,
        op: "delete",
        from: e.table,
        where: { col: "id", eq: 2 },
      });
      // Without the flag: refused before the API, and the row is still there.
      const refused = await s.execute(del, false);
      assert.equal(refused.isError, true, refused.text);
      assert.equal(refused.error.code, "destructive_flag_required", refused.text);
      assert.ok((await ids(s, e)).includes(2), "the row survived the refused delete");
      // With it: deleted.
      const done = await s.run(del, true);
      assert.equal(done.status, 200, JSON.stringify(done));
      assert.ok(!(await ids(s, e)).includes(2), "the row is gone after the flagged delete");
    } finally {
      await s.close();
    }
  },
};
