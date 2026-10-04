const KEY = "chelabase.reauth-attempted";

type FlagStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export interface ReauthGuard {
  /**
   * Whether a 401 should restart sign-in. True once; false while the previous
   * restart hasn't been followed by a successful load. That stops a redirect
   * loop when the gateway rejects even a freshly issued token.
   */
  shouldRedirect(): boolean;
  /** Call after a successful load to allow a future restart. */
  reset(): void;
}

/** A guard whose flag lives in `storage` so it survives the Zitadel round-trip. */
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
 * The management client's `onUnauthorized`: restart sign-in once (returns true so
 * the 401 carries `signInRestarted`); if a fresh token is refused too, return
 * false so the screen shows the error instead of looping through Zitadel. The
 * dashboard re-arms the guard on its first successful `getMe`.
 */
export function restartSignInOnce(guard: ReauthGuard, restart: () => void): () => boolean {
  return () => {
    if (!guard.shouldRedirect()) return false;
    restart();
    return true;
  };
}
