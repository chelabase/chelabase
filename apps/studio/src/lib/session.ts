import { ApiError } from "@chelabase/management-client";
import { mintMemberToken } from "./login-client/generated/endpoints";
import type { SessionEvents } from "./realtime/types";

/** What `POST /auth/v1/token` answers. */
export interface MintedToken {
  access_token: string;
  /** Seconds until it expires. */
  expires_in: number;
}

export type SessionStatus = "unknown" | "signed-in" | "signed-out";

export interface SessionSnapshot {
  status: SessionStatus;
  token: string | undefined;
}

/**
 * Refresh this long before the token expires, at most: the lead is `min(REFRESH_LEAD_MS, ttl/2)`,
 * so a short token (auth allows 60 s) is reused for half its life instead of re-minted per call.
 */
export const REFRESH_LEAD_MS = 60_000;
/** After a failed refresh that wasn't a 401 (an outage), try again this much later. */
export const RETRY_MS = 15_000;
/** A floor for the refresh timer, so a very short token can't make it spin. */
const MIN_DELAY_MS = 5_000;

export interface SessionOptions {
  mint: () => Promise<MintedToken>;
}

export interface Session {
  /** The current token, or undefined when signed out or not yet started. */
  token(): string | undefined;
  snapshot(): SessionSnapshot;
  /** Mints the first token from the cookie session. False when there is no session (a 401). */
  start(): Promise<boolean>;
  /** A token with more than the lead time left; refreshes (single flight) when it has less. */
  getToken(): Promise<string>;
  /** Mints a new token. Concurrent callers share one `/token` call. A 401 signs the session out. */
  refresh(): Promise<string>;
  /** Drops the token and stops refreshing. */
  clear(): void;
  /** Called with each new token, and with null on sign-out. Returns the unsubscribe. */
  onChange(cb: (token: string | null) => void): () => void;
  /** For `useSyncExternalStore`: called on every snapshot change. */
  subscribe(cb: () => void): () => void;
}

const isUnauthorized = (err: unknown) => err instanceof ApiError && err.status === 401;

/**
 * The member token, held in memory only (never storage: a stolen token would outlive the tab).
 * The cookie session is what persists; the token is minted from it and renewed about a minute
 * before it expires (half its life, for a short one), by one shared call however many callers ask.
 */
export function createSession({ mint }: SessionOptions): Session {
  let snap: SessionSnapshot = { status: "unknown", token: undefined };
  let expiresAt = 0;
  // The lead for the current token: min(REFRESH_LEAD_MS, ttl/2).
  let leadMs = REFRESH_LEAD_MS;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let inFlight: Promise<string> | undefined;
  // Bumped by `clear`, so a mint that lands after sign-out is dropped.
  let epoch = 0;
  const listeners = new Set<(token: string | null) => void>();
  const subscribers = new Set<() => void>();

  const set = (next: SessionSnapshot) => {
    snap = next;
    for (const cb of [...subscribers]) cb();
  };
  const cancelTimer = () => {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  };
  const schedule = (delay: number) => {
    cancelTimer();
    timer = setTimeout(() => void refresh().catch(() => undefined), delay);
  };

  function refresh(): Promise<string> {
    if (inFlight) return inFlight;
    const mine = epoch;
    const p = (async () => {
      try {
        const minted = await mint();
        if (mine !== epoch) throw new ApiError(401, "unauthorized", "Signed out");
        const ttlMs = minted.expires_in * 1000;
        expiresAt = Date.now() + ttlMs;
        leadMs = Math.min(REFRESH_LEAD_MS, ttlMs / 2);
        schedule(Math.max(ttlMs - leadMs, MIN_DELAY_MS));
        set({ status: "signed-in", token: minted.access_token });
        for (const cb of [...listeners]) cb(minted.access_token);
        return minted.access_token;
      } catch (err) {
        if (mine === epoch) {
          if (isUnauthorized(err)) clear();
          // An outage: keep the token while it lasts and try again later.
          else if (snap.token !== undefined) schedule(RETRY_MS);
        }
        throw err;
      }
    })();
    inFlight = p;
    const done = () => {
      if (inFlight === p) inFlight = undefined;
    };
    p.then(done, done);
    return p;
  }

  function clear() {
    epoch++;
    cancelTimer();
    inFlight = undefined;
    expiresAt = 0;
    const wasIn = snap.token !== undefined;
    set({ status: "signed-out", token: undefined });
    if (wasIn) for (const cb of [...listeners]) cb(null);
  }

  return {
    token: () => snap.token,
    snapshot: () => snap,
    async start() {
      try {
        await refresh();
        return true;
      } catch (err) {
        if (isUnauthorized(err)) return false;
        throw err;
      }
    },
    getToken() {
      if (snap.token !== undefined && expiresAt - Date.now() > leadMs) return Promise.resolve(snap.token);
      return refresh();
    },
    refresh,
    clear,
    onChange(cb) {
      listeners.add(cb);
      return () => void listeners.delete(cb);
    },
    subscribe(cb) {
      subscribers.add(cb);
      return () => void subscribers.delete(cb);
    },
  };
}

/** The real thing: tokens from `POST /auth/v1/token`, which wants this header and Studio's Origin. */
export function createStudioSession(): Session {
  return createSession({ mint: () => mintMemberToken(undefined, { headers: { "X-Chelabase-Studio": "1" } }) });
}

/**
 * The realtime client's view of the session, in the shape it already listens for: a renewed
 * token is `userLoaded`, sign-out is `userUnloaded` and `userSignedOut`.
 */
export function realtimeAuthEvents(session: Session): SessionEvents {
  type Out = () => void;
  const loaded = new Map<(user: { access_token: string }) => void, Out>();
  const unloaded = new Map<Out, Out>();
  const signedOut = new Map<Out, Out>();
  const onOut = (map: Map<Out, Out>) => ({
    add: (cb: Out) => void map.set(cb, session.onChange((t) => t === null && cb())),
    remove: (cb: Out) => void (map.get(cb)?.(), map.delete(cb)),
  });
  return {
    addUserLoaded: (cb) => void loaded.set(cb, session.onChange((t) => t !== null && cb({ access_token: t }))),
    removeUserLoaded: (cb) => void (loaded.get(cb)?.(), loaded.delete(cb)),
    addUserUnloaded: onOut(unloaded).add,
    removeUserUnloaded: onOut(unloaded).remove,
    addUserSignedOut: onOut(signedOut).add,
    removeUserSignedOut: onOut(signedOut).remove,
  };
}
