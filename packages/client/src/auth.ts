// `db.auth`: app user flows under `/auth/v1/app/`, and the session state the data requests use.
import { DataApiError, NO_REDIRECTS } from "./error.ts";
import type { AppUser, Auth, AuthEvent, AuthOptions, AuthStorage, DatabaseShape, Session } from "./types.ts";

/** Header carrying the signed-in user's access token. */
export const USER_HEADER = "chelabase-user";

const REFRESH_LEAD_MS = 60_000;
/** A token this close to expiry is refreshed before use. */
const STALE_MS = 5_000;
/** The refresh token is gone for good: sign out locally. */
const FATAL = new Set(["refresh_token_reused", "refresh_token_invalid", "user_session_revoked", "user_inactive"]);
/** A request carrying the access token was refused because its session is over. */
export const SESSION_GONE = "user_session_revoked";

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

/** `exp` (ms since the epoch) from a JWT's payload; 0 when unreadable. */
function expiryMs(jwt: string): number {
  try {
    const part = jwt.split(".")[1] ?? "";
    const b64 = part.replace(/-/g, "+").replace(/_/g, "/");
    const exp = (JSON.parse(atob(b64.padEnd(Math.ceil(b64.length / 4) * 4, "="))) as { exp?: unknown }).exp;
    return typeof exp === "number" ? exp * 1000 : 0;
  } catch {
    return 0;
  }
}

export function createAuth<DB extends DatabaseShape>(cfg: {
  base: string;
  key: string;
  fetch: typeof fetch;
  options: AuthOptions | undefined;
  /** Turns a non-2xx answer into the client's `DataApiError`. */
  toError: (res: Response, text: string) => DataApiError;
  server?: ServerSession;
}): AuthCore<DB> {
  const srv = cfg.server;
  const storage = srv ? memoryStorage() : (cfg.options?.storage ?? defaultStorage());
  const autoRefresh = !srv && (cfg.options?.autoRefresh ?? true);
  const storeKey = `chelabase.${cfg.key.slice(0, 12)}.refresh`;

  let access: string | undefined;
  let expMs = 0;
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

  const emit = (event: AuthEvent, user: AppUser | null) => {
    for (const cb of [...listeners]) cb(event, user);
  };

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
    const text = await res.text();
    if (!res.ok) throw cfg.toError(res, text);
    if (res.status === 204 || text === "") return undefined;
    try {
      return JSON.parse(text);
    } catch {
      throw new DataApiError({ status: res.status, code: "internal", message: "the auth API answered with invalid JSON" });
    }
  }

  function clearTimer() {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  }

  async function clearLocal(emitEvent: boolean) {
    clearTimer();
    access = undefined;
    expMs = 0;
    started = Promise.resolve();
    await storage.remove(storeKey);
    await srv?.onSession(null);
    if (emitEvent) emit("signed_out", null);
  }

  async function adopt(s: Session, event: AuthEvent) {
    await srv?.onSession(s); // the cookies first: a token the browser won't get back is never used
    access = s.access_token;
    expMs = expiryMs(s.access_token) || Date.now() + s.expires_in * 1000;
    started = Promise.resolve();
    await storage.set(storeKey, s.refresh_token);
    clearTimer();
    if (autoRefresh) {
      const t = setTimeout(() => void refresh().catch(() => {}), Math.max(0, expMs - REFRESH_LEAD_MS - Date.now()));
      (t as { unref?: () => void }).unref?.();
      timer = t;
    }
    emit(event, s.user);
  }

  function refresh(): Promise<Session> {
    inflight ??= (async () => {
      try {
        const token = await storage.get(storeKey);
        if (token === null) {
          throw new DataApiError({ status: 401, code: "refresh_token_invalid", message: "there is no session to refresh" });
        }
        const s = (await call("refresh", { body: { refresh_token: token } })) as Session;
        await adopt(s, "refreshed");
        return s;
      } catch (e) {
        if (staleRefresh(e)) {
          // Another request may have rotated past this token and written newer cookies: forget
          // the session here only, and go on anonymously.
          access = undefined;
          expMs = 0;
          started = Promise.resolve();
          await storage.remove(storeKey);
        } else if (e instanceof DataApiError && FATAL.has(e.code)) await clearLocal(true);
        throw e;
      } finally {
        inflight = undefined;
      }
    })();
    return inflight;
  }

  /** Server mode: a `refresh_token_invalid` leaves the cookies alone and the request anonymous. */
  function staleRefresh(e: unknown): boolean {
    return srv !== undefined && e instanceof DataApiError && e.code === "refresh_token_invalid";
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

  async function userCall(path: string, init: { method?: "GET" | "POST"; body?: unknown } = {}) {
    const token = await accessToken();
    if (token === undefined) {
      throw new DataApiError({ status: 401, code: "user_token_invalid", message: "not signed in" });
    }
    try {
      return await call(path, { ...init, token });
    } catch (e) {
      if (e instanceof DataApiError && e.code === SESSION_GONE) await dropSession(token);
      throw e;
    }
  }

  const auth: Auth<DB> = {
    async signUp(p) {
      const r = (await call("sign-up", { body: p })) as Session | { user: AppUser; verification_required?: boolean };
      if ("access_token" in r) {
        await adopt(r, "signed_in");
        return { user: r.user, session: r, verificationRequired: false };
      }
      return { user: r.user, session: null, verificationRequired: r.verification_required === true };
    },
    async signIn(p) {
      const s = (await call("sign-in", { body: p })) as Session;
      await adopt(s, "signed_in");
      return s;
    },
    async signOut() {
      try {
        // `call`, not `userCall`: the `finally` below signs out locally (and emits) exactly once.
        const token = access !== undefined ? await accessToken() : undefined;
        if (token !== undefined) await call("sign-out", { token });
        else {
          const token = await storage.get(storeKey);
          if (token !== null) await call("sign-out", { body: { refresh_token: token } });
        }
      } finally {
        await clearLocal(true);
      }
    },
    refresh,
    async getUser() {
      return (await userCall("user", { method: "GET" })) as AppUser;
    },
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
      const s = (await userCall("change-password", {
        body: { current_password: p.currentPassword, password: p.password },
      })) as Session;
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
    getUser: reject,
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
