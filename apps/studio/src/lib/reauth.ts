import { ApiError } from "@chelabase/management-client";

const KEY = "chelabase.reauth-attempted";

type FlagStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export interface ReauthGuard {
  /**
   * Whether a 401 should start a recovery. True once; false until a load succeeds. That stops a
   * loop when the gateway rejects even a freshly minted token.
   */
  shouldRedirect(): boolean;
  /** Call after a successful load to allow a future recovery. */
  reset(): void;
}

/** A guard whose flag lives in `storage`, so it survives a reload. It holds a flag, never a token. */
export function createReauthGuard(storage: FlagStorage): ReauthGuard {
  return {
    shouldRedirect() {
      if (storage.getItem(KEY)) return false;
      storage.setItem(KEY, "1");
      return true;
    },
    reset() {
      storage.removeItem(KEY);
    },
  };
}

/**
 * An `onUnauthorized` that runs `recover` on the first 401 (returns true so the 401 carries
 * `signInRestarted`), and answers false after that so the screen shows the error instead of
 * looping.
 */
export function restartSignInOnce(guard: ReauthGuard, recover: () => void): () => boolean {
  return () => {
    if (!guard.shouldRedirect()) return false;
    recover();
    return true;
  };
}

export interface Recovery {
  /** Mints a fresh token from the cookie session. */
  refresh: () => Promise<unknown>;
  /** The fresh token is in place: run the failed queries again. */
  refreshed: () => void;
  /** The session is gone: go to `/login`. */
  toLogin: () => void;
}

/**
 * The management client's `onUnauthorized`: on a 401, mint a fresh token once and run the queries
 * again (returns true so the 401 carries `signInRestarted` and the screen waits). If `/token`
 * refuses too (401), the session is over and the user goes to `/login`. If a fresh token is
 * refused by the API as well, the guard answers false so the screen shows the error instead of
 * looping. The dashboard re-arms the guard on its first successful `getMe`.
 */
export function recoverFromRejectedToken(guard: ReauthGuard, { refresh, refreshed, toLogin }: Recovery): () => boolean {
  return restartSignInOnce(guard, () => {
    refresh().then(refreshed, (err: unknown) => {
      if (err instanceof ApiError && err.status === 401) toLogin();
    });
  });
}
