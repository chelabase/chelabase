import { describe, expect, it } from "vitest";
import { callbackOutcome } from "./callback";

const base = { isLoading: false, isAuthenticated: false, error: undefined, hasAuthParams: false };

describe("callbackOutcome", () => {
  it("waits while the code exchange runs", () => {
    expect(callbackOutcome({ ...base, isLoading: true, hasAuthParams: true })).toBe("wait");
  });

  it("goes to the app once signed in", () => {
    expect(callbackOutcome({ ...base, isAuthenticated: true })).toBe("app");
  });

  it("reports a failed exchange", () => {
    expect(callbackOutcome({ ...base, error: new Error("invalid_grant") })).toBe("error");
  });

  // Review Focus 1.
  it("sends a bare /callback visit home instead of spinning forever", () => {
    expect(callbackOutcome(base)).toBe("home");
  });
});
