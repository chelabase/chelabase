// `db.auth`: app user flows under `/auth/v1/app/`, and the session state the data requests use.
import { DataApiError, NO_REDIRECTS } from "./error.ts";
import type { AppUser, Auth, AuthEvent, AuthOptions, AuthStorage, DatabaseShape, Session } from "./types.ts";

/** Header carrying the signed-in user's access token. */
export const USER_HEADER = "chelabase-user";

const REFRESH_LEAD_MS = 60_000;
/** A token this close to expiry is refreshed before use. */
const STALE_MS = 5_000;
/** An access token this long past its `exp` belongs to no rotation still in flight. */
const DEAD_AFTER_MS = 60_000;
/** The refresh token is gone for good: sign out locally. */
const FATAL = new Set(["refresh_token_reused", "refresh_token_invalid", "user_session_revoked", "user_inactive"]);
/** A request carrying the access token was refused because its session is over. */
export const SESSION_GONE = "user_session_revoked";
/** Answers that mean "signed out" to `currentUser`: a refused token, or a session that ended (as `refreshIfNeeded` treats it). */
const SIGNED_OUT = new Set(["user_token_invalid", "user_token_expired", ...FATAL]);

export interface AuthCore<DB extends DatabaseShape> {
  auth: Auth<DB>;
  /** The access token to send as `chelabase-user` (refreshing first when needed), or undefined. */
  accessToken(): Promise<string | undefined>;
  /** After a data `401 user_token_expired`: refresh once and return the new token. */
  refreshAfterExpiry(sent: string): Promise<string | undefined>;
  /** After a `401 user_session_revoked` for `sent`: sign out locally (if `sent` is still current). */
  dropSession(sent: string): Promise<void>;
}

/**
 * Server mode (`@chelabase/client/server`): the session lives in the request's cookies. No
 * timer; nothing is kept beyond this client.
 */
export interface ServerSession {
  /** The access token from the request; used while it has at least 5 s left. */
  access: string | undefined;
  /** The refresh token from the request; leave it undefined when the cookies can't be written. */
  refresh: string | undefined;
  /** Writes the session's cookies (or clears them). Runs before a new token is used; a throw stops its use. */
  onSession(s: Session | null): Promise<void>;
  /** Extra headers for the auth API only (the server key and the end user's IP). */
  headers: Record<string, string>;
}

/**
 * Remote mode (`@chelabase/client/browser`): the refresh token lives in an `HttpOnly` cookie
 * only the app's route (`createAuthHandler`) can use. Session flows post to that route; the
 * access token is read from its readable cookie before every use.
 */
export interface RemoteSession {
  /** The route's mount point, same-origin, without a trailing slash. */
  path: string;
  /** The access cookie's current value. */
  readAccess(): string | undefined;
  /** Deletes the readable access cookie, so a reload doesn't bring a dropped session back. */
  clearAccess(): void;
}

export function memoryStorage(): AuthStorage {
  const m = new Map<string, string>();
  return {
    get: (k) => m.get(k) ?? null,
    set: (k, v) => void m.set(k, v),
    remove: (k) => void m.delete(k),
  };
}

function defaultStorage(): AuthStorage {
  const ls = (globalThis as { localStorage?: Storage }).localStorage;
  if (!ls) return memoryStorage();
  return {
    get: (k) => ls.getItem(k),
    set: (k, v) => ls.setItem(k, v),
    remove: (k) => ls.removeItem(k),
  };
}

/** A JWT's (unverified) payload; empty when unreadable. */
function claims(jwt: string): { exp?: unknown; sub?: unknown } {
  try {
    const part = jwt.split(".")[1] ?? "";
    const b64 = part.replace(/-/g, "+").replace(/_/g, "/");
    const c = JSON.parse(atob(b64.padEnd(Math.ceil(b64.length / 4) * 4, "="))) as unknown;
    return typeof c === "object" && c !== null ? c : {};
  } catch {
    return {};
  }
}

/** `exp` (ms since the epoch) from a JWT's payload; 0 when unreadable. */
function expiryMs(jwt: string): number {
  const exp = claims(jwt).exp;
  return typeof exp === "number" ? exp * 1000 : 0;
}

/**
 * After a `refresh_token_invalid`: the access token is missing, unreadable, or more than 60 s
 * past its `exp`. A token that old can't be part of a rotation still in flight, so its cookies
 * are dead; keeping them would cost a refresh on every page view.
 */
export function longExpired(jwt: string | undefined): boolean {
  return jwt === undefined || expiryMs(jwt) < Date.now() - DEAD_AFTER_MS;
}

/** Whose token it is: `sub`, or undefined. Unverified: it only decides which event to emit. */
function subject(jwt: string | undefined): string | undefined {
  const sub = jwt === undefined ? undefined : claims(jwt).sub;
  return typeof sub === "string" ? sub : undefined;
}

export function createAuth<DB extends DatabaseShape>(cfg: {
  base: string;
  key: string;
  fetch: typeof fetch;
  options: AuthOptions | undefined;
  /** Turns a non-2xx answer into the client's `DataApiError`. */
  toError: (res: Response, text: string) => DataApiError;
  server?: ServerSession;
  remote?: RemoteSession;
}): AuthCore<DB> {
  const srv = cfg.server;
  const remote = cfg.remote;
  const storage = srv || remote ? memoryStorage() : (cfg.options?.storage ?? defaultStorage());
  const autoRefresh = !srv && (cfg.options?.autoRefresh ?? true);
  const storeKey = `chelabase.${cfg.key.slice(0, 12)}.refresh`;

  let access: string | undefined;
  let expMs = 0;
  /** Server mode: the access cookie's value as this client last read or wrote it. */
  let cookieAccess = srv?.access;
  if (srv?.refresh !== undefined) void storage.set(storeKey, srv.refresh);
  if (srv?.access !== undefined && expiryMs(srv.access) - Date.now() >= STALE_MS) {
    // Its unverified `exp` only decides whether to refresh; the gateway checks the token itself.
    access = srv.access;
    expMs = expiryMs(srv.access);
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  let inflight: Promise<Session> | undefined;
  let started: Promise<void> | undefined;
  const listeners = new Set<(event: AuthEvent, user: AppUser | null) => void>();
  /** Remote mode: the access cookie's value when last looked at. Only a change to it is acted on. */
  let seen = remote?.readAccess();
  if (seen !== undefined) {
    access = seen;
    expMs = expiryMs(seen);
    if (expMs - Date.now() >= STALE_MS) schedule(); // a stale one is refreshed on first use
  }

  const emit = (event: AuthEvent, user: AppUser | null) => {
    for (const cb of [...listeners]) cb(event, user);
  };

  /**
   * Remote mode: picks up a change to the access cookie (another tab signed in, out or
   * refreshed). `signed_in`/`signed_out` are emitted when the token's `sub` changes, with a
   * `null` user: the cookie carries no profile (`db.auth.getUser()` fetches it).
   */
  function sync() {
    if (!remote) return;
    const cur = remote.readAccess();
    if (cur === seen) return;
    seen = cur;
    const before = subject(access);
    if (cur === undefined) {
      clearTimer();
      access = undefined;
      expMs = 0;
      if (before !== undefined) emit("signed_out", null);
      return;
    }
    access = cur;
    expMs = expiryMs(cur);
    clearTimer();
    if (expMs - Date.now() >= STALE_MS) schedule();
    if (subject(cur) !== before) emit("signed_in", null);
  }

  /** Remote mode: a session flow through the app's route (same-origin; the route adds the key and cookies). */
  async function routeCall(flow: string, body?: unknown): Promise<unknown> {
    const res = await cfg.fetch(`${remote!.path}/${flow}`, {
      method: "POST",
      headers: { "content-type": "application/json", accept: "application/json" },
      credentials: "same-origin",
      redirect: NO_REDIRECTS,
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    return answer(res);
  }

  async function answer(res: Response): Promise<unknown> {
    const text = await res.text();
    if (!res.ok) throw cfg.toError(res, text);
    if (res.status === 204 || text === "") return undefined;
    try {
      return JSON.parse(text);
    } catch {
      throw new DataApiError({ status: res.status, code: "internal", message: "the auth API answered with invalid JSON" });
    }
  }

  async function call(
    path: string,
    init: { method?: "GET" | "POST"; body?: unknown; token?: string },
  ): Promise<unknown> {
    const headers: Record<string, string> = {
      ...srv?.headers,
      authorization: `Bearer ${cfg.key}`,
      accept: "application/json",
    };
    if (init.token !== undefined) headers[USER_HEADER] = init.token;
    const method = init.method ?? "POST";
    if (method === "POST") headers["content-type"] = "application/json";
    const res = await cfg.fetch(`${cfg.base}/auth/v1/app/${path}`, {
      method,
      headers,
      redirect: NO_REDIRECTS,
      ...(method === "POST" ? { body: JSON.stringify(init.body ?? {}) } : {}),
    });
    return answer(res);
  }

  /** A session flow: through the app's route in remote mode, else straight to the auth API. */
  const flow = (path: string, body?: unknown) => (remote ? routeCall(path, body) : call(path, { body }));

  function clearTimer() {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  }

  /** `force`: a sign-out, which drops the access cookie even if it changed meanwhile. */
  async function clearLocal(emitEvent: boolean, force = false) {
    const prev = access;
    clearTimer();
    access = undefined;
    expMs = 0;
    started = Promise.resolve();
    await storage.remove(storeKey);
    await srv?.onSession(null);
    cookieAccess = undefined;
    if (remote) {
      // The route usually cleared the cookies already; when it couldn't (unreachable, or the session
      // ended on a data request), the readable one goes here. A newer cookie another tab wrote
      // meanwhile is left alone, and taken up by the next `sync`.
      const cur = remote.readAccess();
      if (force || cur === prev) {
        if (cur !== undefined) remote.clearAccess();
        seen = remote.readAccess();
      }
    }
    if (emitEvent) emit("signed_out", null);
  }

  async function adopt(s: Session, event: AuthEvent) {
    await srv?.onSession(s); // the cookies first: a token the browser won't get back is never used
    cookieAccess = s.access_token;
    access = s.access_token;
    expMs = expiryMs(s.access_token) || Date.now() + s.expires_in * 1000;
    started = Promise.resolve();
    // Remote mode: the route set the cookie to this token, and keeps the refresh token to itself.
    if (remote) seen = s.access_token;
    else await storage.set(storeKey, s.refresh_token);
    schedule();
    emit(event, s.user);
  }

  function schedule() {
    clearTimer();
    if (!autoRefresh || access === undefined) return;
    const planned = access;
    const t = setTimeout(() => {
      sync(); // remote: another tab may have refreshed already
      if (access === planned) void refresh().catch(() => {});
    }, Math.max(0, expMs - REFRESH_LEAD_MS - Date.now()));
    (t as { unref?: () => void }).unref?.();
    timer = t;
  }

  function refresh(): Promise<Session> {
    inflight ??= (async () => {
      /** Whether a refresh token was actually sent: only the server's own answer can clear cookies. */
      let sent = false;
      try {
        let s: Session;
        if (remote) s = (await routeCall("refresh")) as Session;
        else {
          const token = await storage.get(storeKey);
          if (token === null) {
            throw new DataApiError({ status: 401, code: "refresh_token_invalid", message: "there is no session to refresh" });
          }
          sent = true;
          s = (await call("refresh", { body: { refresh_token: token } })) as Session;
        }
        await adopt(s, "refreshed");
        return s;
      } catch (e) {
        if (staleRefresh(e)) {
          // Another request may have rotated past this token and written newer cookies: forget
          // the session here only, and go on anonymously.
          const prev = access;
          access = undefined;
          expMs = 0;
          clearTimer();
          started = Promise.resolve();
          await storage.remove(storeKey);
          if (srv && sent && longExpired(cookieAccess)) {
            // A token that old can't be part of a rotation still in flight, and keeping dead
            // cookies costs a refresh on every page view: clear them (a no-op without `setAll`).
            // Best effort: the request goes on anonymously either way.
            await srv.onSession(null).catch(() => {});
            cookieAccess = undefined;
          }
          if (remote) {
            // A newer cookie (another tab) is taken up; an unchanged one is the session that ended.
            const cur = remote.readAccess();
            if (cur !== undefined && cur !== seen) {
              access = prev;
              sync();
            } else if (prev !== undefined) emit("signed_out", null);
          }
        } else if (e instanceof DataApiError && FATAL.has(e.code)) await clearLocal(true);
        throw e;
      } finally {
        inflight = undefined;
      }
    })();
    return inflight;
  }

  /**
   * Server and remote mode: a `refresh_token_invalid` leaves the request anonymous, and the
   * cookies alone unless their access token is long dead (`longExpired`).
   */
  function staleRefresh(e: unknown): boolean {
    return (srv !== undefined || remote !== undefined) && e instanceof DataApiError && e.code === "refresh_token_invalid";
  }

  /** On the first use, a stored refresh token (a returning visitor) is exchanged for an access token. */
  function ensureStarted(): Promise<void> {
    started ??= (async () => {
      const token = await storage.get(storeKey);
      if (token !== null && access === undefined) await refresh();
    })().catch((e: unknown) => {
      started = undefined; // a network failure: try again on the next call
      throw e;
    });
    return started;
  }

  async function accessToken(): Promise<string | undefined> {
    sync();
    try {
      await ensureStarted();
      if (access !== undefined && expMs - Date.now() < STALE_MS) await refresh();
    } catch (e) {
      if (!staleRefresh(e)) throw e;
    }
    return access;
  }

  async function refreshAfterExpiry(sent: string): Promise<string | undefined> {
    // Another request may have refreshed already: only refresh if the rejected token is still current.
    sync();
    try {
      if (access === sent) await refresh();
    } catch (e) {
      if (!staleRefresh(e)) throw e;
    }
    return access;
  }

  async function dropSession(sent: string): Promise<void> {
    // A newer session (signed in again meanwhile) is left alone.
    if (access === sent) await clearLocal(true);
  }

  async function userCall(path: string, init: { method?: "GET" | "POST"; body?: unknown } = {}, viaRoute = false) {
    const token = await accessToken();
    if (token === undefined) {
      throw new DataApiError({ status: 401, code: "user_token_invalid", message: "not signed in" });
    }
    try {
      // Through the route, the access cookie (just synced and refreshed) carries the token.
      return await (viaRoute ? routeCall(path, init.body) : call(path, { ...init, token }));
    } catch (e) {
      if (e instanceof DataApiError && e.code === SESSION_GONE) await dropSession(token);
      throw e;
    }
  }

  async function currentUser(): Promise<AppUser | null> {
    try {
      let token = await accessToken();
      for (let retry = true; ; retry = false) {
        if (token === undefined) return null;
        try {
          return (await call("user", { method: "GET", token })) as AppUser;
        } catch (e) {
          if (!(e instanceof DataApiError)) throw e;
          if (e.code === SESSION_GONE) await dropSession(token);
          if (!retry || e.code !== "user_token_expired") throw e;
          token = await refreshAfterExpiry(token); // one refresh, one retry
        }
      }
    } catch (e) {
      if (e instanceof DataApiError && SIGNED_OUT.has(e.code)) return null;
      throw e;
    }
  }

  const auth: Auth<DB> = {
    async signUp(p) {
      const r = (await flow("sign-up", p)) as Session | { user: AppUser; verification_required?: boolean };
      if ("access_token" in r) {
        await adopt(r, "signed_in");
        return { user: r.user, session: r, verificationRequired: false };
      }
      return { user: r.user, session: null, verificationRequired: r.verification_required === true };
    },
    async signIn(p) {
      const s = (await flow("sign-in", p)) as Session;
      await adopt(s, "signed_in");
      return s;
    },
    async signOut() {
      try {
        if (remote) {
          await routeCall("sign-out"); // the route sends the refresh cookie and clears both
          return;
        }
        // `call`, not `userCall`: the `finally` below signs out locally (and emits) exactly once.
        const token = access !== undefined ? await accessToken() : undefined;
        if (token !== undefined) await call("sign-out", { token });
        else {
          const token = await storage.get(storeKey);
          if (token !== null) await call("sign-out", { body: { refresh_token: token } });
        }
      } finally {
        await clearLocal(true, true);
      }
    },
    refresh,
    async refreshIfNeeded() {
      try {
        await accessToken();
      } catch (e) {
        // The session ended (and was signed out locally): nothing left to refresh.
        if (!(e instanceof DataApiError && FATAL.has(e.code))) throw e;
      }
    },
    async getUser() {
      return (await userCall("user", { method: "GET" })) as AppUser;
    },
    currentUser,
    async verifyEmail(p) {
      await call("verify-email", { body: p });
    },
    async resendCode(p) {
      await call("resend-code", { body: p });
    },
    async requestPasswordReset(p) {
      await call("password-reset", { body: p });
    },
    async confirmPasswordReset(p) {
      await call("password-reset/confirm", { body: p });
    },
    async changeEmail(p) {
      await userCall("change-email", { body: p });
    },
    async changePassword(p) {
      const s = (await userCall(
        "change-password",
        { body: { current_password: p.currentPassword, password: p.password } },
        remote !== undefined,
      )) as Session;
      await adopt(s, "refreshed"); // the old refresh token is revoked: keep the new pair
      return s;
    },
    onChange(cb) {
      listeners.add(cb);
      return () => void listeners.delete(cb);
    },
  };

  return { auth, accessToken, refreshAfterExpiry, dropSession };
}

/** `accessToken` mode: the caller owns the session. Nothing is stored, refreshed or timed. */
export function staticAuth<DB extends DatabaseShape>(
  accessToken: () => string | undefined | Promise<string | undefined>,
): AuthCore<DB> {
  const unavailable = () =>
    new Error("db.auth is unavailable when createClient is given `accessToken`: the caller owns the session");
  const reject = () => Promise.reject(unavailable());
  const auth: Auth<DB> = {
    signUp: reject,
    signIn: reject,
    signOut: reject,
    refresh: reject,
    refreshIfNeeded: reject,
    getUser: reject,
    currentUser: reject,
    verifyEmail: reject,
    resendCode: reject,
    requestPasswordReset: reject,
    confirmPasswordReset: reject,
    changeEmail: reject,
    changePassword: reject,
    onChange() {
      throw unavailable();
    },
  };
  return {
    auth,
    accessToken: async () => accessToken(),
    refreshAfterExpiry: async () => undefined,
    dropSession: async () => {},
  };
}
