import { describe, expect, it, vi } from "vitest";
import { ApiError } from "@chelabase/management-client";
import { configureLoginClient, customFetch } from "./fetch";

describe("login customFetch", () => {
  it("login_sends_cookies_and_no_bearer", async () => {
    const f = vi.fn(async () => new Response('{"next":"done"}', { status: 200 }));
    configureLoginClient({ baseUrl: "https://gw.test", fetchImpl: f as unknown as typeof fetch });
    const out = await customFetch<{ next: string }>("/login/sign-in", { method: "POST", body: "{}" });
    expect(out).toEqual({ next: "done" });
    const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://gw.test/login/sign-in");
    expect(init.credentials).toBe("include");
    const h = new Headers(init.headers);
    expect(h.get("authorization")).toBeNull();
    expect(h.get("content-type")).toBe("application/json");
  });

  it("login_error_body_becomes_api_error", async () => {
    const body = '{"error":{"code":"invalid_credentials","message":"Wrong email or password","attempts":2}}';
    configureLoginClient({ baseUrl: "https://gw.test", fetchImpl: (async () => new Response(body, { status: 401 })) as typeof fetch });
    const e = await customFetch("/login/sign-in", { method: "POST" }).catch((x) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 401, code: "invalid_credentials", signInRestarted: false });
    expect((e as ApiError).extra).toEqual({ attempts: 2 });
  });
});
