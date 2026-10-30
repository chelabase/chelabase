import { afterEach, describe, expect, test, vi } from "vitest";
import { createClient, DataApiError } from "./index.ts";
import { fakeFetch } from "./test-fetch.ts";

const url = "http://localhost:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

const userHeader = (init: RequestInit) => new Headers(init.headers).get("chelabase-user");

afterEach(() => vi.unstubAllGlobals());

describe("accessToken callback", () => {
  test("sends_callback_token_as_chelabase_user", async () => {
    const fetch = fakeFetch("[]");
    const db = createClient({ url, key, fetch, accessToken: () => "t1" });
    await db.from("t").select("id");
    const headers = new Headers(fetch.calls[0]!.init.headers);
    expect(headers.get("chelabase-user")).toBe("t1");
    expect(headers.get("authorization")).toBe(`Bearer ${key}`);
  });

  test("sends_no_user_header_when_callback_returns_undefined", async () => {
    const fetch = fakeFetch("[]");
    const db = createClient({ url, key, fetch, accessToken: async () => undefined });
    await db.from("t").select("id");
    expect(userHeader(fetch.calls[0]!.init)).toBeNull();
  });

  test("calls_callback_once_per_request", async () => {
    const fetch = fakeFetch("[]");
    const tokens = ["a", "b"];
    const db = createClient({ url, key, fetch, accessToken: () => tokens.shift() });
    await db.from("t").select("id");
    await db.from("t").select("id");
    expect(fetch.calls.map((c) => userHeader(c.init))).toEqual(["a", "b"]);
  });

  test("callback_failure_rejects_without_fetch", async () => {
    const boom = new Error("boom");
    for (const accessToken of [
      () => {
        throw boom;
      },
      () => Promise.reject(boom),
    ]) {
      const fetch = fakeFetch("[]");
      const db = createClient({ url, key, fetch, accessToken });
      await expect(db.from("t").select("id")).rejects.toBe(boom);
      expect(fetch.calls).toHaveLength(0);
    }
  });

  test("user_token_expired_is_not_retried", async () => {
    const fetch = fakeFetch(JSON.stringify({ error: { code: "user_token_expired", message: "expired" } }), 401);
    const db = createClient({ url, key, fetch, accessToken: () => "t1" });
    await expect(db.from("t").select("id")).rejects.toBeInstanceOf(DataApiError);
    expect(fetch.calls).toHaveLength(1);
  });

  test("auth_methods_reject_naming_the_option", async () => {
    const fetch = fakeFetch("{}");
    const db = createClient({ url, key, fetch, accessToken: () => "t1" });
    const a = db.auth;
    const calls = [
      () => a.signUp({ email: "a@b.co", password: "pw" }),
      () => a.signIn({ email: "a@b.co", password: "pw" }),
      () => a.signOut(),
      () => a.refresh(),
      () => a.getUser(),
      () => a.currentUser(),
      () => a.refreshIfNeeded(),
      () => a.verifyEmail({ email: "a@b.co", code: "1" }),
      () => a.resendCode({ email: "a@b.co" }),
      () => a.requestPasswordReset({ email: "a@b.co" }),
      () => a.confirmPasswordReset({ email: "a@b.co", code: "1", password: "pw" }),
      () => a.changeEmail({ email: "a@b.co" }),
      () => a.changePassword({ currentPassword: "pw", password: "pw2" }),
    ];
    for (const c of calls) await expect(c()).rejects.toThrow(/accessToken/);
    expect(() => a.onChange(() => {})).toThrow(/accessToken/);
    expect(fetch.calls).toHaveLength(0);
  });

  test("never_touches_local_storage", async () => {
    const touched: string[] = [];
    const storage = new Proxy({}, { get: (_t, p) => (touched.push(String(p)), () => null) });
    vi.stubGlobal("localStorage", storage);
    const fetch = fakeFetch("[]");
    const db = createClient({ url, key, fetch, accessToken: () => "t1" });
    await db.from("t").select("id");
    expect(touched).toEqual([]);
  });

  test("database_sibling_uses_the_same_callback", async () => {
    const fetch = fakeFetch("[]");
    const db = createClient({ url, key, fetch, accessToken: () => "t1" });
    await db.database("other").from("t").select("id");
    expect(userHeader(fetch.calls[0]!.init)).toBe("t1");
  });
});
