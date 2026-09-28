import { describe, expect, it, vi } from "vitest";
import { signOut } from "./sign-out";

describe("signOut", () => {
  it("ends the ui-auth session first, then hands over to Zitadel", async () => {
    const order: string[] = [];
    const logout = vi.fn(async () => void order.push("ui-auth"));
    const redirect = vi.fn(async () => void order.push("zitadel"));
    await signOut({ logout }, redirect);
    expect(order).toEqual(["ui-auth", "zitadel"]);
  });

  it("still signs out of Zitadel when ui-auth is unreachable", async () => {
    const redirect = vi.fn(async () => undefined);
    await signOut({ logout: vi.fn().mockRejectedValue(new Error("down")) }, redirect);
    expect(redirect).toHaveBeenCalled();
  });
});
