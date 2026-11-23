import assert from "node:assert/strict";
import { admin, branchPath, type Check } from "../lib.ts";

export const check: Check = {
  name: "execute-chains-calls",
  async run(e) {
    const s = await admin(e);
    try {
      const name = `chain-${Date.now()}`;
      const code = `async () => {
        const made = await chelabase.request({
          method: "POST", path: ${JSON.stringify(`/v1/databases/${e.database}/branches`)}, body: { name: ${JSON.stringify(name)} } });
        if (!made.ok) return { step: "createBranch", made };
        const base = ${JSON.stringify(`/v1/databases/${e.database}/branches/`)} + made.body.id + "/console/query";
        const q = (tree) => chelabase.request({ method: "POST", path: base, body: { tree } });
        let ins;
        for (let i = 0; i < 60; i++) {
          ins = await q({ v: 1, op: "insert", into: ${JSON.stringify(e.table)}, values: [{ id: 500, label: "on the branch" }] });
          if (ins.ok) break;
          await new Promise((r) => setTimeout(r, 1000));
        }
        const rows = await q({ v: 1, op: "select", from: ${JSON.stringify(e.table)}, orderBy: [{ col: "id" }] });
        return { created: made.status, branch: made.body.id, name: made.body.name, inserted: ins.status, selected: rows.status, rows: rows.body };
      }`;
      const v = await s.run(code);
      assert.ok(v.branch, `no branch: ${JSON.stringify(v)}`);
      assert.equal(v.inserted, 200, JSON.stringify(v));
      assert.equal(v.selected, 200, JSON.stringify(v));
      const got: number[] = v.rows.map((r: { id: number }) => r.id);
      assert.ok(got.includes(1) && got.includes(500), `the branch has the seed row and its own: ${JSON.stringify(v.rows)}`);
      // The branch is its own: the main branch has no row 500.
      const main = await s.run(
        `async () => (await chelabase.request({ method: "POST", path: ${JSON.stringify(branchPath(e) + "/console/query")}, body: { tree: { v: 1, op: "select", from: ${JSON.stringify(e.table)}, where: { col: "id", eq: 500 } } } })).body`,
      );
      assert.deepEqual(main, []);
    } finally {
      await s.close();
    }
  },
};
