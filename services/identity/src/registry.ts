// Better Auth instances, one per pool, created lazily and kept in an LRU cache. Each instance
// owns its own pg pool, whose connections pin `search_path` to that pool's schema, so no
// connection ever serves two pools. Queries are schema-qualified on top of that.
import { getMigrations } from "better-auth/db/migration";
import pg from "pg";
import { authOptions, createAuth, type Auth } from "./better-auth.ts";
import { SCHEMA_PATTERN, type PoolRef } from "./pool-name.ts";

export class PoolNotFound extends Error {
  constructor(pool: string) {
    super(`pool ${pool} does not exist`);
  }
}

interface Entry {
  auth: Auth;
  db: pg.Pool;
  refs: number;
  evicted: boolean;
}

/** Connections per cached pool. Idle ones close, so a cold pool holds none. */
const MAX_CONNECTIONS_PER_POOL = 4;
const IDLE_TIMEOUT_MS = 10_000;

export class PoolRegistry {
  private readonly cache = new Map<string, Entry>();
  private readonly pending = new Map<string, Promise<Entry>>();
  /** Per-schema lifecycle lock: create and drop for one pool never overlap, nor a cold load. */
  private readonly locks = new Map<string, Promise<unknown>>();
  private readonly admin: pg.Pool;
  private readonly databaseUrl: string;
  private readonly secret: string;
  private readonly capacity: number;

  constructor(databaseUrl: string, secret: string, capacity: number) {
    this.databaseUrl = databaseUrl;
    this.secret = secret;
    this.capacity = capacity;
    this.admin = this.pgPool("pg_catalog", 2);
  }

  /** Runs `fn` with the pool's instance; the instance stays open until `fn` settles. */
  async use<T>(ref: PoolRef, fn: (auth: Auth) => Promise<T>): Promise<T> {
    const entry = await this.acquire(ref);
    try {
      return await fn(entry.auth);
    } finally {
      entry.refs -= 1;
      if (entry.evicted && entry.refs === 0) void entry.db.end();
    }
  }

  /** Creates the schema through m5's function and runs Better Auth's migrations. Idempotent. */
  async createPool(ref: PoolRef): Promise<void> {
    await this.locked(ref.schema, async () => {
      await this.admin.query("SELECT identity_admin.create_pool_schema($1)", [ref.schema]);
      // Migrate in search_path mode: Better Auth targets current_schema() and emits no
      // CREATE SCHEMA, which this role may not run (it has no CREATE on the database).
      const db = this.pgPool(ref.schema, 1);
      try {
        const { runMigrations } = await getMigrations(authOptions(db, this.secret, undefined));
        await runMigrations();
      } finally {
        await db.end();
      }
    });
  }

  /** Evicts the pool's instance and drops its schema through m5's function. Idempotent. */
  async dropPool(ref: PoolRef): Promise<void> {
    await this.locked(ref.schema, async () => {
      this.evict(ref.schema);
      await this.admin.query("SELECT identity_admin.drop_pool_schema($1)", [ref.schema]);
    });
  }

  async close(): Promise<void> {
    for (const schema of [...this.cache.keys()]) this.evict(schema);
    await this.admin.end();
  }

  private async acquire(ref: PoolRef): Promise<Entry> {
    for (;;) {
      const hit = this.cache.get(ref.schema);
      if (hit) {
        // Refresh LRU order.
        this.cache.delete(ref.schema);
        this.cache.set(ref.schema, hit);
        hit.refs += 1;
        return hit;
      }
      let load = this.pending.get(ref.schema);
      if (!load) {
        load = this.locked(ref.schema, () => this.load(ref));
        this.pending.set(ref.schema, load);
        void load.then(
          () => this.pending.delete(ref.schema),
          () => this.pending.delete(ref.schema),
        );
      }
      const loaded = await load;
      if (!loaded.evicted) {
        loaded.refs += 1;
        return loaded;
      }
      // A drop or the LRU evicted it before we got it: load again (a drop then says not found).
    }
  }

  private async load(ref: PoolRef): Promise<Entry> {
    const cached = this.cache.get(ref.schema);
    if (cached) return cached;
    const db = this.pgPool(ref.schema, MAX_CONNECTIONS_PER_POOL);
    try {
      // A pool exists once its migrations ran: `session` is the last core table created.
      const found = await db.query(
        "SELECT 1 FROM pg_catalog.pg_tables WHERE schemaname = $1 AND tablename = 'session'",
        [ref.schema],
      );
      if (found.rowCount !== 1) throw new PoolNotFound(ref.name);
    } catch (err) {
      await db.end();
      throw err;
    }
    const entry: Entry = { auth: createAuth(db, this.secret, ref.schema), db, refs: 0, evicted: false };
    this.cache.set(ref.schema, entry);
    while (this.cache.size > this.capacity) {
      const oldest = this.cache.keys().next().value!;
      this.evict(oldest);
    }
    return entry;
  }

  private evict(schema: string): void {
    const entry = this.cache.get(schema);
    if (!entry) return;
    this.cache.delete(schema);
    entry.evicted = true;
    if (entry.refs === 0) void entry.db.end();
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

  private pgPool(searchPath: string, max: number): pg.Pool {
    // The schema name goes into the startup packet, never into SQL text; it is still only
    // ever a validated name.
    if (searchPath !== "pg_catalog" && !SCHEMA_PATTERN.test(searchPath)) {
      throw new Error("refusing an unvalidated schema name");
    }
    const db = new pg.Pool({
      connectionString: this.databaseUrl,
      max,
      idleTimeoutMillis: IDLE_TIMEOUT_MS,
      options: `-c search_path=${searchPath}`,
    });
    // An idle client's network error must not crash the process; the next query reconnects.
    db.on("error", (err) => console.error(`identity: pg pool error (${searchPath}): ${err.message}`));
    return db;
  }
}
