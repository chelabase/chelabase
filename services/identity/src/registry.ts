// Better Auth instances, one per pool, created lazily and kept in an LRU cache.
//
// Every instance shares one pg pool whose connections pin `search_path=pg_catalog`. Better
// Auth qualifies every query with the pool's schema (`database.schemaName`), and an
// unqualified table reference fails rather than reaching some pool's tables. One shared
// pool also keeps the connection count fixed however many pools are cached.
import { getSchema } from "better-auth/db";
import { getMigrations } from "better-auth/db/migration";
import pg from "pg";
import { authOptions, createAuth, type Auth } from "./better-auth.ts";
import { SCHEMA_PATTERN, type PoolRef } from "./pool-name.ts";

export class PoolNotFound extends Error {
  constructor(pool: string) {
    super(`pool ${pool} does not exist`);
  }
}

/** Connections in the shared pool, for every pool together. */
const SHARED_POOL_MAX = 10;
const IDLE_TIMEOUT_MS = 10_000;

function pgPool(databaseUrl: string, searchPath: string, max: number): pg.Pool {
  // The search path goes into the startup packet, never into SQL text, and is only ever
  // pg_catalog or a validated pool schema.
  if (searchPath !== "pg_catalog" && !SCHEMA_PATTERN.test(searchPath)) {
    throw new Error("refusing an unvalidated schema name");
  }
  const db = new pg.Pool({
    connectionString: databaseUrl,
    max,
    idleTimeoutMillis: IDLE_TIMEOUT_MS,
    options: `-c search_path=${searchPath}`,
  });
  // An idle client's network error must not crash the process; the next query reconnects.
  db.on("error", (err) => console.error(`identity: pg pool error (${searchPath}): ${err.message}`));
  return db;
}

/** The pool every Better Auth instance shares: `search_path` is pg_catalog only. */
export function sharedPgPool(databaseUrl: string, max = SHARED_POOL_MAX): pg.Pool {
  return pgPool(databaseUrl, "pg_catalog", max);
}

export class PoolRegistry {
  private readonly cache = new Map<string, Auth>();
  private readonly pending = new Map<string, Promise<Auth>>();
  /** Per-schema lifecycle lock: create, drop and a cold load for one pool never overlap. */
  private readonly locks = new Map<string, Promise<unknown>>();
  private readonly db: pg.Pool;
  private readonly databaseUrl: string;
  private readonly secret: string;
  private readonly capacity: number;
  /** Every table a pool's migration creates, from the same options the migration uses. */
  private readonly tables: string[];

  constructor(databaseUrl: string, secret: string, capacity: number) {
    this.databaseUrl = databaseUrl;
    this.secret = secret;
    this.capacity = capacity;
    this.db = sharedPgPool(databaseUrl);
    this.tables = Object.entries(getSchema(authOptions(this.db, secret, undefined)))
      .filter(([, table]) => !table.disableMigrations)
      .map(([name]) => name);
  }

  /** Runs `fn` with the pool's instance. */
  async use<T>(ref: PoolRef, fn: (auth: Auth) => Promise<T>): Promise<T> {
    return fn(await this.acquire(ref));
  }

  /** Creates the schema through m5's function and runs Better Auth's migrations. Idempotent. */
  async createPool(ref: PoolRef): Promise<void> {
    await this.locked(ref.schema, async () => {
      await this.db.query("SELECT identity_admin.create_pool_schema($1)", [ref.schema]);
      // Migrate in search_path mode on a connection pinned to the schema: Better Auth targets
      // current_schema() and emits no CREATE SCHEMA, which this role may not run (it has no
      // CREATE on the database). The schema exists by now.
      const pinned = pgPool(this.databaseUrl, ref.schema, 1);
      try {
        const { runMigrations } = await getMigrations(authOptions(pinned, this.secret, undefined));
        await runMigrations();
      } finally {
        await pinned.end();
      }
    });
  }

  /** Evicts the pool's instance and drops its schema through m5's function. Idempotent. */
  async dropPool(ref: PoolRef): Promise<void> {
    await this.locked(ref.schema, async () => {
      this.cache.delete(ref.schema);
      await this.db.query("SELECT identity_admin.drop_pool_schema($1)", [ref.schema]);
    });
  }

  async close(): Promise<void> {
    this.cache.clear();
    await this.db.end();
  }

  private async acquire(ref: PoolRef): Promise<Auth> {
    const hit = this.cache.get(ref.schema);
    if (hit) {
      // Refresh LRU order.
      this.cache.delete(ref.schema);
      this.cache.set(ref.schema, hit);
      return hit;
    }
    let load = this.pending.get(ref.schema);
    if (!load) {
      load = this.locked(ref.schema, () => this.load(ref));
      this.pending.set(ref.schema, load);
      const done = () => this.pending.delete(ref.schema);
      void load.then(done, done);
    }
    return load;
  }

  private async load(ref: PoolRef): Promise<Auth> {
    const cached = this.cache.get(ref.schema);
    if (cached) return cached;
    // A pool exists once its migration ran to the end: every table it creates is there. One
    // that stopped partway (or predates a table) is not found until createPool runs again;
    // loading never migrates.
    const found = await this.db.query<{ n: number }>(
      "SELECT count(*)::int AS n FROM pg_catalog.pg_tables WHERE schemaname = $1 AND tablename = ANY($2::text[])",
      [ref.schema, this.tables],
    );
    if (found.rows[0]?.n !== this.tables.length) throw new PoolNotFound(ref.name);
    const auth = createAuth(this.db, this.secret, ref.schema);
    this.cache.set(ref.schema, auth);
    while (this.cache.size > this.capacity) {
      this.cache.delete(this.cache.keys().next().value!);
    }
    return auth;
  }

  private locked<T>(schema: string, fn: () => Promise<T>): Promise<T> {
    const prev = this.locks.get(schema) ?? Promise.resolve();
    const next = prev.then(fn, fn);
    const tail = next.catch(() => undefined);
    this.locks.set(schema, tail);
    void tail.then(() => {
      if (this.locks.get(schema) === tail) this.locks.delete(schema);
    });
    return next;
  }
}
