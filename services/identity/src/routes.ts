// Our own routes. Handlers call Better Auth server-side (`auth.api.*`) and map the results to
// our shapes; Better Auth's HTTP handler is never mounted.
import type { Caller } from "./config.ts";
import { parsePoolName, type PoolRef } from "./pool-name.ts";
import type { PoolRegistry } from "./registry.ts";
import { HttpError, type Json } from "./http.ts";

export type RouteId = "pools.create" | "pools.delete" | "users.create" | "sessions.create" | "sessions.lookup";

/** What each caller may do. Anything not listed answers 403. */
export const ALLOW: Record<Caller, ReadonlySet<RouteId>> = {
  auth: new Set<RouteId>(["users.create", "sessions.create", "sessions.lookup"]),
  orchestrator: new Set<RouteId>(["users.create"]),
  worker: new Set<RouteId>(["pools.create", "pools.delete"]),
};

export interface Reply {
  status: number;
  body?: Json;
}

interface Route {
  id: RouteId;
  method: string;
  /** Path segments after `/internal/v1/pools/{pool}`. */
  rest: readonly string[];
  handle(ctx: { registry: PoolRegistry; pool: PoolRef; body: Record<string, unknown> }): Promise<Reply>;
}

const MAX_EMAIL = 320;
const MAX_PASSWORD = 1024;
const MAX_NAME = 256;
const MAX_TOKEN = 512;

function str(body: Record<string, unknown>, key: string, max: number): string {
  const v = body[key];
  if (typeof v !== "string" || v === "" || v.length > max) {
    throw new HttpError(400, "invalid_request", `${key} must be a non-empty string of at most ${max} characters`);
  }
  return v;
}

function optStr(body: Record<string, unknown>, key: string, max: number): string | undefined {
  return body[key] === undefined ? undefined : str(body, key, max);
}

interface BaUser {
  id: string;
  email: string;
  name: string;
  emailVerified: boolean;
  banned?: boolean | null;
  createdAt: Date | string;
  updatedAt: Date | string;
}

interface BaSession {
  id: string;
  userId: string;
  expiresAt: Date | string;
  createdAt: Date | string;
}

const iso = (d: Date | string) => new Date(d).toISOString();

export function userOut(u: BaUser): Json {
  return {
    id: u.id,
    email: u.email,
    name: u.name,
    email_verified: u.emailVerified,
    banned: u.banned === true,
    created_at: iso(u.createdAt),
    updated_at: iso(u.updatedAt),
  };
}

export function sessionOut(s: BaSession): Json {
  return { id: s.id, user_id: s.userId, expires_at: iso(s.expiresAt), created_at: iso(s.createdAt) };
}

function bearerHeaders(token: string): Headers {
  return new Headers({ authorization: `Bearer ${token}` });
}

const ROUTES: readonly Route[] = [
  {
    id: "pools.create",
    method: "POST",
    rest: [],
    async handle({ registry, pool }) {
      await registry.createPool(pool);
      return { status: 201, body: { pool: pool.name } };
    },
  },
  {
    id: "pools.delete",
    method: "DELETE",
    rest: [],
    async handle({ registry, pool }) {
      await registry.dropPool(pool);
      return { status: 204 };
    },
  },
  {
    id: "users.create",
    method: "POST",
    rest: ["users"],
    async handle({ registry, pool, body }) {
      const email = str(body, "email", MAX_EMAIL);
      const password = optStr(body, "password", MAX_PASSWORD);
      const name = optStr(body, "name", MAX_NAME) ?? "";
      const verified = body.email_verified;
      if (verified !== undefined && typeof verified !== "boolean") {
        throw new HttpError(400, "invalid_request", "email_verified must be a boolean");
      }
      const out = await registry.use(pool, (auth) =>
        auth.api.createUser({
          body: { email, name, ...(password === undefined ? {} : { password }), data: { emailVerified: verified === true } },
        }),
      );
      return { status: 201, body: { user: userOut(out.user as BaUser) } };
    },
  },
  {
    id: "sessions.create",
    method: "POST",
    rest: ["sessions"],
    async handle({ registry, pool, body }) {
      const email = str(body, "email", MAX_EMAIL);
      const password = str(body, "password", MAX_PASSWORD);
      return registry.use(pool, async (auth) => {
        const signedIn = await auth.api.signInEmail({ body: { email, password } });
        const found = await auth.api.getSession({
          headers: bearerHeaders(signedIn.token),
          query: { disableCookieCache: true, disableRefresh: true },
        });
        if (!found) throw new Error("a session just created did not resolve");
        return {
          status: 201,
          body: { session_id: found.session.id, session_token: signedIn.token, user: userOut(found.user as BaUser) },
        };
      });
    },
  },
  {
    id: "sessions.lookup",
    method: "POST",
    rest: ["sessions", "lookup"],
    async handle({ registry, pool, body }) {
      const token = str(body, "token", MAX_TOKEN);
      return registry.use(pool, async (auth) => {
        const found = await auth.api.getSession({
          headers: bearerHeaders(token),
          query: { disableCookieCache: true },
        });
        if (!found) throw new HttpError(404, "session_not_found", "no live session for this token");
        return {
          status: 200,
          body: { session: sessionOut(found.session as BaSession), user: userOut(found.user as BaUser) },
        };
      });
    },
  },
];

const PREFIX = "/internal/v1/pools/";

export interface Match {
  route: Route;
  /** The raw (undecoded) pool segment; validated by `resolvePool`. */
  poolSegment: string;
}

/** The route for a method and raw path, or null. Path segments are never decoded. */
export function match(method: string, path: string): Match | null {
  if (!path.startsWith(PREFIX)) return null;
  const [poolSegment, ...rest] = path.slice(PREFIX.length).split("/");
  for (const route of ROUTES) {
    if (route.method !== method || route.rest.length !== rest.length) continue;
    if (route.rest.every((s, i) => s === rest[i])) return { route, poolSegment: poolSegment ?? "" };
  }
  return null;
}

export function resolvePool(segment: string): PoolRef {
  const pool = parsePoolName(segment);
  if (!pool) throw new HttpError(400, "invalid_pool", "pool must be `members` or `app-<lowercase uuid>`");
  return pool;
}
