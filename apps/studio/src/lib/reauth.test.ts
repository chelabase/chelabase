import { ApiError } from "@chelabase/management-client";
import { describe, expect, it, vi } from "vitest";
import { createReauthGuard, recoverFromRejectedToken } from "./reauth";

function memoryStorage() {
  const data = new Map<string, string>();
  return {
    getItem: (k: string) => data.get(k) ?? null,
    setItem: (k: string, v: string) => void data.set(k, v),
    removeItem: (k: string) => void data.delete(k),
  };
}

describe("createReauthGuard", () => {
  it("allows one recovery, then stops so the error can show", () => {
    const guard = createReauthGuard(memoryStorage());
    expect(guard.shouldRedirect()).toBe(true);
    expect(guard.shouldRedirect()).toBe(false);
    expect(guard.shouldRedirect()).toBe(false);
  });

  it("allows a recovery again after a successful load", () => {
    const guard = createReauthGuard(memoryStorage());
    guard.shouldRedirect();
    guard.reset();
    expect(guard.shouldRedirect()).toBe(true);
  });

  it("survives a page load via storage", () => {
    const storage = memoryStorage();
    expect(createReauthGuard(storage).shouldRedirect()).toBe(true);
    expect(createReauthGuard(storage).shouldRedirect()).toBe(false);
  });
});

describe("recoverFromRejectedToken", () => {
  const flush = () => new Promise((r) => setTimeout(r, 0));

  it("a_rejected_token_is_refreshed_once_and_the_queries_run_again", async () => {
    const refresh = vi.fn(async () => "fresh");
    const refreshed = vi.fn();
    const toLogin = vi.fn();
    const on401 = recoverFromRejectedToken(createReauthGuard(memoryStorage()), { refresh, refreshed, toLogin });
    expect(on401()).toBe(true);
    await flush();
    expect(refresh).toHaveBeenCalledTimes(1);
    expect(refreshed).toHaveBeenCalledTimes(1);
    expect(toLogin).not.toHaveBeenCalled();
  });

  it("token_401_redirects_once", async () => {
    const refresh = vi.fn(async () => {
      throw new ApiError(401, "unauthorized", "no session");
    });
    const toLogin = vi.fn();
    const guard = createReauthGuard(memoryStorage());
    const on401 = recoverFromRejectedToken(guard, { refresh, refreshed: vi.fn(), toLogin });
    expect(on401()).toBe(true);
    // Every request in flight sees its own 401; none of them starts a second recovery.
    expect(on401()).toBe(false);
    expect(on401()).toBe(false);
    await flush();
    expect(refresh).toHaveBeenCalledTimes(1);
    expect(toLogin).toHaveBeenCalledTimes(1);
  });

  it("a_fresh_token_the_gateway_still_refuses_does_not_loop", async () => {
    const refresh = vi.fn(async () => "fresh");
    const refreshed = vi.fn();
    const toLogin = vi.fn();
    const guard = createReauthGuard(memoryStorage());
    const on401 = recoverFromRejectedToken(guard, { refresh, refreshed, toLogin });
    expect(on401()).toBe(true);
    await flush();
    // The retried request is refused as well: the error shows, nothing restarts.
    expect(on401()).toBe(false);
    await flush();
    expect(refresh).toHaveBeenCalledTimes(1);
    expect(toLogin).not.toHaveBeenCalled();
  });

  it("a_refresh_that_fails_for_another_reason_does_not_send_the_user_to_login", async () => {
    const refresh = vi.fn(async () => {
      throw new ApiError(503, "unavailable", "down");
    });
    const toLogin = vi.fn();
    recoverFromRejectedToken(createReauthGuard(memoryStorage()), { refresh, refreshed: vi.fn(), toLogin })();
    await flush();
    expect(toLogin).not.toHaveBeenCalled();
  });
});
