// Service configuration, read once at start-up. Secrets come from files (`*_FILE`), never
// from plain env values, matching the Rust services' house style.
import { readFileSync } from "node:fs";

/** The internal services allowed to call identity. Each has its own token file. */
export const CALLERS = ["auth", "orchestrator", "worker"] as const;
export type Caller = (typeof CALLERS)[number];

/**
 * Cached Better Auth instances. `bench/pools.ts` measured 307 KiB RSS per cached pool (200
 * pools, Node 24, default max_connections=100): 256 pools cost about 77 MiB. All instances
 * share one pg pool, so the cache size does not change the connection count.
 */
export const DEFAULT_POOL_CACHE_SIZE = 256;

export interface Config {
  /** Postgres URL for the `chelabase_identity` role. */
  databaseUrl: string;
  /** Better Auth's secret (session-token signing). At least 32 bytes. */
  secret: string;
  /** Each caller's bearer token. A caller without a token cannot call at all. */
  callerTokens: ReadonlyMap<Caller, string>;
  poolCacheSize: number;
  host: string;
  port: number;
}

const MIN_SECRET_BYTES = 32;
const MIN_TOKEN_BYTES = 32;

function readSecretFile(name: string, path: string): string {
  const value = readFileSync(path, "utf8").trim();
  if (value === "") throw new Error(`${name} points at an empty file`);
  return value;
}

/** The value of `NAME_FILE` (a file path) or `NAME`; secrets accept only the file form. */
function secretFromEnv(env: NodeJS.ProcessEnv, name: string): string | undefined {
  const file = env[`${name}_FILE`];
  if (env[name] !== undefined) {
    throw new Error(`${name} must be passed as a file (${name}_FILE), not a plain value`);
  }
  return file ? readSecretFile(`${name}_FILE`, file) : undefined;
}

function required<T>(value: T | undefined, name: string): T {
  if (value === undefined) throw new Error(`${name} is not set`);
  return value;
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  const databaseUrl = required(
    secretFromEnv(env, "CHELABASE_IDENTITY_DATABASE_URL"),
    "CHELABASE_IDENTITY_DATABASE_URL_FILE",
  );
  const secret = required(secretFromEnv(env, "CHELABASE_IDENTITY_SECRET"), "CHELABASE_IDENTITY_SECRET_FILE");
  if (Buffer.byteLength(secret) < MIN_SECRET_BYTES) {
    throw new Error(`CHELABASE_IDENTITY_SECRET_FILE must hold at least ${MIN_SECRET_BYTES} bytes`);
  }
  const callerTokens = new Map<Caller, string>();
  for (const caller of CALLERS) {
    const name = `CHELABASE_IDENTITY_${caller.toUpperCase()}_TOKEN`;
    const token = secretFromEnv(env, name);
    if (token === undefined) continue;
    if (Buffer.byteLength(token) < MIN_TOKEN_BYTES) {
      throw new Error(`${name}_FILE must hold at least ${MIN_TOKEN_BYTES} bytes`);
    }
    callerTokens.set(caller, token);
  }
  if (callerTokens.size === 0) throw new Error("no caller token files are set; nobody could call identity");
  const poolCacheSize = Number(env.CHELABASE_IDENTITY_POOL_CACHE_SIZE ?? DEFAULT_POOL_CACHE_SIZE);
  if (!Number.isInteger(poolCacheSize) || poolCacheSize < 1) {
    throw new Error("CHELABASE_IDENTITY_POOL_CACHE_SIZE must be a positive integer");
  }
  const port = Number(env.CHELABASE_IDENTITY_PORT ?? 8080);
  if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error("CHELABASE_IDENTITY_PORT is invalid");
  return {
    databaseUrl,
    secret,
    callerTokens,
    poolCacheSize,
    host: env.CHELABASE_IDENTITY_HOST ?? "0.0.0.0",
    port,
  };
}
