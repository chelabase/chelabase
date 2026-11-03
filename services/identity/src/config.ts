// Service configuration, read once at start-up. Secrets come from files (`*_FILE`), never
// from plain env values, matching the Rust services' house style.
import { closeSync, constants, fstatSync, openSync, readFileSync } from "node:fs";

/**
 * The internal services allowed to call identity. Each has its own token file. `bootstrap` is
 * chelabase-init's bootstrap, which creates the members pool and nothing else.
 */
export const CALLERS = ["auth", "bootstrap", "orchestrator", "worker"] as const;
export type Caller = (typeof CALLERS)[number];

/**
 * Cached Better Auth instances. `bench/pools.ts` measured 307 KiB RSS per cached pool (200
 * pools, Node 24, default max_connections=100): 256 pools cost about 77 MiB. On Node 26.13
 * (the image's runtime) the same run showed no growth (-36 KiB/pool): RSS was already at the
 * high-water mark left by migrating the 200 pools, so treat 307 KiB as the upper estimate.
 * All instances share one pg pool, so the cache size does not change the connection count.
 */
export const DEFAULT_POOL_CACHE_SIZE = 256;

/**
 * How the SMTP connection is secured: `starttls` (required STARTTLS, the default), `tls`
 * (implicit TLS, the default on port 465) or `none` (plain text; local sinks such as Mailpit).
 */
export const SMTP_TLS_MODES = ["starttls", "tls", "none"] as const;
export type SmtpTls = (typeof SMTP_TLS_MODES)[number];

export interface SmtpConfig {
  host: string;
  port: number;
  tls: SmtpTls;
  /** Sender address, e.g. `Chelabase <noreply@example.com>`. */
  from: string;
  /** Both or neither. */
  user?: string;
  password?: string;
}

export interface Config {
  /** Postgres URL for the `chelabase_identity` role. */
  databaseUrl: string;
  /** Better Auth's secret (session-token signing). At least 32 bytes. */
  secret: string;
  /** Each caller's bearer token. A caller without a token cannot call at all. */
  callerTokens: ReadonlyMap<Caller, string>;
  poolCacheSize: number;
  /** Outgoing mail. Unset: no email delivery, codes can only be returned to the caller. */
  smtp?: SmtpConfig;
  host: string;
  port: number;
}

const MIN_SECRET_BYTES = 32;
const MIN_TOKEN_BYTES = 32;

/** The largest secret file read, as the Rust services' `FileValue::MAX_LEN`. */
const MAX_SECRET_FILE_LEN = 64 * 1024;

/**
 * A secret file, read with the Rust services' `FileValue` rules for a secret: a regular file
 * (never a symlink, a FIFO or a device), writable and readable by its owner only (`chmod 600`),
 * at most 64 KiB, not empty. The checks are on the descriptor that was opened (`O_NOFOLLOW`,
 * `O_NONBLOCK`), so a name swapped after a check is never trusted. Errors name the variable and
 * the path, never the contents.
 */
function readSecretFile(name: string, path: string): string {
  const refuse = (reason: string) => new Error(`refusing ${name}=${path}: ${reason}`);
  let fd: number;
  try {
    fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ELOOP") throw refuse("it is a symlink");
    throw refuse(`opening it: ${(e as NodeJS.ErrnoException).code ?? "error"}`);
  }
  try {
    const stat = fstatSync(fd);
    if (!stat.isFile()) throw refuse("it is not a regular file");
    const mode = stat.mode & 0o7777;
    const octal = mode.toString(8).padStart(4, "0");
    if (mode & 0o022) throw refuse(`it is writable by its group or others (mode ${octal}); chmod 600 it`);
    if (mode & 0o077) {
      throw refuse(`it holds a secret but is readable by its group or others (mode ${octal}); chmod 600 it`);
    }
    if (stat.size > MAX_SECRET_FILE_LEN) throw refuse(`it is larger than ${MAX_SECRET_FILE_LEN} bytes`);
    const value = readFileSync(fd, "utf8").trim();
    if (value === "") throw new Error(`${name} points at an empty file`);
    return value;
  } finally {
    closeSync(fd);
  }
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

function nonBlank(env: NodeJS.ProcessEnv, name: string): string | undefined {
  const v = env[name]?.trim();
  return v ? v : undefined;
}

/** `CHELABASE_SMTP_*`, the same variables chelabase-init reads, plus `CHELABASE_SMTP_TLS`. */
export function smtpFromEnv(env: NodeJS.ProcessEnv): SmtpConfig | undefined {
  const password = secretFromEnv(env, "CHELABASE_SMTP_PASSWORD");
  const host = nonBlank(env, "CHELABASE_SMTP_HOST");
  if (host === undefined) return undefined;
  const rawPort = nonBlank(env, "CHELABASE_SMTP_PORT");
  const port = rawPort === undefined ? 587 : Number(rawPort);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error("CHELABASE_SMTP_PORT is not a port");
  const from = nonBlank(env, "CHELABASE_SMTP_FROM");
  if (from === undefined) throw new Error("CHELABASE_SMTP_HOST is set without CHELABASE_SMTP_FROM (the sender address)");
  const user = nonBlank(env, "CHELABASE_SMTP_USER");
  if ((user === undefined) !== (password === undefined)) {
    throw new Error("set both CHELABASE_SMTP_USER and CHELABASE_SMTP_PASSWORD_FILE, or neither");
  }
  const tls = nonBlank(env, "CHELABASE_SMTP_TLS") ?? (port === 465 ? "tls" : "starttls");
  if (!(SMTP_TLS_MODES as readonly string[]).includes(tls)) {
    throw new Error(`CHELABASE_SMTP_TLS must be one of ${SMTP_TLS_MODES.join(", ")}`);
  }
  return { host, port, tls: tls as SmtpTls, from, ...(user === undefined ? {} : { user, password: password! }) };
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
  const smtp = smtpFromEnv(env);
  return {
    databaseUrl,
    secret,
    callerTokens,
    poolCacheSize,
    ...(smtp === undefined ? {} : { smtp }),
    host: env.CHELABASE_IDENTITY_HOST ?? "0.0.0.0",
    port,
  };
}
