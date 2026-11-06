// Many pools created, used and dropped at once, as the Rust client's live suite does on the
// stack. A pool's migration and its first request's schema check introspect the database
// while other pools' schemas are being dropped; neither may trip over another pool's schema.
import { afterAll, beforeAll, expect, it } from "vitest";
import { call, createPool, describeDb, newAppPool, schemaExists, startHarness, type Harness } from "./harness.ts";

const POOLS = 24;
const ROUNDS = 3;
/** Unrelated tables, so each introspection runs long enough for a drop to land inside it. */
const BULK_SCHEMA = "concurrency_bulk";
const BULK_TABLES = 1500;

describeDb("concurrent pools", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness();
    await h.db.query(`DROP SCHEMA IF EXISTS ${BULK_SCHEMA} CASCADE`);
    await h.db.query(`CREATE SCHEMA ${BULK_SCHEMA}`);
    await h.db.query(`DO $$ BEGIN
      FOR i IN 1..${BULK_TABLES} LOOP
        EXECUTE format('CREATE TABLE ${BULK_SCHEMA}.t%s (a int, b text, c int, d text, e int)', i);
      END LOOP;
    END $$`);
    // The service's role must see them (it sees every pool schema it owns the same way).
    await h.db.query(`GRANT USAGE ON SCHEMA ${BULK_SCHEMA} TO PUBLIC`);
  }, 120_000);
  afterAll(async () => {
    await h?.db.query(`DROP SCHEMA IF EXISTS ${BULK_SCHEMA} CASCADE`);
    await h?.close();
  });

  async function lifecycle(): Promise<string[]> {
    const p = newAppPool();
    const base = `/internal/v1/pools/${p.pool}`;
    const failures: string[] = [];
    const check = (step: string, r: { status: number; body: unknown }, ok: number) => {
      if (r.status !== ok) failures.push(`${p.pool} ${step}: ${r.status} ${JSON.stringify(r.body)}`);
      return r.status === ok;
    };
    const created = await call(h, "POST", base, { as: "worker" });
    if (!check("create pool", created, 201)) return failures;
    const email = "u@x.io";
    const password = "password-concurrent-1";
    const user = await call(h, "POST", `${base}/users`, { body: { email, password } });
    if (check("create user", user, 201)) {
      check("sign in", await call(h, "POST", `${base}/sessions`, { body: { email, password } }), 201);
    }
    check("drop pool", await call(h, "DELETE", base, { as: "worker" }), 204);
    if (await schemaExists(h.db, p.schema)) failures.push(`${p.pool}: schema left behind`);
    return failures;
  }

  it("pools_created_used_and_dropped_in_parallel", async () => {
    // Some long-lived pools, so every introspection has other schemas' tables to read.
    const standing = Array.from({ length: 8 }, () => newAppPool());
    await Promise.all(standing.map((p) => createPool(h, p.pool)));
    // Workers run lifecycles back to back, so creates, first requests and drops interleave.
    const worker = async () => {
      const failures: string[] = [];
      for (let i = 0; i < ROUNDS; i++) failures.push(...(await lifecycle()));
      return failures;
    };
    const failures = (await Promise.all(Array.from({ length: POOLS }, worker))).flat();
    expect(failures).toEqual([]);
  }, 120_000);
});
