import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";
import type { CookieToSet } from "./cookies.ts";
import { DataApiError } from "./index.ts";
import * as serverModule from "./server.ts";
import { createServerClient } from "./server.ts";
import type { Recorded } from "./test-fetch.ts";

const url = "http://localhost:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const serverKey = "chk_sec_zyxwvutsrqponmlkjihgfedcbaZYXWVUTSRQPONMLKJIH";
const ACCESS = "__Host-chelabase.chk_pub_0123.access";
const REFRESH = "__Host-chelabase.chk_pub_0123.refresh";

const user = { id: "u1", email: "a@b.co", email_verified: true, created_at: "2026-10-03T00:00:00Z" };
const nowSecs = () => Math.floor(Date.now() / 1000);

function jwt(exp: number, n: number | string, sub = "u1"): string {
  const b64 = (o: unknown) => btoa(JSON.stringify(o)).replace(/=+$/, "").replace(/\+/g, "-").replace(/\//g, "_");
  return `${b64({ alg: "EdDSA" })}.${b64({ exp, sub, n })}.sig`;
}

function session(n: number, ttl = 900) {
  return { access_token: jwt(nowSecs() + ttl, n), token_type: "bearer", expires_in: ttl, refresh_token: `rt${n}`, user };
}

type Reply = { status?: number; body?: unknown };
type Handler = (call: Recorded) => Reply | Promise<Reply>;

/** A fake fetch that routes by path, logging each request's path to `log`. */
function router(routes: Record<string, Handler>, log: string[] = []) {
  const calls: Recorded[] = [];
  const f = async (input: string | URL | Request, init?: RequestInit) => {
    const call = { url: String(input), init: init ?? {} };
    calls.push(call);
    const path = new URL(call.url).pathname;
    log.push(path);
    const h = routes[path];
    if (!h) return new Response(JSON.stringify({ error: { code: "not_found", message: path } }), { status: 404 });
    const r = await h(call);
    if (r.status === 204) return new Response(null, { status: 204 });
    return new Response(JSON.stringify(r.body ?? null), { status: r.status ?? 200 });
  };
  return Object.assign(f as typeof fetch, {
    calls,
    to: (path: string) => calls.filter((c) => new URL(c.url).pathname === path),
  });
}
const hdr = (c: Recorded, name: string) => new Headers(c.init.headers).get(name);
const err = (status: number, code: string): Reply => ({ status, body: { error: { code, message: code } } });
const body = (c: Recorded) => JSON.parse(c.init.body as string) as unknown;

/** A request's cookies: `setAll` updates what `getAll` returns, as the adapter contract asks. */
function jar(initial: Record<string, string> = {}, log: string[] = []) {
  const m = new Map(Object.entries(initial));
  const writes: CookieToSet[][] = [];
  const getAll = () => [...m].map(([name, value]) => ({ name, value }));
  const setAll = (list: CookieToSet[]) => {
    log.push("setAll");
    writes.push(list);
    for (const c of list) {
      if (c.options.maxAge === 0) m.delete(c.name);
      else m.set(c.name, c.value);
    }
  };
  return { m, writes, cookies: { getAll, setAll }, readOnly: { getAll } };
}

const stale = () => jwt(nowSecs() + 3, "old");

describe("createServerClient", () => {
  test("fresh_access_cookie_is_used_without_refresh", async () => {
    const access = jwt(nowSecs() + 60, 1);
    const fetch = router({ "/data/v1/query": () => ({ body: [] }) });
    const j = jar({ [ACCESS]: access, [REFRESH]: "rt1" });
    const db = createServerClient({ url, key, fetch, cookies: j.cookies });
    await db.from("t").select("id");
    expect(fetch.calls).toHaveLength(1);
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBe(access);
    expect(hdr(fetch.calls[0]!, "authorization")).toBe(`Bearer ${key}`);
    expect(j.writes).toEqual([]);
  });

  test("stale_access_refreshes_once_and_writes_cookies_before_queries", async () => {
    for (const access of [undefined, "not-a-jwt", stale()]) {
      const log: string[] = [];
      const s2 = session(2);
      const fetch = router(
        {
          "/auth/v1/app/refresh": async () => {
            await new Promise((r) => setTimeout(r, 5));
            return { body: s2 };
          },
          "/data/v1/query": () => ({ body: [] }),
        },
        log,
      );
      const j = jar({ ...(access === undefined ? {} : { [ACCESS]: access }), [REFRESH]: "rt1" }, log);
      const db = createServerClient({ url, key, fetch, cookies: j.cookies });
      await Promise.all([db.from("t").select("id"), db.from("u").select("id")]);
      expect(log).toEqual(["/auth/v1/app/refresh", "setAll", "/data/v1/query", "/data/v1/query"]);
      expect(body(fetch.calls[0]!)).toEqual({ refresh_token: "rt1" });
      expect(j.writes[0]!.map((c) => [c.name, c.value])).toEqual([
        [ACCESS, s2.access_token],
        [REFRESH, "rt2"],
      ]);
      for (const q of fetch.to("/data/v1/query")) expect(hdr(q, "chelabase-user")).toBe(s2.access_token);
    }
  });

  test("without_set_all_no_refresh_and_the_query_is_anonymous", async () => {
    const fetch = router({ "/data/v1/query": () => ({ body: [] }) });
    const j = jar({ [ACCESS]: stale(), [REFRESH]: "rt1" });
    const db = createServerClient({ url, key, fetch, cookies: j.readOnly });
    await db.from("t").select("id");
    expect(fetch.calls.map((c) => new URL(c.url).pathname)).toEqual(["/data/v1/query"]);
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBeNull();
  });

  test("without_set_all_an_expired_answer_retries_anonymous", async () => {
    const access = jwt(nowSecs() + 60, 1);
    const fetch = router({
      "/data/v1/query": (c) => (hdr(c, "chelabase-user") ? err(401, "user_token_expired") : { body: [] }),
    });
    const db = createServerClient({ url, key, fetch, cookies: jar({ [ACCESS]: access, [REFRESH]: "rt1" }).readOnly });
    await db.from("t").select("id");
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(0);
    expect(fetch.calls.map((c) => hdr(c, "chelabase-user"))).toEqual([access, null]);
  });

  test("expired_answer_refreshes_writes_and_retries", async () => {
    const access = jwt(nowSecs() + 60, 1);
    const s2 = session(2);
    const log: string[] = [];
    const fetch = router(
      {
        "/auth/v1/app/refresh": () => ({ body: s2 }),
        "/data/v1/query": (c) => (hdr(c, "chelabase-user") === access ? err(401, "user_token_expired") : { body: [] }),
      },
      log,
    );
    const j = jar({ [ACCESS]: access, [REFRESH]: "rt1" }, log);
    await createServerClient({ url, key, fetch, cookies: j.cookies }).from("t").select("id");
    expect(log).toEqual(["/data/v1/query", "/auth/v1/app/refresh", "setAll", "/data/v1/query"]);
    expect(hdr(fetch.calls[2]!, "chelabase-user")).toBe(s2.access_token);
  });

  test("throwing_set_all_rejects_and_sends_no_data_request", async () => {
    const boom = new Error("cookies are read-only here");
    for (const setAll of [
      () => {
        throw boom;
      },
      () => Promise.reject(boom),
    ]) {
      const fetch = router({ "/auth/v1/app/refresh": () => ({ body: session(2) }), "/data/v1/query": () => ({ body: [] }) });
      const cookies = { getAll: jar({ [REFRESH]: "rt1" }).cookies.getAll, setAll };
      const db = createServerClient({ url, key, fetch, cookies });
      await expect(db.from("t").select("id")).rejects.toBe(boom);
      expect(fetch.to("/data/v1/query")).toHaveLength(0);
      // The token the browser never got isn't used later either.
      await expect(db.from("t").select("id")).rejects.toBe(boom);
      expect(fetch.to("/data/v1/query")).toHaveLength(0);
    }
  });

  test("reused_or_inactive_clears_both_cookies_and_throws", async () => {
    for (const code of ["refresh_token_reused", "user_inactive"]) {
      const fetch = router({ "/auth/v1/app/refresh": () => err(401, code), "/data/v1/query": () => ({ body: [] }) });
      const j = jar({ [ACCESS]: stale(), [REFRESH]: "rt1" });
      const db = createServerClient({ url, key, fetch, cookies: j.cookies });
      const e = await db.from("t").select("id").catch((x: unknown) => x);
      expect(e).toBeInstanceOf(DataApiError);
      expect(e).toMatchObject({ code });
      expect(j.writes).toHaveLength(1);
      expect(j.writes[0]!.map((c) => [c.name, c.value, c.options.maxAge])).toEqual([
        [ACCESS, "", 0],
        [REFRESH, "", 0],
      ]);
      expect(fetch.to("/data/v1/query")).toHaveLength(0);
    }
  });

  test("invalid_refresh_leaves_cookies_and_goes_anonymous", async () => {
    const fetch = router({
      "/auth/v1/app/refresh": () => err(401, "refresh_token_invalid"),
      "/data/v1/query": () => ({ body: [] }),
    });
    const j = jar({ [ACCESS]: stale(), [REFRESH]: "rt1" });
    const db = createServerClient({ url, key, fetch, cookies: j.cookies });
    await db.from("t").select("id");
    await db.from("t").select("id");
    expect(j.writes).toEqual([]);
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
    for (const q of fetch.to("/data/v1/query")) expect(hdr(q, "chelabase-user")).toBeNull();
    await expect(db.auth.getUser()).rejects.toMatchObject({ code: "user_token_invalid" });
  });

  test("invalid_refresh_clears_cookies_whose_access_token_is_long_dead", async () => {
    // Missing, unreadable, or more than 5 min past its `exp`: cleared, so later pages don't refresh again.
    for (const access of [undefined, "not-a-jwt", jwt(nowSecs() - 400, 0)]) {
      const fetch = router({
        "/auth/v1/app/refresh": () => err(401, "refresh_token_invalid"),
        "/data/v1/query": () => ({ body: [] }),
      });
      const j = jar({ ...(access === undefined ? {} : { [ACCESS]: access }), [REFRESH]: "rt1" });
      await createServerClient({ url, key, fetch, cookies: j.cookies }).from("t").select("id");
      expect(j.writes.map((w) => w.map((c) => [c.name, c.value, c.options.maxAge])), String(access)).toEqual([
        [
          [ACCESS, "", 0],
          [REFRESH, "", 0],
        ],
      ]);
      expect(hdr(fetch.to("/data/v1/query")[0]!, "chelabase-user")).toBeNull();
      // The next page view has nothing to refresh.
      await createServerClient({ url, key, fetch, cookies: j.cookies }).from("t").select("id");
      expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
    }
  });

  test("invalid_refresh_keeps_cookies_whose_access_token_expired_recently", async () => {
    // Within 5 min of its `exp` (clock skew between the app server and auth): kept.
    for (const ago of [30, 240]) {
      const fetch = router({
        "/auth/v1/app/refresh": () => err(401, "refresh_token_invalid"),
        "/data/v1/query": () => ({ body: [] }),
      });
      const j = jar({ [ACCESS]: jwt(nowSecs() - ago, 0), [REFRESH]: "rt1" });
      await createServerClient({ url, key, fetch, cookies: j.cookies }).from("t").select("id");
      expect(j.writes, String(ago)).toEqual([]);
      expect(hdr(fetch.to("/data/v1/query")[0]!, "chelabase-user")).toBeNull();
    }
  });

  test("late_invalid_after_a_rotation_keeps_the_newer_cookies", async () => {
    const s2 = session(2);
    let refreshes = 0;
    const fetch = router({
      // The first exchange of rt1 rotates; a later one (past the grace window) is stale.
      "/auth/v1/app/refresh": () => (refreshes++ === 0 ? { body: s2 } : err(401, "refresh_token_invalid")),
      "/data/v1/query": () => ({ body: [] }),
    });
    const j = jar({ [ACCESS]: stale(), [REFRESH]: "rt1" });
    const early = createServerClient({ url, key, fetch, cookies: j.cookies });
    const late = createServerClient({ url, key, fetch, cookies: j.cookies }); // read rt1 before the rotation
    await early.from("t").select("id");
    await late.from("t").select("id");
    expect(refreshes).toBe(2);
    expect(j.m.get(ACCESS)).toBe(s2.access_token);
    expect(j.m.get(REFRESH)).toBe("rt2");
    expect(j.writes).toHaveLength(1);
    expect(hdr(fetch.to("/data/v1/query")[1]!, "chelabase-user")).toBeNull();
  });

  test("second_client_after_a_refresh_uses_the_new_token", async () => {
    const s2 = session(2);
    const fetch = router({ "/auth/v1/app/refresh": () => ({ body: s2 }), "/data/v1/query": () => ({ body: [] }) });
    const j = jar({ [REFRESH]: "rt1" });
    await createServerClient({ url, key, fetch, cookies: j.cookies }).from("t").select("id");
    await createServerClient({ url, key, fetch, cookies: j.cookies }).from("t").select("id");
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(1);
    expect(fetch.to("/data/v1/query").map((c) => hdr(c, "chelabase-user"))).toEqual([s2.access_token, s2.access_token]);
  });

  test("forged_access_cookie_decides_nothing", async () => {
    const forged = jwt(nowSecs() + 10 * 365 * 86_400, "x", "someone-else");
    const fetch = router({
      "/auth/v1/app/user": () => err(401, "user_token_invalid"),
      "/data/v1/query": () => err(401, "user_token_invalid"),
    });
    const j = jar({ [ACCESS]: forged, [REFRESH]: "rt1" });
    const db = createServerClient({ url, key, fetch, cookies: j.cookies });
    // The module and the client expose nothing read from the cookie.
    expect(Object.keys(serverModule).sort()).toEqual([
      "WebhookVerificationError",
      "createAuthHandler",
      "createServerClient",
      "parseCookies",
      "serializeCookie",
      "verifyWebhook",
    ]);
    expect(Object.keys(db).sort()).toEqual(["auth", "batch", "call", "database", "from", "storage"]);
    await expect(db.auth.getUser()).rejects.toMatchObject({ status: 401, code: "user_token_invalid" });
    await expect(db.from("t").select("id")).rejects.toMatchObject({ code: "user_token_invalid" });
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(0);
    expect(j.writes).toEqual([]);
  });

  test("default_cookie_names_and_unprefixed_lookalikes_are_ignored", async () => {
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/data/v1/query": () => ({ body: [] }),
    });
    const j = jar({ "chelabase.chk_pub_0123.refresh": "planted", "chelabase.chk_pub_0123.access": jwt(nowSecs() + 60, "p") });
    const db = createServerClient({ url, key, fetch, cookies: j.cookies });
    await db.from("t").select("id");
    expect(fetch.to("/auth/v1/app/refresh")).toHaveLength(0);
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBeNull();
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    for (const c of j.writes[0]!) {
      expect([ACCESS, REFRESH]).toContain(c.name);
      expect(c.options).toMatchObject({ path: "/", secure: true });
      expect(c.options.domain).toBeUndefined();
    }
  });

  test("sign_in_writes_both_cookies_and_sign_out_clears_them", async () => {
    const s1 = session(1);
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: s1 }),
      "/auth/v1/app/sign-out": () => ({ status: 204 }),
      "/data/v1/query": () => ({ body: [] }),
    });
    const j = jar();
    const db = createServerClient({ url, key, fetch, cookies: j.cookies });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(j.writes[0]!.map((c) => [c.name, c.value, c.options.httpOnly])).toEqual([
      [ACCESS, s1.access_token, false],
      [REFRESH, "rt1", true],
    ]);
    await db.from("t").select("id");
    expect(hdr(fetch.to("/data/v1/query")[0]!, "chelabase-user")).toBe(s1.access_token);
    await db.auth.signOut();
    expect(hdr(fetch.to("/auth/v1/app/sign-out")[0]!, "chelabase-user")).toBe(s1.access_token);
    expect(j.writes[1]!.map((c) => [c.name, c.value, c.options.maxAge])).toEqual([
      [ACCESS, "", 0],
      [REFRESH, "", 0],
    ]);
    expect(j.m.size).toBe(0);
  });

  test("session_issuing_auth_without_set_all_rejects_before_any_fetch", async () => {
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/auth/v1/app/sign-up": () => ({ status: 201, body: session(1) }),
      "/auth/v1/app/change-password": () => ({ body: session(2) }),
      "/auth/v1/app/refresh": () => ({ body: session(2) }),
    });
    const access = jwt(nowSecs() + 60, 1);
    const db = createServerClient({ url, key, fetch, cookies: jar({ [ACCESS]: access, [REFRESH]: "rt1" }).readOnly });
    for (const call of [
      () => db.auth.signIn({ email: "a@b.co", password: "pw" }),
      () => db.auth.signUp({ email: "a@b.co", password: "pw" }),
      () => db.auth.changePassword({ currentPassword: "pw", password: "pw2" }),
      () => db.auth.refresh(),
      () => db.auth.signOut(),
    ]) {
      await expect(call()).rejects.toThrow(/cookies\.setAll/);
    }
    expect(fetch.calls).toHaveLength(0);
  });

  test("server_key_and_client_ip_go_only_to_the_auth_api", async () => {
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1, 3) }),
      "/auth/v1/app/refresh": () => ({ body: session(2) }),
      "/data/v1/query": () => ({ body: [] }),
    });
    const db = createServerClient({ url, key, serverKey, clientIp: "203.0.113.9", fetch, cookies: jar().cookies });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    await db.from("t").select("id"); // the 3 s token refreshes first
    expect(fetch.calls.map((c) => new URL(c.url).pathname)).toEqual([
      "/auth/v1/app/sign-in",
      "/auth/v1/app/refresh",
      "/data/v1/query",
    ]);
    for (const c of fetch.calls.slice(0, 2)) {
      expect(hdr(c, "chelabase-server-key")).toBe(serverKey);
      expect(hdr(c, "chelabase-client-ip")).toBe("203.0.113.9");
      expect(hdr(c, "authorization")).toBe(`Bearer ${key}`);
    }
    const q = fetch.calls[2]!;
    expect(hdr(q, "chelabase-server-key")).toBeNull();
    expect(hdr(q, "chelabase-client-ip")).toBeNull();
    expect(JSON.stringify(q.init)).not.toContain(serverKey);
  });

  test("server_key_alone_goes_only_to_the_auth_api", async () => {
    const fetch = router({
      "/auth/v1/app/sign-in": () => ({ body: session(1, 3) }),
      "/auth/v1/app/refresh": () => ({ body: session(2) }),
      "/data/v1/query": () => ({ body: [] }),
    });
    for (const clientIp of [undefined, ""]) {
      fetch.calls.length = 0;
      const db = createServerClient({ url, key, serverKey, ...(clientIp === undefined ? {} : { clientIp }), fetch, cookies: jar().cookies });
      await db.auth.signIn({ email: "a@b.co", password: "pw" });
      await db.from("t").select("id"); // the 3 s token refreshes first
      expect(fetch.calls.map((c) => new URL(c.url).pathname)).toEqual([
        "/auth/v1/app/sign-in",
        "/auth/v1/app/refresh",
        "/data/v1/query",
      ]);
      for (const c of fetch.calls.slice(0, 2)) {
        expect(hdr(c, "chelabase-server-key")).toBe(serverKey);
        expect(hdr(c, "chelabase-client-ip")).toBeNull();
      }
      const q = fetch.calls[2]!;
      expect(hdr(q, "chelabase-server-key")).toBeNull();
      expect(hdr(q, "chelabase-client-ip")).toBeNull();
      expect(JSON.stringify(q.init)).not.toContain(serverKey);
    }
  });

  test("a_blank_server_key_counts_as_absent", async () => {
    const fetch = router({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const cookies = jar().cookies;
    const db = createServerClient({ url, key, serverKey: "", clientIp: "", fetch, cookies });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(hdr(fetch.calls[0]!, "chelabase-server-key")).toBeNull();
    expect(hdr(fetch.calls[0]!, "chelabase-client-ip")).toBeNull();
    expect(() => createServerClient({ url, key, serverKey: "", clientIp: "203.0.113.9", cookies })).toThrow(
      /`clientIp` needs `serverKey`/,
    );
  });

  test("client_ip_needs_server_key", () => {
    const cookies = jar().cookies;
    const f = () => createServerClient({ url, key, clientIp: "203.0.113.9", cookies });
    expect(f).toThrow(/`clientIp` needs `serverKey`/);
    expect(f).not.toThrow(key.slice(8));
    // An empty one counts as missing; the server key alone is enough.
    expect(() => createServerClient({ url, key, clientIp: "", cookies })).not.toThrow();
    expect(() => createServerClient({ url, key, serverKey, cookies })).not.toThrow();
  });

  test("wrong_key_kinds_throw_at_creation", () => {
    const cookies = jar().cookies;
    expect(() => createServerClient({ url, key, serverKey: key, cookies })).toThrow(/serverKey/);
    expect(() => createServerClient({ url, key: serverKey, cookies })).toThrow(/key/);
    expect(() => createServerClient({ url, key: "whatever", cookies })).toThrow(/key/);
    // The full shape: prefix and base62, nothing else.
    for (const bad of ["chk_pub_", "chk_pub_abc;x", "chk_pub_abc def", "chk_pub_abc\r\n", "chk_pub_ab-c"]) {
      expect(() => createServerClient({ url, key: bad, cookies }), JSON.stringify(bad)).toThrow(/`key`/);
    }
    for (const bad of ["chk_sec_", "chk_sec_abc\n", "chk_sec_abc\0", "chk_sec_ab_c"]) {
      const f = () => createServerClient({ url, key, serverKey: bad, clientIp: "203.0.113.9", cookies });
      expect(f, JSON.stringify(bad)).toThrow(/`serverKey`/);
    }
  });

  test("server_key_never_appears_in_an_error", async () => {
    const errors: unknown[] = [];
    const capture = async (f: () => unknown) => {
      try {
        await f();
      } catch (e) {
        errors.push(e);
      }
    };
    const cookies = jar({ [ACCESS]: stale(), [REFRESH]: "rt1" }).cookies;
    const leakyPub = `chk_pub_${serverKey.slice(8)}`;
    await capture(() => createServerClient({ url, key, serverKey: leakyPub, cookies }));
    await capture(() => createServerClient({ url, key: serverKey, cookies }));
    const fail = router({
      "/auth/v1/app/sign-in": () => err(400, "invalid_credentials"),
      "/auth/v1/app/refresh": () => err(401, "refresh_token_reused"),
      "/auth/v1/app/password-reset": () => ({ status: 502, body: "bad gateway" }),
    });
    const db = createServerClient({ url, key, serverKey, clientIp: "203.0.113.9", fetch: fail, cookies });
    await capture(() => db.auth.signIn({ email: "a@b.co", password: "pw" }));
    await capture(() => db.auth.requestPasswordReset({ email: "a@b.co" }));
    await capture(() => db.from("t").select("id"));
    const down = (() => Promise.reject(new TypeError("fetch failed"))) as typeof fetch;
    const offline = createServerClient({ url, key, serverKey, clientIp: "203.0.113.9", fetch: down, cookies: jar().cookies });
    await capture(() => offline.auth.signIn({ email: "a@b.co", password: "pw" }));
    // A malformed key must not reach `fetch`, whose header error quotes the value.
    const strict = (async (_u: string, init: RequestInit) => {
      new Headers(init.headers);
      return new Response(JSON.stringify(session(1)));
    }) as typeof fetch;
    for (const bad of [`${serverKey}\r`, `${serverKey}\n`, `${serverKey}\0`]) {
      await capture(async () => {
        const c = createServerClient({ url, key, serverKey: bad, clientIp: "203.0.113.9", fetch: strict, cookies: jar().cookies });
        await c.auth.signIn({ email: "a@b.co", password: "pw" });
      });
    }
    const secret = serverKey.slice(8);
    for (const e of errors) {
      const x = e as Error;
      for (const text of [String(x), x.message, x.stack ?? "", JSON.stringify(x)]) expect(text).not.toContain(secret);
    }
    expect(errors).toHaveLength(9);
  });

  test("cookie_options_reach_the_written_cookies", async () => {
    const fetch = router({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const j = jar();
    const db = createServerClient({
      url,
      key,
      fetch,
      cookies: j.cookies,
      cookieOptions: { secure: false, domain: "example.test", maxAge: 3600 },
    });
    await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(j.writes[0]!.map((c) => c.name)).toEqual(["chelabase.chk_pub_0123.access", "chelabase.chk_pub_0123.refresh"]);
    for (const c of j.writes[0]!) {
      expect(c.options).toMatchObject({ secure: false, domain: "example.test", maxAge: 3600, sameSite: "lax", path: "/" });
    }
  });
});

describe("refreshIfNeeded and currentUser", () => {
  test("refresh_if_needed_with_a_fresh_cookie_sends_nothing", async () => {
    const fetch = router({});
    const j = jar({ [ACCESS]: jwt(nowSecs() + 60, 1), [REFRESH]: "rt1" });
    await expect(createServerClient({ url, key, fetch, cookies: j.cookies }).auth.refreshIfNeeded()).resolves.toBeUndefined();
    expect(fetch.calls).toHaveLength(0);
    expect(j.writes).toEqual([]);
  });

  test("refresh_if_needed_refreshes_a_stale_cookie_once_and_writes_before_resolving", async () => {
    const log: string[] = [];
    const s2 = session(2);
    const fetch = router({ "/auth/v1/app/refresh": () => ({ body: s2 }) }, log);
    const j = jar({ [ACCESS]: stale(), [REFRESH]: "rt1" }, log);
    const db = createServerClient({ url, key, serverKey, fetch, cookies: j.cookies });
    await Promise.all([db.auth.refreshIfNeeded(), db.auth.refreshIfNeeded()]);
    log.push("resolved");
    expect(log).toEqual(["/auth/v1/app/refresh", "setAll", "resolved"]);
    expect(hdr(fetch.calls[0]!, "chelabase-server-key")).toBe(serverKey);
    expect(j.m.get(ACCESS)).toBe(s2.access_token);
    // Fresh now: nothing more.
    await db.auth.refreshIfNeeded();
    expect(fetch.calls).toHaveLength(1);
  });

  test("refresh_if_needed_without_set_all_sends_nothing", async () => {
    const fetch = router({ "/auth/v1/app/refresh": () => ({ body: session(2) }) });
    const db = createServerClient({ url, key, fetch, cookies: jar({ [ACCESS]: stale(), [REFRESH]: "rt1" }).readOnly });
    await expect(db.auth.refreshIfNeeded()).resolves.toBeUndefined();
    expect(fetch.calls).toHaveLength(0);
  });

  test("refresh_if_needed_resolves_when_the_session_has_ended", async () => {
    for (const code of ["refresh_token_invalid", "refresh_token_reused", "user_inactive"]) {
      const fetch = router({ "/auth/v1/app/refresh": () => err(401, code) });
      const db = createServerClient({ url, key, fetch, cookies: jar({ [ACCESS]: stale(), [REFRESH]: "rt1" }).cookies });
      await expect(db.auth.refreshIfNeeded(), code).resolves.toBeUndefined();
    }
    const down = router({ "/auth/v1/app/refresh": () => err(503, "unavailable") });
    const db = createServerClient({ url, key, fetch: down, cookies: jar({ [ACCESS]: stale(), [REFRESH]: "rt1" }).cookies });
    await expect(db.auth.refreshIfNeeded()).rejects.toMatchObject({ status: 503 });
  });

  test("current_user_is_null_without_a_session_and_sends_nothing", async () => {
    const fetch = router({});
    for (const cookies of [jar().cookies, jar().readOnly, jar({ [ACCESS]: stale(), [REFRESH]: "rt1" }).readOnly]) {
      expect(await createServerClient({ url, key, fetch, cookies }).auth.currentUser()).toBeNull();
    }
    expect(fetch.calls).toHaveLength(0);
  });

  test("current_user_returns_the_user_from_the_gateway", async () => {
    const access = jwt(nowSecs() + 60, 1);
    const fetch = router({ "/auth/v1/app/user": () => ({ body: user }) });
    const db = createServerClient({ url, key, serverKey, fetch, cookies: jar({ [ACCESS]: access }).readOnly });
    expect(await db.auth.currentUser()).toEqual(user);
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBe(access);
  });

  test("current_user_is_null_when_the_session_is_over", async () => {
    for (const code of ["user_session_revoked", "user_token_invalid", "user_token_expired"]) {
      const fetch = router({ "/auth/v1/app/user": () => err(401, code) });
      const j = jar({ [ACCESS]: jwt(nowSecs() + 60, 1) });
      const db = createServerClient({ url, key, fetch, cookies: j.cookies });
      expect(await db.auth.currentUser(), code).toBeNull();
    }
  });

  test("current_user_is_null_when_its_refresh_ends_the_session", async () => {
    for (const code of ["refresh_token_reused", "user_inactive"]) {
      const fetch = router({ "/auth/v1/app/refresh": () => err(401, code) });
      const j = jar({ [ACCESS]: stale(), [REFRESH]: "rt1" });
      expect(await createServerClient({ url, key, fetch, cookies: j.cookies }).auth.currentUser(), code).toBeNull();
      expect(j.m.size, code).toBe(0);
    }
  });

  test("current_user_refreshes_once_after_an_expired_answer", async () => {
    const access = jwt(nowSecs() + 60, 1);
    const s2 = session(2);
    const log: string[] = [];
    const fetch = router(
      {
        "/auth/v1/app/refresh": () => ({ body: s2 }),
        "/auth/v1/app/user": (c) => (hdr(c, "chelabase-user") === access ? err(401, "user_token_expired") : { body: user }),
      },
      log,
    );
    const j = jar({ [ACCESS]: access, [REFRESH]: "rt1" }, log);
    expect(await createServerClient({ url, key, fetch, cookies: j.cookies }).auth.currentUser()).toEqual(user);
    expect(log).toEqual(["/auth/v1/app/user", "/auth/v1/app/refresh", "setAll", "/auth/v1/app/user"]);
    expect(hdr(fetch.calls[2]!, "chelabase-user")).toBe(s2.access_token);
  });

  test("current_user_rejects_on_other_errors", async () => {
    const fetch = router({ "/auth/v1/app/user": () => err(503, "unavailable") });
    const db = createServerClient({ url, key, fetch, cookies: jar({ [ACCESS]: jwt(nowSecs() + 60, 1) }).cookies });
    await expect(db.auth.currentUser()).rejects.toMatchObject({ status: 503 });
  });
});

describe("ambiguous cookies", () => {
  test("a_duplicated_session_cookie_is_no_session", async () => {
    const access = jwt(nowSecs() + 60, 1);
    for (const list of [
      [
        { name: ACCESS, value: jwt(nowSecs() + 60, 9, "attacker") },
        { name: ACCESS, value: access },
        { name: REFRESH, value: "rt1" },
      ],
      [
        { name: ACCESS, value: stale() },
        { name: REFRESH, value: "planted" },
        { name: REFRESH, value: "rt1" },
      ],
    ]) {
      const fetch = router({ "/auth/v1/app/refresh": () => ({ body: session(2) }), "/data/v1/query": () => ({ body: [] }) });
      const writes: CookieToSet[][] = [];
      const db = createServerClient({ url, key, fetch, cookies: { getAll: () => list, setAll: (l) => void writes.push(l) } });
      await db.from("t").select("id");
      expect(fetch.calls.map((c) => new URL(c.url).pathname)).toEqual(["/data/v1/query"]);
      expect(hdr(fetch.calls[0]!, "chelabase-user")).toBeNull();
      // An explicit refresh has nothing to send: it never clears the (possibly real) cookies.
      await expect(db.auth.refresh()).rejects.toMatchObject({ code: "refresh_token_invalid" });
      expect(writes).toEqual([]);
    }
  });
});

describe("cookieOptions", () => {
  test("domain_is_validated_at_creation", () => {
    const cookies = jar().cookies;
    for (const domain of ["", "a.test;x", "a.test,b.test", "a .test", "a.test\n", "a.test\0", "\ta.test"]) {
      expect(() => createServerClient({ url, key, cookies, cookieOptions: { domain } }), JSON.stringify(domain)).toThrow(
        /cookieOptions\.domain/,
      );
    }
    expect(() => createServerClient({ url, key, cookies, cookieOptions: { domain: "example.test" } })).not.toThrow();
  });
});

describe("@chelabase/client/server resolution", () => {
  const pkgDir = fileURLToPath(new URL("..", import.meta.url));
  /** Node's own resolver, which always adds the `node`, `import` and `default` conditions. */
  const nodeResolve = (conditions: string[]) =>
    execFileSync(
      process.execPath,
      [
        ...conditions.map((c) => `--conditions=${c}`),
        "--input-type=module",
        "-e",
        "console.log(import.meta.resolve('@chelabase/client/server'))",
      ],
      { cwd: pkgDir, encoding: "utf8" },
    ).trim();
  /** A bundler's view: the first key of the `./server` entry in an active condition (or `default`) wins. */
  const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as {
    exports: Record<string, string | Record<string, string>>;
  };
  const bundlerResolve = (conditions: string[]) => {
    const entry = pkg.exports["./server"]!;
    if (typeof entry === "string") return entry;
    const hit = Object.keys(entry).find((k) => k === "default" || conditions.includes(k));
    return hit === undefined ? undefined : entry[hit];
  };

  test("browser_resolves_to_the_stub_and_server_runtimes_to_server_ts", async () => {
    expect(nodeResolve([])).toMatch(/\/src\/server\.ts$/);
    expect(nodeResolve(["worker"])).toMatch(/\/src\/server\.ts$/);
    expect(bundlerResolve(["browser", "import"])).toBe("./src/server-browser.ts");
    for (const set of [
      ["edge-light"],
      ["workerd"],
      ["node"],
      ["worker"],
      ["edge-light", "worker", "browser"], // Next.js edge (middleware)
      ["workerd", "worker", "browser"], // wrangler
      [],
    ]) {
      expect(bundlerResolve([...set, "import"]), set.join("+")).toBe("./src/server.ts");
    }
    await expect(import("./server-browser.ts")).rejects.toThrow(/only on the server/);
  });
});

describe("cookie helpers from @chelabase/client/server", () => {
  test("parse_cookies_keeps_duplicates_and_raw_bad_escapes", () => {
    expect(serverModule.parseCookies("a=1; a=2; b=%E0%A4%A; c=x%20y")).toEqual([
      { name: "a", value: "1" },
      { name: "a", value: "2" },
      { name: "b", value: "%E0%A4%A" },
      { name: "c", value: "x y" },
    ]);
  });

  test("serialize_cookie_carries_every_attribute", () => {
    const line = serverModule.serializeCookie({
      name: "chelabase.chk_pub_0123.access",
      value: "a b",
      options: { path: "/", secure: true, httpOnly: true, sameSite: "strict", maxAge: 60, domain: "example.test" },
    });
    expect(line).toBe("chelabase.chk_pub_0123.access=a%20b; Path=/; Max-Age=60; Domain=example.test; Secure; HttpOnly; SameSite=Strict");
  });
});
