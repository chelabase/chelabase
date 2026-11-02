// AC 7 measurement, not CI: RSS per cached pool. Creates N pools, then loads every one into
// the cache (one query each) and compares RSS before and after, with GC forced both times.
//
//   CHELABASE_IDENTITY_TEST_DB_URL=postgres://… node --expose-gc bench/pools.ts [N]
import { randomBytes, randomUUID } from "node:crypto";
import pg from "pg";
import { parsePoolName, type PoolRef } from "../src/pool-name.ts";
import { PoolRegistry } from "../src/registry.ts";
import { ADMIN_FIXTURE, identityLoginUrl } from "../tests/admin-fixture.ts";

const url = process.env.CHELABASE_IDENTITY_TEST_DB_URL;
if (!url) throw new Error("set CHELABASE_IDENTITY_TEST_DB_URL");
const n = Number(process.argv[2] ?? 200);
const gc = (globalThis as { gc?: () => void }).gc;
if (!gc) throw new Error("run with node --expose-gc");

const mib = (b: number) => (b / 1024 / 1024).toFixed(1);
function rss(): number {
  gc!();
  gc!();
  return process.memoryUsage().rss;
}

const db = new pg.Pool({ connectionString: url, max: 1 });
await db.query(ADMIN_FIXTURE);
const registry = new PoolRegistry(identityLoginUrl(url), randomBytes(32).toString("hex"), n);
const pools: PoolRef[] = Array.from({ length: n }, () => parsePoolName(`app-${randomUUID()}`)!);

try {
  for (const p of pools) await registry.createPool(p);
  // Warm the code paths with one pool, then measure the rest from a steady state.
  const touch = (p: PoolRef) =>
    registry.use(p, (auth) => auth.api.getSession({ headers: new Headers({ authorization: "Bearer x" }) }));
  await touch(pools[0]!);
  const before = rss();
  for (const p of pools.slice(1)) await touch(p);
  const after = rss();
  const per = (after - before) / (n - 1);
  const conns = await db.query("SELECT count(*)::int AS n FROM pg_stat_activity WHERE datname = current_database()");
  console.log(`server connections now: ${conns.rows[0].n}`);
  console.log(`pools: ${n}  rss before: ${mib(before)} MiB  after: ${mib(after)} MiB`);
  console.log(`rss per cached pool: ${(per / 1024).toFixed(0)} KiB`);
} finally {
  for (const p of pools) await registry.dropPool(p);
  await registry.close();
  await db.end();
}
