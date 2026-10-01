import { describe, expect, it, vi } from "vitest";
import { createReauthGuard, restartSignInOnce } from "./reauth";

function memoryStorage() {
  const data = new Map<string, string>();
  return {
    getItem: (k: string) => data.get(k) ?? null,
    setItem: (k: string, v: string) => void data.set(k, v),
    removeItem: (k: string) => void data.delete(k),
  };
}

describe("createReauthGuard", () => {
  it("allows one sign-in restart, then stops so the error can show", () => {
    const guard = createReauthGuard(memoryStorage());
    expect(guard.shouldRedirect()).toBe(true);
    // Back from Zitadel with a fresh token that the gateway still refuses:
    expect(guard.shouldRedirect()).toBe(false);
    expect(guard.shouldRedirect()).toBe(false);
  });

  it("allows a restart again after a successful load", () => {
    const guard = createReauthGuard(memoryStorage());
    guard.shouldRedirect();
    guard.reset();
    expect(guard.shouldRedirect()).toBe(true);
  });

  it("survives the redirect round-trip via storage", () => {
    const storage = memoryStorage();
    expect(createReauthGuard(storage).shouldRedirect()).toBe(true);
    // New page load after returning from Zitadel:
    expect(createReauthGuard(storage).shouldRedirect()).toBe(false);
  });
});

describe("restartSignInOnce", () => {
  it("restarts sign-in on the first 401 and reports it", () => {
    const restart = vi.fn();
    const onUnauthorized = restartSignInOnce(createReauthGuard(memoryStorage()), restart);
    expect(onUnauthorized()).toBe(true);
    expect(restart).toHaveBeenCalledTimes(1);
  });

  it("lets a second 401 through without restarting again", () => {
    const storage = memoryStorage();
    const restart = vi.fn();
    expect(restartSignInOnce(createReauthGuard(storage), restart)()).toBe(true);
    // Back from Zitadel; the fresh token is refused too.
    expect(restartSignInOnce(createReauthGuard(storage), restart)()).toBe(false);
    expect(restart).toHaveBeenCalledTimes(1);
  });
});
