import type { ApiClient } from "./api";

const KEY = "cheladb.reauth-attempted";

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

/** Wrap `api` so a successful `/v1/me` re-arms the guard. */
export function resetOnLoad(api: ApiClient, guard: ReauthGuard): ApiClient {
  return {
    ...api,
    me: async () => {
      const me = await api.me();
      guard.reset();
      return me;
    },
  };
}
