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

// The failure budget and the replacement window under concurrency. Invariants: concurrent
// guesses never count past FAILURE_BUDGET, a right code racing wrong ones either clears the
// budget or is refused (never both, never a lost count), and of concurrent issues for one
// (user, kind) at most one replaces the code.
describeDb("concurrent codes", () => {
  const GUESSES = 24;
  let h: Harness;
  let p: { pool: string; schema: string };
  beforeAll(async () => {
    h = await startHarness();
    p = newAppPool();
    await createPool(h, p.pool);
  });
  afterAll(async () => {
    await h?.close();
  });

  const base = () => `/internal/v1/pools/${p.pool}`;
  let n = 0;
  async function user(): Promise<string> {
    const r = await call(h, "POST", `${base()}/users`, { body: { email: `c${++n}-${Date.now()}@x.io` } });
    expect(r.status).toBe(201);
    return r.body.user.id;
  }
  const issue = (id: string, kind = "reset") =>
    call(h, "POST", `${base()}/codes`, { body: { user_id: id, kind, delivery: "return" } });
  const guess = (id: string, code: string) =>
    call(h, "POST", `${base()}/codes/verify`, { body: { user_id: id, kind: "reset", code, new_password: "password-conc-1" } });
  const wrong = (code: string, k: number) => String((Number(code) + k) % 1_000_000).padStart(6, "0");
  const row = async (id: string) =>
    (await h.db.query(`SELECT failures, attempts, "consumedAt" AS consumed FROM "${p.schema}".email_code WHERE "userId" = $1`, [id]))
      .rows[0] as { failures: number; attempts: number; consumed: Date | null };
  /** Leaves one failure in the budget, as 9 earlier wrong guesses would. */
  const nineFailures = (id: string) =>
    h.db.query(`UPDATE "${p.schema}".email_code SET failures = 9, "failuresSince" = now() WHERE "userId" = $1`, [id]);
  const count = (replies: { status: number }[], status: number) => replies.filter((r) => r.status === status).length;

  it("concurrent wrong guesses never exceed the budget", async () => {
    for (let round = 0; round < 3; round++) {
      const id = await user();
      const { code } = (await issue(id)).body;
      await nineFailures(id);
      const replies = await Promise.all(Array.from({ length: GUESSES }, (_, i) => guess(id, wrong(code, i + 1))));
      expect(count(replies, 400), "exactly one guess is counted").toBe(1);
      expect(count(replies, 429)).toBe(GUESSES - 1);
      expect(replies.filter((r) => r.status === 429).every((r) => (r as any).body.error === "code_budget_spent")).toBe(true);
      expect(await row(id)).toMatchObject({ failures: 10, attempts: 1, consumed: null });
    }
  });

  it("a right code racing wrong ones either clears the budget or is refused", async () => {
    for (let round = 0; round < 5; round++) {
      const id = await user();
      const { code } = (await issue(id)).body;
      await nineFailures(id);
      const guesses = Array.from({ length: GUESSES }, (_, i) => (i === round ? code : wrong(code, i + 1)));
      const replies = await Promise.all(guesses.map((g) => guess(id, g)));
      const right = replies[round]!;
      const r = await row(id);
      // One guess is counted, whichever came first.
      expect(r.attempts).toBe(1);
      if (right.status === 200) {
        expect(r).toMatchObject({ failures: 0 });
        expect(r.consumed).not.toBeNull();
        expect(count(replies, 429)).toBe(0);
      } else {
        expect(right.status).toBe(429);
        expect(r).toMatchObject({ failures: 10, consumed: null });
        expect(count(replies, 400)).toBe(1);
      }
    }
  });

  it("concurrent issues replace at most once", async () => {
    // No code yet, then a locked one: either way exactly one issue wins.
    const id = await user();
    for (const prepare of [async () => undefined, async () => h.db.query(`UPDATE "${p.schema}".email_code SET attempts = 5 WHERE "userId" = $1`, [id])]) {
      await prepare();
      const replies = await Promise.all(Array.from({ length: GUESSES }, () => issue(id)));
      expect(count(replies, 201)).toBe(1);
      expect(count(replies, 429)).toBe(GUESSES - 1);
      const won = replies.find((r) => r.status === 201)!;
      expect((await guess(id, won.body.code)).status).toBe(200);
    }
  });
});
