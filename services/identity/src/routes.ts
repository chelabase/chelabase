// Our own routes. Better Auth's HTTP handler is never mounted.
//
// Public `auth.api.*` calls are used where Better Auth allows them server-side (createUser,
// signInEmail, getSession). Its admin endpoints refuse a call without an admin session, so
// the admin routes call `(await auth.$context).internalAdapter` with the same calls the
// admin endpoint makes. Each such wrapper names the endpoint it mirrors in
// better-auth@1.7.15 `dist/plugins/admin/routes.mjs`; re-check them on every upgrade.
import * as z from "zod";
import type { Auth } from "./better-auth.ts";
import type { Caller } from "./config.ts";
import { HttpError, type Json } from "./http.ts";
import { parsePoolName, type PoolRef } from "./pool-name.ts";
import type { PoolRegistry } from "./registry.ts";

export type RouteId =
  | "pools.create"
  | "pools.delete"
  | "users.create"
  | "users.list"
  | "users.get"
  | "users.set_email"
  | "users.set_password"
  | "users.verify_email"
  | "users.ban"
  | "users.unban"
  | "users.delete"
  | "sessions.create"
  | "sessions.lookup"
  | "sessions.get"
  | "sessions.revoke"
  | "sessions.revoke_all";

/** What each caller may do. Anything not listed answers 403. */
export const ALLOW: Record<Caller, ReadonlySet<RouteId>> = {
  // Member and app-user flows: everything but pool lifecycle.
  auth: new Set<RouteId>([
    "users.create",
    "users.list",
    "users.get",
    "users.set_email",
    "users.set_password",
    "users.verify_email",
    "users.ban",
    "users.unban",
    "users.delete",
    "sessions.create",
    "sessions.lookup",
    "sessions.get",
    "sessions.revoke",
    "sessions.revoke_all",
  ]),
  // App-user admin in the management API: no sign-in and no session tokens. It may set an
  // initial password when it creates a user, but never changes one afterwards.
  orchestrator: new Set<RouteId>([
    "users.create",
    "users.list",
    "users.get",
    "users.set_email",
    "users.ban",
    "users.unban",
    "users.delete",
    "sessions.revoke",
    "sessions.revoke_all",
  ]),
  // Pool lifecycle and the users sync.
  worker: new Set<RouteId>(["pools.create", "pools.delete", "users.list", "users.get"]),
};

export interface Reply {
  status: number;
  body?: Json;
}

interface Ctx {
  registry: PoolRegistry;
  pool: PoolRef;
  /** The `:id` path segment, already checked against ID_PATTERN. */
  id: string;
  query: URLSearchParams;
  body: Record<string, unknown>;
}

interface Route {
  id: RouteId;
  method: string;
  /** Path segments after `/internal/v1/pools/{pool}`; `:id` matches one id segment. */
  rest: readonly string[];
  handle(ctx: Ctx): Promise<Reply>;
}

const MAX_EMAIL = 320;
const MAX_PASSWORD = 1024;
const MAX_NAME = 256;
const MAX_TOKEN = 512;
const MAX_REASON = 512;
const MAX_PAGE = 200;
const DEFAULT_PAGE = 50;
/** Better Auth ids are random alphanumeric strings; the raw segment must look like one. */
const ID_PATTERN = /^[A-Za-z0-9_-]{1,128}$/;
/**
 * A session token as the bearer plugin accepts it: RFC 6750's b64token charset, plus `%` for a
 * URL-encoded signed token. Anything else (CR, LF, spaces) could not form a header anyway.
 */
const TOKEN_PATTERN = /^[A-Za-z0-9._~+/=%-]{1,512}$/;

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

function optBool(body: Record<string, unknown>, key: string): boolean | undefined {
  const v = body[key];
  if (v !== undefined && typeof v !== "boolean") throw new HttpError(400, "invalid_request", `${key} must be a boolean`);
  return v;
}

function intParam(query: URLSearchParams, key: string, fallback: number, min: number, max: number): number {
  const raw = query.get(key);
  if (raw === null) return fallback;
  const n = /^[0-9]{1,9}$/.test(raw) ? Number(raw) : NaN;
  if (!(n >= min && n <= max)) throw new HttpError(400, "invalid_request", `${key} must be an integer from ${min} to ${max}`);
  return n;
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
  token: string;
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

/** Never includes the token. */
export function sessionOut(s: BaSession): Json {
  return { id: s.id, user_id: s.userId, expires_at: iso(s.expiresAt), created_at: iso(s.createdAt) };
}

function bearerHeaders(token: string): Headers {
  return new Headers({ authorization: `Bearer ${token}` });
}

type Context = Awaited<Auth["$context"]>;

async function context(auth: Auth): Promise<Context> {
  return auth.$context;
}

async function requireUser(ctx: Context, id: string): Promise<BaUser> {
  const user = (await ctx.internalAdapter.findUserById(id)) as BaUser | null;
  if (!user) throw new HttpError(404, "user_not_found", "no such user");
  return user;
}

/**
 * A live session by id, with its user. Better Auth has no by-id lookup; this is
 * `internalAdapter.findSession` (dist/db/internal-adapter.mjs) keyed by id instead of token,
 * plus getSession's rule that an expired session does not resolve.
 */
async function findSessionById(ctx: Context, id: string): Promise<{ session: BaSession; user: BaUser } | null> {
  const row = (await ctx.adapter.findOne({
    model: "session",
    where: [{ field: "id", value: id }],
    join: { user: true },
  })) as (BaSession & { user?: BaUser | null }) | null;
  if (!row?.user) return null;
  const { user, ...session } = row;
  if (new Date(session.expiresAt).getTime() <= Date.now()) return null;
  return { session, user };
}

/**
 * A banned user has no live session. A sign-in that races a ban can insert its session after
 * the ban's sweep (the admin plugin checks `banned` before the insert, the ban deletes
 * sessions after its update), so every read re-checks and deletes what it finds.
 */
async function dropIfBanned(ctx: Context, session: BaSession, user: BaUser): Promise<boolean> {
  if (user.banned !== true) return false;
  await ctx.internalAdapter.deleteSession(session.token);
  return true;
}

/**
 * Better Auth's password length limits (`assertPasswordNotTooShort` / `...TooLong` in
 * dist/utils/password.mjs, which the package does not export), read from its own config.
 */
function checkPasswordLength(ctx: Context, password: string): void {
  const { minPasswordLength, maxPasswordLength } = ctx.password.config;
  if (password.length < minPasswordLength || password.length > maxPasswordLength) {
    throw new HttpError(400, "weak_password", `password must be ${minPasswordLength} to ${maxPasswordLength} characters`);
  }
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
      const verified = optBool(body, "email_verified") === true;
      // `auth.api.createUser` is the one admin endpoint that allows a sessionless server call.
      const out = await registry.use(pool, (auth) =>
        auth.api.createUser({
          body: { email, name, ...(password === undefined ? {} : { password }), data: { emailVerified: verified } },
        }),
      );
      return { status: 201, body: { user: userOut(out.user as BaUser) } };
    },
  },
  {
    // Mirrors `listUsers` (/admin/list-users), oldest first so pages are stable.
    id: "users.list",
    method: "GET",
    rest: ["users"],
    async handle({ registry, pool, query }) {
      const limit = intParam(query, "limit", DEFAULT_PAGE, 1, MAX_PAGE);
      const offset = intParam(query, "offset", 0, 0, 1_000_000_000);
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        const users = (await ctx.internalAdapter.listUsers(limit, offset, {
          field: "createdAt",
          direction: "asc",
        })) as BaUser[];
        const total = await ctx.internalAdapter.countTotalUsers();
        return { status: 200, body: { users: users.map(userOut), total, limit, offset } };
      });
    },
  },
  {
    // Mirrors `getUser` (/admin/get-user).
    id: "users.get",
    method: "GET",
    rest: ["users", ":id"],
    async handle({ registry, pool, id }) {
      return registry.use(pool, async (auth) => {
        const user = await requireUser(await context(auth), id);
        return { status: 200, body: { user: userOut(user) } };
      });
    },
  },
  {
    // Mirrors the email branch of `adminUpdateUser` (/admin/update-user): lowercase, z.email(),
    // refuse an email another user holds. `email_verified` is passed through in the same
    // update, false unless given.
    id: "users.set_email",
    method: "PUT",
    rest: ["users", ":id", "email"],
    async handle({ registry, pool, id, body }) {
      const email = str(body, "email", MAX_EMAIL).toLowerCase();
      const verified = optBool(body, "email_verified") === true;
      if (!z.email().safeParse(email).success) throw new HttpError(400, "invalid_email", "invalid email");
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        const holder = await ctx.internalAdapter.findUserByEmail(email);
        if (holder && holder.user.id !== id) throw new HttpError(409, "conflict", "another user has this email");
        await requireUser(ctx, id);
        const user = (await ctx.internalAdapter.updateUser(id, { email, emailVerified: verified })) as BaUser;
        return { status: 200, body: { user: userOut(user) } };
      });
    },
  },
  {
    // Mirrors `setUserPassword` (/admin/set-user-password).
    id: "users.set_password",
    method: "PUT",
    rest: ["users", ":id", "password"],
    async handle({ registry, pool, id, body }) {
      const password = str(body, "password", MAX_PASSWORD);
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        checkPasswordLength(ctx, password);
        const user = await requireUser(ctx, id);
        const hashed = await ctx.password.hash(password);
        if (await ctx.internalAdapter.findCredentialAccount(id)) await ctx.internalAdapter.updatePassword(id, hashed);
        else await ctx.internalAdapter.createAccount({ userId: id, providerId: "credential", accountId: user.id, password: hashed });
        return { status: 200, body: { status: true } };
      });
    },
  },
  {
    // Mirrors `adminUpdateUser` (/admin/update-user) with `{ emailVerified: true }`.
    id: "users.verify_email",
    method: "POST",
    rest: ["users", ":id", "verify-email"],
    async handle({ registry, pool, id }) {
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        await requireUser(ctx, id);
        const user = (await ctx.internalAdapter.updateUser(id, { emailVerified: true })) as BaUser;
        return { status: 200, body: { user: userOut(user) } };
      });
    },
  },
  {
    // Mirrors `banUser` (/admin/ban-user): a permanent ban, then every session revoked. The
    // admin plugin's session-create hook refuses sign-in while banned.
    id: "users.ban",
    method: "POST",
    rest: ["users", ":id", "ban"],
    async handle({ registry, pool, id, body }) {
      const reason = optStr(body, "reason", MAX_REASON);
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        await requireUser(ctx, id);
        const user = (await ctx.internalAdapter.updateUser(id, {
          banned: true,
          banReason: reason || "No reason",
          banExpires: null,
          updatedAt: new Date(),
        })) as BaUser;
        await ctx.internalAdapter.deleteUserSessions(id);
        return { status: 200, body: { user: userOut(user) } };
      });
    },
  },
  {
    // Mirrors `unbanUser` (/admin/unban-user).
    id: "users.unban",
    method: "POST",
    rest: ["users", ":id", "unban"],
    async handle({ registry, pool, id }) {
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        await requireUser(ctx, id);
        const user = (await ctx.internalAdapter.updateUser(id, {
          banned: false,
          banExpires: null,
          banReason: null,
          updatedAt: new Date(),
        })) as BaUser;
        return { status: 200, body: { user: userOut(user) } };
      });
    },
  },
  {
    // Mirrors `removeUser` (/admin/remove-user).
    id: "users.delete",
    method: "DELETE",
    rest: ["users", ":id"],
    async handle({ registry, pool, id }) {
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        await requireUser(ctx, id);
        await ctx.internalAdapter.deleteUserSessions(id);
        await ctx.internalAdapter.deleteUser(id);
        return { status: 204 };
      });
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
        const ctx = await context(auth);
        const found = await auth.api.getSession({
          headers: bearerHeaders(signedIn.token),
          query: { disableCookieCache: true, disableRefresh: true },
        });
        if (!found) {
          // A concurrent ban or revoke deleted the new session before it was read back.
          const user = (await ctx.internalAdapter.findUserById(signedIn.user.id)) as BaUser | null;
          if (user?.banned === true) throw new HttpError(403, "banned", "user is banned");
          throw new HttpError(401, "session_revoked", "the new session was revoked during sign-in");
        }
        // The ban committed between the admin plugin's check and the insert.
        if (await dropIfBanned(ctx, found.session as BaSession, found.user as BaUser)) {
          throw new HttpError(403, "banned", "user is banned");
        }
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
      if (!TOKEN_PATTERN.test(token)) throw new HttpError(400, "invalid_request", "token has characters a session token never has");
      return registry.use(pool, async (auth) => {
        const found = await auth.api.getSession({
          headers: bearerHeaders(token),
          query: { disableCookieCache: true },
        });
        if (!found || (await dropIfBanned(await context(auth), found.session as BaSession, found.user as BaUser))) throw new HttpError(404, "session_not_found", "no live session for this token");
        return {
          status: 200,
          body: { session: sessionOut(found.session as BaSession), user: userOut(found.user as BaUser) },
        };
      });
    },
  },
  {
    id: "sessions.get",
    method: "GET",
    rest: ["sessions", ":id"],
    async handle({ registry, pool, id }) {
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        const found = await findSessionById(ctx, id);
        if (!found || (await dropIfBanned(ctx, found.session, found.user))) throw new HttpError(404, "session_not_found", "no live session with this id");
        return { status: 200, body: { session: sessionOut(found.session), user: userOut(found.user) } };
      });
    },
  },
  {
    // Mirrors `revokeUserSession` (/admin/revoke-user-session), addressed by id.
    id: "sessions.revoke",
    method: "DELETE",
    rest: ["sessions", ":id"],
    async handle({ registry, pool, id }) {
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        const found = await findSessionById(ctx, id);
        if (!found) throw new HttpError(404, "session_not_found", "no live session with this id");
        await ctx.internalAdapter.deleteSession(found.session.token);
        return { status: 204 };
      });
    },
  },
  {
    // Mirrors `revokeUserSessions` (/admin/revoke-user-sessions).
    id: "sessions.revoke_all",
    method: "DELETE",
    rest: ["users", ":id", "sessions"],
    async handle({ registry, pool, id }) {
      return registry.use(pool, async (auth) => {
        const ctx = await context(auth);
        await requireUser(ctx, id);
        await ctx.internalAdapter.deleteUserSessions(id);
        return { status: 204 };
      });
    },
  },
];

const PREFIX = "/internal/v1/pools/";

export interface Match {
  route: Route;
  /** The raw (undecoded) pool segment; validated by `resolvePool`. */
  poolSegment: string;
  /** The raw `:id` segment, if the route has one; validated by `resolveId`. */
  idSegment: string;
}

/** The route for a method and raw path, or null. Path segments are never decoded. */
export function match(method: string, path: string): Match | null {
  if (!path.startsWith(PREFIX)) return null;
  const [poolSegment = "", ...rest] = path.slice(PREFIX.length).split("/");
  for (const route of ROUTES) {
    if (route.method !== method || route.rest.length !== rest.length) continue;
    let idSegment = "";
    const ok = route.rest.every((s, i) => {
      if (s !== ":id") return s === rest[i];
      idSegment = rest[i]!;
      return true;
    });
    if (ok) return { route, poolSegment, idSegment };
  }
  return null;
}

export function resolvePool(segment: string): PoolRef {
  const pool = parsePoolName(segment);
  if (!pool) throw new HttpError(400, "invalid_pool", "pool must be `members` or `app-<lowercase uuid>`");
  return pool;
}

export function resolveId(route: Route, segment: string): string {
  if (!route.rest.includes(":id")) return "";
  if (!ID_PATTERN.test(segment)) throw new HttpError(400, "invalid_request", "invalid id");
  return segment;
}
