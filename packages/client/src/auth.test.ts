import { afterEach, describe, expect, test, vi } from "vitest";
import { createClient, DataApiError } from "./index.ts";
import type { AuthEvent, AuthStorage } from "./index.ts";
import type { Recorded } from "./test-fetch.ts";

const url = "http://localhost:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const STORE_KEY = "chelabase.chk_pub_0123.refresh";

const user = { id: "u1", email: "a@b.co", email_verified: true, created_at: "2026-10-03T00:00:00Z" };

function jwt(exp: number, n: number): string {
  const b64 = (o: unknown) => btoa(JSON.stringify(o)).replace(/=+$/, "").replace(/\+/g, "-").replace(/\//g, "_");
  return `${b64({ alg: "EdDSA" })}.${b64({ exp, sub: "u1", n })}.sig`;
}
const nowSecs = () => Math.floor(Date.now() / 1000);

function session(n: number, ttl = 900) {
  return {
    access_token: jwt(nowSecs() + ttl, n),
    token_type: "bearer",
    expires_in: ttl,
    refresh_token: `rt${n}`,
    user,
  };
}

type Reply = { status?: number; body?: unknown };
type Handler = (call: Recorded) => Reply | Promise<Reply>;

/** A fake fetch that routes by path; each handler may be a function so replies can vary per call. */
function router(routes: Record<string, Handler>) {
  const calls: Recorded[] = [];
  const f = async (input: string | URL | Request, init?: RequestInit) => {
    const call = { url: String(input), init: init ?? {} };
    calls.push(call);
    const path = new URL(call.url).pathname;
    const h = routes[path];
    if (!h) return new Response(JSON.stringify({ error: { code: "not_found", message: path } }), { status: 404 });
    const r = await h(call);
    if (r.status === 204) return new Response(null, { status: 204 });
    return new Response(JSON.stringify(r.body ?? null), {
      status: r.status ?? 200,
      headers: { "content-type": "application/json" },
    });
  };
  return Object.assign(f as typeof fetch, {
    calls,
    to: (path: string) => calls.filter((c) => new URL(c.url).pathname === path),
  });
}
const hdr = (c: Recorded, name: string) => new Headers(c.init.headers).get(name);
const err = (status: number, code: string): Reply => ({ status, body: { error: { code, message: code } } });

function memoryStorage(initial: Record<string, string> = {}) {
  const m = new Map(Object.entries(initial));
  const s: AuthStorage = {
    get: (k) => m.get(k) ?? null,
    set: (k, v) => void m.set(k, v),
    remove: (k) => void m.delete(k),
  };
  return { s, m };
}

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("auth", () => {
  test("sign_in_stores_refresh_and_sends_user_header", async () => {
    const sess = session(1);
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: sess }),
      "/data/v1/query": () => ({ body: [] }),
    });
    const { s, m } = memoryStorage();
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    const got = await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(got).toEqual(sess);
    expect(m.get(STORE_KEY)).toBe("rt1");
    expect([...m.values()]).not.toContain(sess.access_token);
    const signIn = fetch.to("/auth/v1/app/sign-in")[0]!;
    expect(hdr(signIn, "chelabase-user")).toBeNull();
    expect(JSON.parse(signIn.init.body as string)).toEqual({ email: "a@b.co", password: "pw" });
    await db.from("t").select("id");
    const q = fetch.to("/data/v1/query")[0]!;
    expect(hdr(q, "chelabase-user")).toBe(sess.access_token);
    expect(hdr(q, "authorization")).toBe(`Bearer ${key}`);
  });

  test("sign_up_sends_data_and_no_user_header", async () => {
    const sess = session(1);
    const fetch = router({ "/auth/v1/app/sign-up": () => ({ status: 201, body: sess }) });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s, autoRefresh: false } });
    const r = await db.auth.signUp({ email: "a@b.co", password: "pw", data: { nick: "x" } });
    expect(r).toEqual({ user, session: sess, verificationRequired: false });
    const c = fetch.calls[0]!;
    expect(hdr(c, "chelabase-user")).toBeNull();
    expect(JSON.parse(c.init.body as string)).toEqual({ email: "a@b.co", password: "pw", data: { nick: "x" } });
  });

  test("sign_up_pending_has_no_session", async () => {
    const fetch = router({ "/auth/v1/app/sign-up": () => ({ status: 201, body: { user, verification_required: true } }) });
    const { s, m } = memoryStorage();
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    const r = await db.auth.signUp({ email: "a@b.co", password: "pw" });
    expect(r).toEqual({ user, session: null, verificationRequired: true });
    expect(m.size).toBe(0);
  });

  test("concurrent_refreshes_share_one_request", async () => {
    const fetch = router({
      "/auth/v1/app/refresh": async () => {
        await new Promise((r) => setTimeout(r, 10));
        return { body: session(2) };
      },
    });
    const { s, m } = memoryStorage({ [STORE_KEY]: "rt1" });
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    const [a, b] = await Promise.all([db.auth.refresh(), db.auth.refresh()]);
    expect(a).toEqual(b);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
    expect(JSON.parse(fetch.calls[0]!.init.body as string)).toEqual({ refresh_token: "rt1" });
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBeNull();
    expect(m.get(STORE_KEY)).toBe("rt2");
  });

  test("startup_refreshes_lazily_before_first_request", async () => {
    const sess = session(2);
    const fetch = router({
      "/auth/v1/app/refresh": () => ({ body: sess }),
      "/data/v1/query": () => ({ body: [] }),
    });
    const db = createClient({
      url,
      key,
      fetch,
      auth: { storage: memoryStorage({ [STORE_KEY]: "rt1" }).s, autoRefresh: false },
    });
    expect(fetch.calls).toHaveLength(0);
    await db.from("t").select("id");
    expect(fetch.calls.map((c) => new URL(c.url).pathname)).toEqual(["/auth/v1/app/refresh", "/data/v1/query"]);
    expect(hdr(fetch.calls[1]!, "chelabase-user")).toBe(sess.access_token);
  });

  test("expired_access_refreshes_once_and_retries", async () => {
    const s1 = session(1);
    const s2 = session(2);
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: s1 }),
      "/auth/v1/app/refresh": () => ({ body: s2 }),
      "/data/v1/query": (c) =>
        hdr(c, "chelabase-user") === s1.access_token ? err(401, "user_token_expired") : { body: [{ id: 1 }] },
    });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s, autoRefresh: false } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(await db.from("t").select("id")).toEqual([{ id: 1 }]);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
    const q = fetch.to("/data/v1/query");
    expect(q).toHaveLength(2);
    expect(hdr(q[1]!, "chelabase-user")).toBe(s2.access_token);
  });

  test("expired_twice_throws_after_one_retry", async () => {
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/auth/v1/app/refresh": () => ({ body: session(2) }),
      "/data/v1/query": () => err(401, "user_token_expired"),
    });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s, autoRefresh: false } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await expect(db.from("t").select("id")).rejects.toMatchObject({ code: "user_token_expired" });
    expect(fetch.to("/data/v1/query")).toHaveLength(2);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
  });

  test("reused_signs_out_and_emits", async () => {
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/auth/v1/app/refresh": () => err(401, "refresh_token_reused"),
    });
    const { s, m } = memoryStorage();
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    const events: [AuthEvent, unknown][] = [];
    db.auth.onChange((e, u) => events.push([e, u]));
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    const e = await db.auth.refresh().catch((x: unknown) => x);
    expect(e).toBeInstanceOf(DataApiError);
    expect(e).toMatchObject({ status: 401, code: "refresh_token_reused" });
    expect(m.size).toBe(0);
    expect(events).toEqual([
      ["signed_in", user],
      ["signed_out", null],
    ]);
  });

  test("data_call_session_revoked_signs_out_locally", async () => {
    const s1 = session(1);
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: s1 }),
      "/data/v1/query": (c) => (hdr(c, "chelabase-user") === null ? { body: [] } : err(401, "user_session_revoked")),
    });
    const { s, m } = memoryStorage();
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    const events: [AuthEvent, unknown][] = [];
    db.auth.onChange((e, u) => events.push([e, u]));
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await expect(db.from("t").select("id")).rejects.toMatchObject({ status: 401, code: "user_session_revoked" });
    expect(m.size).toBe(0);
    expect(events).toEqual([
      ["signed_in", user],
      ["signed_out", null],
    ]);
    expect(await db.from("t").select("id")).toEqual([]);
    const q = fetch.to("/data/v1/query");
    expect(q).toHaveLength(2);
    expect(hdr(q[1]!, "chelabase-user")).toBeNull();
  });

  test("user_call_session_revoked_signs_out_locally", async () => {
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/auth/v1/app/user": () => err(401, "user_session_revoked"),
    });
    const { s, m } = memoryStorage();
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    const events: AuthEvent[] = [];
    db.auth.onChange((e) => events.push(e));
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await expect(db.auth.getUser()).rejects.toMatchObject({ code: "user_session_revoked" });
    expect(m.size).toBe(0);
    expect(events).toEqual(["signed_in", "signed_out"]);
    await expect(db.auth.getUser()).rejects.toMatchObject({ code: "user_token_invalid" });
  });

  test("sign_out_of_a_revoked_session_emits_signed_out_once", async () => {
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/auth/v1/app/sign-out": () => err(401, "user_session_revoked"),
    });
    const { s, m } = memoryStorage();
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    const events: AuthEvent[] = [];
    db.auth.onChange((e) => events.push(e));
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await expect(db.auth.signOut()).rejects.toMatchObject({ code: "user_session_revoked" });
    expect(m.size).toBe(0);
    expect(events).toEqual(["signed_in", "signed_out"]);
  });

  test("refresh_user_inactive_signs_out_locally", async () => {
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/auth/v1/app/refresh": () => err(403, "user_inactive"),
      "/data/v1/query": () => ({ body: [] }),
    });
    const { s, m } = memoryStorage();
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    const events: AuthEvent[] = [];
    db.auth.onChange((e) => events.push(e));
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await expect(db.auth.refresh()).rejects.toMatchObject({ status: 403, code: "user_inactive" });
    expect(m.size).toBe(0);
    expect(events).toEqual(["signed_in", "signed_out"]);
    await db.from("t").select("id");
    expect(hdr(fetch.to("/data/v1/query")[0]!, "chelabase-user")).toBeNull();
  });

  test("auto_refresh_60s_before_exp", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-03T00:00:00Z"));
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1, 300) }),
      "/auth/v1/app/refresh": () => ({ body: session(2, 300) }),
    });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s } });
    const events: AuthEvent[] = [];
    db.auth.onChange((e) => events.push(e));
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await vi.advanceTimersByTimeAsync(238_000);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(3_000);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
    expect(events).toEqual(["signed_in", "refreshed"]);
    // and it re-arms for the new token
    await vi.advanceTimersByTimeAsync(240_000);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(2);
  });

  test("auto_refresh_of_a_60s_token_waits_half_its_life", async () => {
    // The lead is capped at half the lifetime: a 60 s token refreshes every ~30 s, not in a loop.
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-03T00:00:00Z"));
    let n = 1;
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1, 60) }),
      "/auth/v1/app/refresh": () => ({ body: session(++n, 60) }),
    });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await vi.advanceTimersByTimeAsync(29_000);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(28_000);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(2);
  });

  test("auto_refresh_of_a_15min_token_is_60s_before_exp", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-03T00:00:00Z"));
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1, 900) }),
      "/auth/v1/app/refresh": () => ({ body: session(2, 900) }),
    });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await vi.advanceTimersByTimeAsync(839_000);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
  });

  test("memory_storage_without_localStorage", async () => {
    vi.stubGlobal("localStorage", undefined);
    const fetch = router({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const db = createClient({ url, key, fetch, auth: { autoRefresh: false } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(await db.auth.getUser().catch(() => "x")).toBeDefined();
    const db2 = createClient({ url, key, fetch: router({}), auth: { autoRefresh: false } });
    // a second client has its own memory: nothing to refresh
    await expect(db2.auth.refresh()).rejects.toMatchObject({ code: "refresh_token_invalid" });
  });

  test("default_storage_is_localStorage_when_present", async () => {
    const store = new Map<string, string>();
    vi.stubGlobal("localStorage", {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
      removeItem: (k: string) => void store.delete(k),
    });
    const fetch = router({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const db = createClient({ url, key, fetch, auth: { autoRefresh: false } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(store.get(STORE_KEY)).toBe("rt1");
  });

  test("async_storage_adapter", async () => {
    const m = new Map<string, string>([[STORE_KEY, "rt1"]]);
    const s: AuthStorage = {
      get: async (k) => m.get(k) ?? null,
      set: async (k, v) => void m.set(k, v),
      remove: async (k) => void m.delete(k),
    };
    const fetch = router({
      "/auth/v1/app/refresh": () => ({ body: session(2) }),
      "/auth/v1/app/sign-out": () => ({ status: 204 }),
    });
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    await db.auth.refresh();
    expect(m.get(STORE_KEY)).toBe("rt2");
    await db.auth.signOut();
    expect(m.size).toBe(0);
  });

  test("on_change_unsubscribe", async () => {
    const fetch = router({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s, autoRefresh: false } });
    const seen: AuthEvent[] = [];
    const off = db.auth.onChange((e) => seen.push(e));
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    off();
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(seen).toEqual(["signed_in"]);
  });

  test("change_password_swaps_tokens", async () => {
    const s1 = session(1);
    const s2 = session(2);
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: s1 }),
      "/auth/v1/app/change-password": () => ({ status: 200, body: s2 }),
      "/data/v1/query": () => ({ body: [] }),
    });
    const { s, m } = memoryStorage();
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    const got = await db.auth.changePassword({ currentPassword: "pw", password: "pw2" });
    expect(got).toEqual(s2);
    const c = fetch.to("/auth/v1/app/change-password")[0]!;
    expect(hdr(c, "chelabase-user")).toBe(s1.access_token);
    expect(JSON.parse(c.init.body as string)).toEqual({ current_password: "pw", password: "pw2" });
    expect(m.get(STORE_KEY)).toBe("rt2");
    await db.from("t").select("id");
    expect(hdr(fetch.to("/data/v1/query")[0]!, "chelabase-user")).toBe(s2.access_token);
  });

  test("user_scoped_calls_send_header_and_public_calls_do_not", async () => {
    const s1 = session(1);
    const ok = () => ({ status: 204 });
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: s1 }),
      "/auth/v1/app/user": () => ({ body: user }),
      "/auth/v1/app/change-email": ok,
      "/auth/v1/app/sign-out": ok,
      "/auth/v1/app/verify-email": ok,
      "/auth/v1/app/resend-code": ok,
      "/auth/v1/app/password-reset": ok,
      "/auth/v1/app/password-reset/confirm": ok,
    });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s, autoRefresh: false } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await db.auth.verifyEmail({ email: "a@b.co", code: "1" });
    await db.auth.resendCode({ email: "a@b.co" });
    await db.auth.requestPasswordReset({ email: "a@b.co" });
    await db.auth.confirmPasswordReset({ email: "a@b.co", code: "1", password: "x" });
    for (const c of fetch.calls.slice(1)) expect(hdr(c, "chelabase-user")).toBeNull();
    expect(await db.auth.getUser()).toEqual(user);
    expect(fetch.to("/auth/v1/app/user")[0]!.init.method).toBe("GET");
    await db.auth.changeEmail({ email: "n@b.co" });
    await db.auth.signOut();
    for (const p of ["user", "change-email", "sign-out"]) {
      expect(hdr(fetch.to(`/auth/v1/app/${p}`)[0]!, "chelabase-user")).toBe(s1.access_token);
    }
  });

  test("request_bodies_are_snake_case_exactly", async () => {
    const ok = () => ({ status: 204 });
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/auth/v1/app/verify-email": ok,
      "/auth/v1/app/resend-code": ok,
      "/auth/v1/app/password-reset": ok,
      "/auth/v1/app/password-reset/confirm": ok,
      "/auth/v1/app/change-email": ok,
      "/auth/v1/app/sign-out": ok,
    });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s, autoRefresh: false } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await db.auth.verifyEmail({ email: "a@b.co", code: "1" });
    await db.auth.resendCode({ email: "a@b.co" });
    await db.auth.requestPasswordReset({ email: "a@b.co" });
    await db.auth.confirmPasswordReset({ email: "a@b.co", code: "1", password: "x" });
    await db.auth.changeEmail({ email: "n@b.co" });
    const body = (p: string) => JSON.parse(fetch.to(`/auth/v1/app/${p}`)[0]!.init.body as string);
    expect(body("sign-in")).toEqual({ email: "a@b.co", password: "pw" });
    expect(body("verify-email")).toEqual({ email: "a@b.co", code: "1" });
    expect(body("resend-code")).toEqual({ email: "a@b.co" });
    expect(body("password-reset")).toEqual({ email: "a@b.co" });
    expect(body("password-reset/confirm")).toEqual({ email: "a@b.co", code: "1", password: "x" });
    expect(body("change-email")).toEqual({ email: "n@b.co" });
  });

  test("sign_out_without_access_token_sends_refresh_token_body", async () => {
    const fetch = router({ "/auth/v1/app/sign-out": () => ({ status: 204 }) });
    const { s, m } = memoryStorage({ [STORE_KEY]: "rt9" });
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    await db.auth.signOut();
    expect(JSON.parse(fetch.calls[0]!.init.body as string)).toEqual({ refresh_token: "rt9" });
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBeNull();
    expect(m.size).toBe(0);
  });
});

describe("auth refreshIfNeeded and currentUser", () => {
  test("refresh_if_needed_only_refreshes_when_needed", async () => {
    const s2 = session(2);
    const fetch = router({ "/auth/v1/app/refresh": () => ({ body: s2 }) });
    const { s, m } = memoryStorage({ [STORE_KEY]: "rt1" });
    const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
    await expect(db.auth.refreshIfNeeded()).resolves.toBeUndefined();
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
    expect(m.get(STORE_KEY)).toBe("rt2");
    await db.auth.refreshIfNeeded();
    expect(fetch.calls).toHaveLength(1);
    // No stored session: nothing to do.
    const none = router({});
    await createClient({ url, key, fetch: none, auth: { storage: memoryStorage().s } }).auth.refreshIfNeeded();
    expect(none.calls).toHaveLength(0);
  });

  test("current_user_is_null_when_signed_out", async () => {
    const none = router({});
    expect(await createClient({ url, key, fetch: none, auth: { storage: memoryStorage().s } }).auth.currentUser()).toBeNull();
    expect(none.calls).toHaveLength(0);

    for (const code of ["user_session_revoked", "user_token_invalid"]) {
      const fetch = router({ "/auth/v1/app/sign-in": () => ({ body: session(1) }), "/auth/v1/app/user": () => err(401, code) });
      const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s, autoRefresh: false } });
      await db.auth.signIn({ email: "a@b.co", password: "pw" });
      expect(await db.auth.currentUser(), code).toBeNull();
    }
    // A stored refresh token the server no longer knows.
    const stale = router({ "/auth/v1/app/refresh": () => err(401, "refresh_token_invalid") });
    const db = createClient({ url, key, fetch: stale, auth: { storage: memoryStorage({ [STORE_KEY]: "rt1" }).s } });
    expect(await db.auth.currentUser()).toBeNull();
  });

  test("current_user_is_null_when_its_refresh_ends_the_session", async () => {
    for (const code of ["refresh_token_reused", "user_inactive"]) {
      const fetch = router({ "/auth/v1/app/refresh": () => err(401, code) });
      const { s, m } = memoryStorage({ [STORE_KEY]: "rt1" });
      const db = createClient({ url, key, fetch, auth: { storage: s, autoRefresh: false } });
      expect(await db.auth.currentUser(), code).toBeNull();
      expect(m.has(STORE_KEY), code).toBe(false);
    }
  });

  test("current_user_retries_once_after_expired_then_gives_null", async () => {
    let refreshes = 0;
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/auth/v1/app/refresh": () => ({ body: session(10 + refreshes++) }),
      "/auth/v1/app/user": () => err(401, "user_token_expired"),
    });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s, autoRefresh: false } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(await db.auth.currentUser()).toBeNull();
    expect(refreshes).toBe(1);
    expect(fetch.to("/auth/v1/app/user")).toHaveLength(2);
  });

  test("current_user_rejects_on_a_503", async () => {
    const fetch = router({ "/auth/v1/app/sign-in": () => ({ body: session(1) }), "/auth/v1/app/user": () => err(503, "unavailable") });
    const db = createClient({ url, key, fetch, auth: { storage: memoryStorage().s, autoRefresh: false } });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await expect(db.auth.currentUser()).rejects.toBeInstanceOf(DataApiError);
  });
});
