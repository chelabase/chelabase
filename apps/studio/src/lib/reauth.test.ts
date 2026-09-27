import { describe, expect, it, vi } from "vitest";
import type { MeResponse } from "@cheladb/types";
import type { ApiClient } from "./api";
import { createReauthGuard, resetOnLoad } from "./reauth";

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

describe("resetOnLoad", () => {
  it("clears the guard when /v1/me succeeds", async () => {
    const guard = { shouldRedirect: vi.fn(), reset: vi.fn() };
    const me: MeResponse = { user: { sub: "u", org_id: "o" }, tenant: null };
    const api = { me: vi.fn().mockResolvedValue(me), createDatabase: vi.fn(), getDatabase: vi.fn() } as unknown as ApiClient;
    await expect(resetOnLoad(api, guard).me()).resolves.toEqual(me);
    expect(guard.reset).toHaveBeenCalledTimes(1);
  });

  it("keeps the guard when /v1/me fails", async () => {
    const guard = { shouldRedirect: vi.fn(), reset: vi.fn() };
    const api = {
      me: vi.fn().mockRejectedValue(new Error("missing org claim")),
      createDatabase: vi.fn(),
      getDatabase: vi.fn(),
    } as unknown as ApiClient;
    await expect(resetOnLoad(api, guard).me()).rejects.toThrow("missing org claim");
    expect(guard.reset).not.toHaveBeenCalled();
  });
});
