import { describe, expect, it, vi } from "vitest";
import { signOut } from "./sign-out";

describe("signOut", () => {
  it("ends the auth session first, then hands over to Zitadel", async () => {
    const order: string[] = [];
    const logout = vi.fn(async () => void order.push("auth"));
    const redirect = vi.fn(async () => void order.push("zitadel"));
    await signOut({ logout }, redirect);
    expect(order).toEqual(["auth", "zitadel"]);
  });

  it("still signs out of Zitadel when auth is unreachable", async () => {
    const redirect = vi.fn(async () => undefined);
    await signOut({ logout: vi.fn().mockRejectedValue(new Error("down")) }, redirect);
    expect(redirect).toHaveBeenCalled();
  });
});
