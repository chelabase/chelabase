import { afterAll, beforeAll, expect, it } from "vitest";
import { sharedPgPool } from "../src/registry.ts";
import {
  call,
  createPool,
  describeDb,
  newAppPool,
  schemaExists,
  startHarness,
  TEST_DB_URL,
  type Harness,
} from "./harness.ts";

describeDb("pools", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness();
  });
  afterAll(async () => {
    await h?.close();
  });

  async function signUp(pool: string, email: string, password: string) {
    return call(h, "POST", `/internal/v1/pools/${pool}/users`, { body: { email, password, name: "A" } });
  }
  async function signIn(pool: string, email: string, password: string) {
    return call(h, "POST", `/internal/v1/pools/${pool}/sessions`, { body: { email, password } });
  }

  // AC 1: the same email signs up in two pools, each with its own password.
  it("same_email_two_pools", async () => {
    const a = newAppPool();
    const b = newAppPool();
    await createPool(h, a.pool);
    await createPool(h, b.pool);
    const upA = await signUp(a.pool, "a@x.io", "password-for-pool-a");
    const upB = await signUp(b.pool, "a@x.io", "password-for-pool-b");
    expect(upA.status).toBe(201);
    expect(upB.status).toBe(201);
    expect(upA.body.user.email).toBe("a@x.io");
    expect(upA.body.user.id).not.toBe(upB.body.user.id);

    const inA = await signIn(a.pool, "a@x.io", "password-for-pool-a");
    expect(inA.status).toBe(201);
    expect(inA.body.user.id).toBe(upA.body.user.id);
    expect(typeof inA.body.session_id).toBe("string");
    expect(typeof inA.body.session_token).toBe("string");

    const inB = await signIn(b.pool, "a@x.io", "password-for-pool-b");
    expect(inB.status).toBe(201);
    expect(inB.body.user.id).toBe(upB.body.user.id);

    // A session token from pool A resolves in A only.
    const lookA = await call(h, "POST", `/internal/v1/pools/${a.pool}/sessions/lookup`, {
      body: { token: inA.body.session_token },
    });
    expect(lookA.status).toBe(200);
    expect(lookA.body.session.id).toBe(inA.body.session_id);
    const lookB = await call(h, "POST", `/internal/v1/pools/${b.pool}/sessions/lookup`, {
      body: { token: inA.body.session_token },
    });
    expect(lookB.status).toBe(404);
  });

  // AC 1, failure path: each password works only in its own pool.
  it("wrong_pool_password_refused", async () => {
    const a = newAppPool();
    const b = newAppPool();
    await createPool(h, a.pool);
    await createPool(h, b.pool);
    expect((await signUp(a.pool, "b@x.io", "password-for-pool-a")).status).toBe(201);
    expect((await signUp(b.pool, "b@x.io", "password-for-pool-b")).status).toBe(201);

    const crossed = await signIn(a.pool, "b@x.io", "password-for-pool-b");
    expect(crossed.status).toBe(401);
    expect(crossed.body.error).toBe("bad_credentials");
    expect((await signIn(b.pool, "b@x.io", "password-for-pool-a")).status).toBe(401);
  });

  // AC 5: dropping one pool removes its schema and leaves the others working.
  it("drop_pool_isolated", async () => {
    const a = newAppPool();
    const b = newAppPool();
    await createPool(h, a.pool);
    await createPool(h, b.pool);
    expect((await signUp(a.pool, "c@x.io", "password-for-pool-a")).status).toBe(201);
    expect((await signUp(b.pool, "c@x.io", "password-for-pool-b")).status).toBe(201);
    // Warm A's cached instance so the drop has to evict it.
    expect((await signIn(a.pool, "c@x.io", "password-for-pool-a")).status).toBe(201);

    const dropped = await call(h, "DELETE", `/internal/v1/pools/${a.pool}`, { as: "worker" });
    expect(dropped.status).toBe(204);
    expect(await schemaExists(h.db, a.schema)).toBe(false);
    expect(await schemaExists(h.db, b.schema)).toBe(true);

    expect((await signIn(a.pool, "c@x.io", "password-for-pool-a")).status).toBe(404);
    expect((await signIn(b.pool, "c@x.io", "password-for-pool-b")).status).toBe(201);
    // Dropping again is a no-op, not an error.
    expect((await call(h, "DELETE", `/internal/v1/pools/${a.pool}`, { as: "worker" })).status).toBe(204);
  });

  // Every pool's instance shares one pg pool whose search_path is only pg_catalog, so an
  // unqualified table reference can never land in some pool's schema.
  it("shared_pool_refuses_unqualified_tables", async () => {
    const a = newAppPool();
    await createPool(h, a.pool);
    const shared = sharedPgPool(TEST_DB_URL!, 1);
    try {
      expect((await shared.query("SHOW search_path")).rows[0].search_path).toBe("pg_catalog");
      for (const table of ['"user"', "session", "account", "verification"]) {
        await expect(shared.query(`SELECT 1 FROM ${table}`), table).rejects.toMatchObject({ code: "42P01" });
      }
      // The same tables qualified with the pool's schema are there.
      expect((await shared.query(`SELECT count(*)::int AS n FROM ${a.schema}.session`)).rows[0].n).toBe(0);
    } finally {
      await shared.end();
    }
  });

  // Defence in depth: nothing may drop the members pool, the worker (which may drop pools)
  // included.
  it("members_pool_is_never_dropped", async () => {
    await createPool(h, "members");
    for (const as of ["worker", "auth", "orchestrator", "bootstrap"] as const) {
      const r = await call(h, "DELETE", "/internal/v1/pools/members", { as });
      expect(r.status, as).toBe(403);
    }
    expect(await schemaExists(h.db, "identity_members")).toBe(true);
  });

  it("creating a pool twice is idempotent", async () => {
    const a = newAppPool();
    await createPool(h, a.pool);
    expect((await signUp(a.pool, "d@x.io", "password-for-pool-a")).status).toBe(201);
    await createPool(h, a.pool);
    expect((await signIn(a.pool, "d@x.io", "password-for-pool-a")).status).toBe(201);
  });

  // A pool is ready only when every table its migration creates exists. One whose migration
  // stopped partway (here: no email_code) is not found until createPool runs again.
  it("pool_missing_email_code_not_ready", async () => {
    const a = newAppPool();
    await createPool(h, a.pool);
    await h.db.query(`DROP TABLE "${a.schema}".email_code`);
    // A fresh server, so nothing is cached.
    const cold = await startHarness();
    try {
      const r = await call(cold, "POST", `/internal/v1/pools/${a.pool}/users`, { body: { email: "e@x.io" } });
      expect(r.status, JSON.stringify(r.body)).toBe(404);
      expect(r.body.error).toBe("pool_not_found");
      // Creating it again completes the migration.
      await createPool(cold, a.pool);
      const up = await call(cold, "POST", `/internal/v1/pools/${a.pool}/users`, { body: { email: "f@x.io" } });
      expect(up.status, JSON.stringify(up.body)).toBe(201);
      const code = await call(cold, "POST", `/internal/v1/pools/${a.pool}/codes`, {
        body: { user_id: up.body.user.id, kind: "verify", delivery: "return" },
      });
      expect(code.status, JSON.stringify(code.body)).toBe(201);
    } finally {
      await cold.close();
    }
  });

  // Better Auth's schema check still runs, against the pool's own schema: a missing column
  // fails the pool's first request instead of a query halfway through it.
  it("schema_check_reads_the_pools_own_schema", async () => {
    const a = newAppPool();
    await createPool(h, a.pool);
    await h.db.query(`ALTER TABLE "${a.schema}"."user" DROP COLUMN "banReason"`);
    const cold = await startHarness();
    try {
      const r = await call(cold, "POST", `/internal/v1/pools/${a.pool}/users`, { body: { email: "g@x.io" } });
      expect(r.status, JSON.stringify(r.body)).toBe(500);
    } finally {
      await cold.close();
    }
  });
});

// AC 6: a bad pool name is refused with 400 before any SQL runs. The server here points at a
// database that refuses every connection, so any SQL attempt would surface as a 5xx.
describeDb("pool names", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness({ databaseUrl: "postgres://nobody:nothing@127.0.0.1:1/none" });
  });
  afterAll(async () => {
    await h?.close();
  });

  const uuid = "0b9e6f4c-3c1e-4d8a-9f6b-2a7d5e8c1f03";
  const bad: [string, string][] = [
    ["parent path", "..%2Fmembers"],
    ["quote", "app-'%3B%20drop%20schema%20public%3B--"],
    ["double quote", "app-%22x%22"],
    ["uppercase members", "MEMBERS"],
    ["uppercase uuid", `app-${uuid.toUpperCase()}`],
    ["overlong", `app-${uuid}0`],
    ["very long", `app-${"a".repeat(300)}`],
    ["no dashes", `app-${uuid.replaceAll("-", "")}`],
    ["schema name itself", `identity_app_${uuid.replaceAll("-", "")}`],
    ["empty uuid", "app-"],
    ["trailing newline", `app-${uuid}%0A`],
    ["unicode digit", `app-${uuid.slice(0, -1)}%D9%A3`],
  ];

  it("rejects_bad_pool_names", async () => {
    for (const [label, name] of bad) {
      for (const [method, path, as] of [
        ["POST", `/internal/v1/pools/${name}`, "worker"],
        ["DELETE", `/internal/v1/pools/${name}`, "worker"],
        ["POST", `/internal/v1/pools/${name}/users`, "auth"],
        ["POST", `/internal/v1/pools/${name}/sessions`, "auth"],
      ] as const) {
        const r = await call(h, method, path, { as, body: { email: "a@x.io", password: "password-x-1" } });
        expect(r.status, `${label}: ${method} ${path}`).toBe(400);
        expect(r.body.error, label).toBe("invalid_pool");
      }
    }
  });
});
