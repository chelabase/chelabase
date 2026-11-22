import assert from "node:assert/strict";
import { admin, branchPath, consoleCode, ids, insert, sqlCode, type Check } from "../lib.ts";

export const check: Check = {
  name: "read-only-mode-refuses-writes",
  async run(e) {
    const full = await admin(e);
    const ro = await admin(e, { readOnly: true });
    try {
      await insert(full, e, 10, "kept");
      const before = await ids(full, e);
      const path = branchPath(e);

      // An insert through the console: refused before the API.
      const ins = await ro.execute(
        consoleCode(path, { v: 1, op: "insert", into: e.table, values: [{ id: 11, label: "no" }] }),
      );
      assert.equal(ins.error?.code, "read_only_connection", ins.text);

      // A delete, flag or not.
      const del = await ro.execute(
        consoleCode(path, { v: 1, op: "delete", from: e.table, where: { col: "id", eq: 10 } }),
        true,
      );
      assert.equal(del.error?.code, "read_only_connection", del.text);

      // A writing SQL statement, flag or not: the script is refused.
      for (const destructive of [false, true]) {
        const sql = await ro.run(
          sqlCode(path, `insert into public.${e.table} values (12, 'no')`),
          destructive,
        );
        assert.equal(sql.status, 422, JSON.stringify(sql));
        assert.equal(sql.body.error.code, "read_only_script_refused", JSON.stringify(sql));
      }

      // A select works, through the console and through SQL.
      const sel = await ro.run(
        consoleCode(path, { v: 1, op: "select", from: e.table, where: { col: "id", eq: 10 } }),
      );
      assert.equal(sel.status, 200, JSON.stringify(sel));
      assert.equal(sel.body.length, 1);
      const sql = await ro.run(sqlCode(path, `select count(*) from public.${e.table}`));
      assert.equal(sql.status, 200, JSON.stringify(sql));

      assert.deepEqual(await ids(full, e), before, "the data changed");
    } finally {
      await ro.close();
      await full.close();
    }
  },
};
