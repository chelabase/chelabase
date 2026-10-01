import { describe, expect, it, vi } from "vitest";
import { configureLoginClient } from "./login-client/fetch";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { signOut } from "./sign-out";

describe("signOut", () => {
  it("ends the auth session first, then hands over to Zitadel", async () => {
    const order: string[] = [];
    const f = fakeFetch({ "POST /auth/v1/logout": () => (order.push("auth"), jsonResponse({})) });
    configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
    const redirect = vi.fn(async () => void order.push("zitadel"));
    await signOut(redirect);
    expect(order).toEqual(["auth", "zitadel"]);
    expect(f.calls[0]!.credentials).toBe("include");
  });

  it("still signs out of Zitadel when auth is unreachable", async () => {
    const f = fakeFetch({ "POST /auth/v1/logout": () => { throw new TypeError("down"); } });
    configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
    const redirect = vi.fn(async () => undefined);
    await signOut(redirect);
    expect(redirect).toHaveBeenCalled();
  });
});
