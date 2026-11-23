import assert from "node:assert/strict";
import { admin, branchPath, events, ids, sqlCode, type Check } from "../lib.ts";

/** SQLSTATE read_only_sql_transaction. */
const READ_ONLY = "25006";

export const check: Check = {
  name: "sql-is-read-only-without-the-flag",
  async run(e) {
    const s = await admin(e);
    try {
      const path = branchPath(e);
      const before = await ids(s, e);
      const run = (sql: string, destructive = false) => s.run(sqlCode(path, sql), destructive);

      // The script is refused before it runs.
      for (const sql of [
        `insert into public.${e.table} values (20, 'no')`,
        "create table public.e2e_new (id int)",
      ]) {
        const v = await run(sql);
        assert.equal(v.status, 422, `${sql}: ${JSON.stringify(v)}`);
        assert.equal(v.body.error.code, "read_only_script_refused", sql);
      }
      // Postgres refuses what only a function call writes.
      for (const sql of [
        "select pg_catalog.nextval('public.e2e_seq')",
        "select public.e2e_write()",
      ]) {
        const v = await run(sql);
        assert.equal(v.status, 200, `${sql}: ${JSON.stringify(v)}`);
        const error = events(v.body).find((ev) => ev.t === "error");
        assert.equal(error?.code, READ_ONLY, `${sql}: ${JSON.stringify(v.body)}`);
      }

      // A read-only script may hold several statements.
      const three = await run("select 1 as a; select 2 as b; select 3 as c");
      assert.equal(three.status, 200, JSON.stringify(three));
      const evs = events(three.body);
      assert.equal(evs.find((ev) => ev.t === "error"), undefined, JSON.stringify(evs));
      assert.equal(evs.filter((ev) => ev.t === "rows").length, 3, JSON.stringify(evs));

      // A script holding COMMIT is refused whole: its first statement didn't run either.
      const refused = await run("select 1; commit; select 2");
      assert.equal(refused.status, 422, JSON.stringify(refused));
      assert.equal(refused.body.error.code, "read_only_script_refused", JSON.stringify(refused));

      assert.deepEqual(await ids(s, e), before, "the refused scripts changed the data");

      // With the flag the insert goes through.
      const done = await run(`insert into public.${e.table} values (22, 'flagged')`, true);
      assert.equal(done.status, 200, JSON.stringify(done));
      assert.equal(events(done.body).find((ev) => ev.t === "error"), undefined, JSON.stringify(done));
      assert.ok((await ids(s, e)).includes(22), "the flagged insert is there");
    } finally {
      await s.close();
    }
  },
};
