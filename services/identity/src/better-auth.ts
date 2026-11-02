// The one place Better Auth is configured. Every pool gets exactly these options; only the
// schema and the pg pool differ.
import { betterAuth, type BetterAuthOptions } from "better-auth";
import { admin, bearer } from "better-auth/plugins";
import { PostgresDialect } from "kysely";
import type pg from "pg";
import { CodeStore, codesSchemaPlugin } from "./email/codes.ts";

/**
 * Better Auth wants a base URL for callbacks and redirects. Its HTTP handler is never
 * mounted here (callers only reach our own routes), so this is a fixed, unroutable name.
 */
const BASE_URL = "http://identity.invalid";

/**
 * The fixed plugin list: core email/password (in the options), admin, bearer, and the email
 * codes table (schema only; see email/codes.ts for why not `email-otp`).
 */
function plugins() {
  return [admin(), bearer(), codesSchemaPlugin()];
}

/**
 * Options for the pool whose tables live in `schema`.
 *
 * `schemaName` set: every query is schema-qualified (runtime). Left out: Better Auth targets
 * the connection's `current_schema()` and never emits `CREATE SCHEMA` (migrations, which run
 * as a role without database CREATE; the schema already exists by then).
 */
export function authOptions(pool: pg.Pool, secret: string, schema: string | undefined) {
  return {
    appName: "chelabase-identity",
    baseURL: BASE_URL,
    secret,
    database: {
      dialect: new PostgresDialect({ pool }),
      type: "postgres",
      ...(schema === undefined ? {} : { schemaName: schema }),
    },
    emailAndPassword: {
      enabled: true,
      // Sign-up happens only through our create-user route (admin createUser).
      disableSignUp: true,
      requireEmailVerification: false,
    },
    plugins: plugins(),
    // Our Rust services rate-limit; Better Auth's limiter would key on a proxy address anyway.
    rateLimit: { enabled: false },
    session: { cookieCache: { enabled: false } },
    // No browser ever reaches this service: there are no cookies to protect.
    advanced: { disableCSRFCheck: true },
    telemetry: { enabled: false },
  } satisfies BetterAuthOptions;
}

/** The pool's Better Auth instance, plus its email-code store on the same pg pool. */
export function createAuth(pool: pg.Pool, secret: string, schema: string) {
  return Object.assign(betterAuth(authOptions(pool, secret, schema)), { codes: new CodeStore(pool, schema, secret) });
}

export type Auth = ReturnType<typeof createAuth>;
