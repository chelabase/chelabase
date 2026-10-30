import { describe, expect, test } from "vitest";
import { type AuthHandlerOptions, createAuthHandler } from "./server.ts";
import type { Recorded } from "./test-fetch.ts";

const url = "http://gateway.test:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const serverKey = "chk_sec_zyxwvutsrqponmlkjihgfedcbaZYXWVUTSRQPONMLKJIH";
const siteUrl = "https://app.example.test";
const ACCESS = "__Host-chelabase.chk_pub_0123.access";
const REFRESH = "__Host-chelabase.chk_pub_0123.refresh";
const user = { id: "u1", email: "a@b.co", email_verified: true, created_at: "2026-10-03T00:00:00Z" };

const nowSecs = () => Math.floor(Date.now() / 1000);
function jwt(exp: number, n: number): string {
  const b64 = (o: unknown) => btoa(JSON.stringify(o)).replace(/=+$/, "").replace(/\+/g, "-").replace(/\//g, "_");
  return `${b64({ alg: "EdDSA" })}.${b64({ exp, sub: "u1", n })}.sig`;
}

const session = (n: number) => ({
  access_token: `at${n}`,
  token_type: "bearer",
  expires_in: 900,
  refresh_token: `rt${n}`,
  user,
});

type Reply = { status?: number; body?: unknown };

/** A fake gateway: answers by path, records every call. */
function gateway(routes: Record<string, (c: Recorded) => Reply>) {
  const calls: Recorded[] = [];
  const f = async (input: string | URL | Request, init?: RequestInit) => {
    const c = { url: String(input), init: init ?? {} };
    calls.push(c);
    const h = routes[new URL(c.url).pathname];
    if (!h) return new Response(JSON.stringify({ error: { code: "not_found", message: "no" } }), { status: 404 });
    const r = h(c);
    if (r.status === 204) return new Response(null, { status: 204 });
    return new Response(JSON.stringify(r.body ?? null), { status: r.status ?? 200 });
  };
  return Object.assign(f as typeof fetch, { calls });
}
const err = (status: number, code: string): Reply => ({ status, body: { error: { code, message: code } } });
const hdr = (c: Recorded, name: string) => new Headers(c.init.headers).get(name);
const sent = (c: Recorded) => JSON.parse(c.init.body as string) as unknown;

function handler(fetch: typeof globalThis.fetch, extra: Partial<AuthHandlerOptions> = {}) {
  return createAuthHandler({ url, key, siteUrl, fetch, ...extra });
}

function req(
  flow: string,
  o: { method?: string; origin?: string | null; type?: string | null; body?: unknown; cookies?: Record<string, string>; headers?: Record<string, string>; base?: string } = {},
): Request {
  const headers = new Headers(o.headers);
  const origin = o.origin === undefined ? siteUrl : o.origin;
  if (origin !== null) headers.set("origin", origin);
  const type = o.type === undefined ? "application/json" : o.type;
  if (type !== null) headers.set("content-type", type);
  if (o.cookies) {
    headers.set(
      "cookie",
      Object.entries(o.cookies)
        .map(([k, v]) => `${k}=${v}`)
        .join("; "),
    );
  }
  const method = o.method ?? "POST";
  return new Request(`${o.base ?? "http://localhost:4000"}/api/chelabase/auth/${flow}`, {
    method,
    headers,
    ...(method === "GET" || method === "HEAD" ? {} : { body: o.body === undefined ? "" : JSON.stringify(o.body) }),
  });
}

/** The response's cookies by name: value, and the raw attribute line. */
function setCookies(res: Response): Map<string, { value: string; line: string }> {
  const out = new Map<string, { value: string; line: string }>();
  for (const line of res.headers.getSetCookie()) {
    const [pair] = line.split(";");
    const eq = pair!.indexOf("=");
    out.set(pair!.slice(0, eq), { value: decodeURIComponent(pair!.slice(eq + 1)), line });
  }
  return out;
}
const cleared = (res: Response) => {
  const c = setCookies(res);
  return [ACCESS, REFRESH].every((n) => c.get(n)?.value === "" && /Max-Age=0/.test(c.get(n)!.line));
};

describe("createAuthHandler", () => {
  test("sign_in_sets_both_cookies_and_hides_the_refresh_token", async () => {
    const gw = gateway({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const res = await handler(gw)(req("sign-in", { body: { email: "a@b.co", password: "pw" } }));
    expect(res.status).toBe(200);
    expect(gw.calls).toHaveLength(1);
    expect(gw.calls[0]!.url).toBe(`${url}/auth/v1/app/sign-in`);
    expect(sent(gw.calls[0]!)).toEqual({ email: "a@b.co", password: "pw" });
    expect(hdr(gw.calls[0]!, "authorization")).toBe(`Bearer ${key}`);
    const c = setCookies(res);
    expect(c.get(REFRESH)!.value).toBe("rt1");
    expect(c.get(REFRESH)!.line).toMatch(/; HttpOnly/);
    expect(c.get(REFRESH)!.line).toMatch(/; Secure/);
    expect(c.get(ACCESS)!.value).toBe("at1");
    expect(c.get(ACCESS)!.line).not.toMatch(/HttpOnly/);
    const text = await res.text();
    expect(text).not.toContain("rt1");
    const b = JSON.parse(text) as Record<string, unknown>;
    expect(b).not.toHaveProperty("refresh_token");
    expect(b).toMatchObject({ access_token: "at1", expires_in: 900, user });
    expect(res.headers.get("cache-control")).toBe("no-store");
  });

  test("refusals_make_no_upstream_call", async () => {
    const gw = gateway({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const h = handler(gw);
    const body = { email: "a@b.co", password: "pw" };
    const cases: [Request, number][] = [
      [req("sign-in", { body, origin: "https://evil.test" }), 403],
      [req("sign-in", { body, origin: "https://app.example.test:444" }), 403],
      [req("sign-in", { body, origin: "http://app.example.test" }), 403],
      [req("sign-in", { body, origin: "https://sub.app.example.test" }), 403],
      [req("sign-in", { body, origin: null }), 403],
      [req("sign-in", { body, origin: "null" }), 403],
      [req("sign-in", { method: "GET" }), 405],
      [req("sign-in", { method: "PUT", body }), 405],
      [req("sign-in", { body, type: "text/plain" }), 415],
      [req("sign-in", { body, type: "application/x-www-form-urlencoded" }), 415],
      [req("sign-in", { body, type: "multipart/form-data; boundary=x" }), 415],
      [req("sign-in", { body, type: null }), 415],
      [req("sign-in", { body, type: "application/jsonx" }), 415],
    ];
    for (const [r, status] of cases) {
      const res = await h(r);
      expect(res.status, `${r.method} ${r.headers.get("origin")} ${r.headers.get("content-type")}`).toBe(status);
      expect(res.headers.getSetCookie()).toEqual([]);
      const e = (await res.json()) as { error: { code: string } };
      expect(typeof e.error.code).toBe("string");
    }
    expect(gw.calls).toHaveLength(0);
    // A JSON content type with parameters is fine.
    expect((await h(req("sign-in", { body, type: "application/json; charset=utf-8" }))).status).toBe(200);
  });

  test("behind_a_proxy_the_origin_is_checked_against_site_url", async () => {
    const gw = gateway({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const h = handler(gw);
    const body = { email: "a@b.co", password: "pw" };
    // req.url is the internal address; the browser's Origin is the public site.
    expect((await h(req("sign-in", { body, base: "http://localhost:4000" }))).status).toBe(200);
    // An Origin equal to the internal req.url origin is not the site.
    expect((await h(req("sign-in", { body, base: "http://localhost:4000", origin: "http://localhost:4000" }))).status).toBe(403);
    // A siteUrl with a path still compares origins only.
    const h2 = handler(gw, { siteUrl: `${siteUrl}/app/` });
    expect((await h2(req("sign-in", { body }))).status).toBe(200);
    // allowedOrigins adds exact origins.
    const h3 = handler(gw, { allowedOrigins: ["https://admin.example.test"] });
    expect((await h3(req("sign-in", { body, origin: "https://admin.example.test" }))).status).toBe(200);
    expect((await h3(req("sign-in", { body, origin: "https://other.example.test" }))).status).toBe(403);
  });

  test("allowed_origins_refuse_wildcards_and_null", () => {
    const gw = gateway({});
    for (const o of ["*", "null", "", "https://a.test/path", "not a url", "https://*.example.test"]) {
      expect(() => handler(gw, { allowedOrigins: [o] }), o).toThrow(/allowedOrigins/);
    }
    expect(() => handler(gw, { siteUrl: "not a url" })).toThrow(/siteUrl/);
  });

  test("only_the_exact_flows_are_routed", async () => {
    const gw = gateway({});
    const h = handler(gw);
    const body = { email: "a@b.co", password: "pw" };
    for (const flow of [
      "sign-in%2F..%2F..%2Fdata%2Fv1%2Fquery",
      "sign-in%2f..%2f..%2fdata%2fv1%2fquery",
      "sign-in/",
      "SIGN-IN",
      "Sign-In",
      "password-reset/confirm",
      "password-reset",
      "verify-email",
      "user",
      "sign-in/x?x=1",
      "sign-in/x",
      "",
      "%73ign-in",
      "sign-in%00",
      "%E0%A4%A",
    ]) {
      const res = await h(req(flow, { body }));
      expect(res.status, flow).toBe(404);
    }
    // Outside the mount point.
    const outside = new Request("http://localhost:4000/api/chelabase/other/sign-in", {
      method: "POST",
      headers: { origin: siteUrl, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    expect((await h(outside)).status).toBe(404);
    expect(gw.calls).toHaveLength(0);
  });

  test("a_query_string_on_an_allowed_flow_is_ignored", async () => {
    const gw = gateway({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const res = await handler(gw)(req("sign-in?redirect=/data/v1/query", { body: { email: "a@b.co", password: "pw" } }));
    expect(res.status).toBe(200);
    expect(gw.calls.map((c) => c.url)).toEqual([`${url}/auth/v1/app/sign-in`]);
  });

  test("a_custom_mount_point", async () => {
    const gw = gateway({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const h = handler(gw, { authPath: "/auth/" });
    const r = new Request("http://localhost:4000/auth/sign-in", {
      method: "POST",
      headers: { origin: siteUrl, "content-type": "application/json" },
      body: JSON.stringify({ email: "a@b.co", password: "pw" }),
    });
    expect((await h(r)).status).toBe(200);
    expect((await h(req("sign-in", { body: {} }))).status).toBe(404);
  });

  test("server_key_alone_is_sent_without_a_client_ip_and_no_header_is_read", async () => {
    const gw = gateway({
      "/auth/v1/app/sign-in": () => ({ body: session(1) }),
      "/auth/v1/app/refresh": () => ({ body: session(2) }),
    });
    const h = handler(gw, { serverKey });
    const headers = { "x-forwarded-for": "1.2.3.4", "x-vercel-forwarded-for": "203.0.113.9" };
    await h(req("sign-in", { body: { email: "a@b.co", password: "pw" }, headers }));
    await h(req("refresh", { cookies: { [REFRESH]: "rt1" }, headers }));
    expect(gw.calls).toHaveLength(2);
    for (const c of gw.calls) {
      expect(hdr(c, "chelabase-server-key")).toBe(serverKey);
      expect(hdr(c, "chelabase-client-ip")).toBeNull();
      expect(hdr(c, "x-forwarded-for")).toBeNull();
    }
    // Without either, nothing extra reaches the gateway.
    const plain = gateway({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    await handler(plain)(req("sign-in", { body: { email: "a@b.co", password: "pw" }, headers }));
    expect(hdr(plain.calls[0]!, "chelabase-client-ip")).toBeNull();
    expect(hdr(plain.calls[0]!, "chelabase-server-key")).toBeNull();
    expect(hdr(plain.calls[0]!, "x-forwarded-for")).toBeNull();
  });

  test("a_blank_server_key_counts_as_absent", async () => {
    const gw = gateway({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const h = handler(gw, { serverKey: "" });
    await h(req("sign-in", { body: { email: "a@b.co", password: "pw" } }));
    expect(hdr(gw.calls[0]!, "chelabase-server-key")).toBeNull();
    expect(() => handler(gw, { serverKey: "", clientIp: () => "203.0.113.9" })).toThrow(/`clientIp` needs `serverKey`/);
  });

  test("client_ip_needs_server_key", () => {
    const f = () => handler(gateway({}), { clientIp: () => "203.0.113.9" });
    expect(f).toThrow(/`clientIp` needs `serverKey`/);
    expect(f).not.toThrow(key.slice(8));
  });

  test("client_ip_option_supplies_the_ip_with_the_server_key", async () => {
    const gw = gateway({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    const h = handler(gw, { serverKey, clientIp: (r) => r.headers.get("x-vercel-forwarded-for") ?? undefined });
    await h(
      req("sign-in", {
        body: { email: "a@b.co", password: "pw" },
        headers: { "x-vercel-forwarded-for": "203.0.113.9", "x-forwarded-for": "1.2.3.4" },
      }),
    );
    expect(hdr(gw.calls[0]!, "chelabase-client-ip")).toBe("203.0.113.9");
    expect(hdr(gw.calls[0]!, "chelabase-server-key")).toBe(serverKey);
    // No IP (absent or empty) for this request: the server key alone.
    await h(req("sign-in", { body: { email: "a@b.co", password: "pw" } }));
    await h(req("sign-in", { body: { email: "a@b.co", password: "pw" }, headers: { "x-vercel-forwarded-for": "" } }));
    for (const c of gw.calls.slice(1)) {
      expect(hdr(c, "chelabase-client-ip")).toBeNull();
      expect(hdr(c, "chelabase-server-key")).toBe(serverKey);
    }
  });

  test("key_shapes_are_checked_without_quoting_them", () => {
    const gw = gateway({});
    const cases: Partial<AuthHandlerOptions>[] = [
      { key: serverKey },
      { key: "chk_pub_bad\nvalue" },
      { serverKey: key, clientIp: () => "1.2.3.4" },
      { serverKey: "chk_sec_bad value", clientIp: () => "1.2.3.4" },
      { clientIp: () => "1.2.3.4" },
    ];
    for (const c of cases) {
      let msg = "";
      try {
        handler(gw, c);
      } catch (e) {
        msg = (e as Error).message;
      }
      expect(msg, JSON.stringify(Object.keys(c))).toMatch(/^createAuthHandler: /);
      expect(msg).not.toContain("0123456789");
      expect(msg).not.toContain("zyxwvu");
      expect(msg).not.toContain("bad");
    }
    expect(() => handler(gw, { cookieOptions: { domain: "a.test;x" } })).toThrow(/cookieOptions\.domain/);
  });

  test("sign_out_sends_only_the_refresh_cookie_and_always_clears", async () => {
    const expired = "expired.jwt.value";
    const gw = gateway({ "/auth/v1/app/sign-out": () => ({ status: 204 }) });
    const res = await handler(gw)(req("sign-out", { cookies: { [ACCESS]: expired, [REFRESH]: "rt1" }, body: { refresh_token: "other" } }));
    expect(res.status).toBe(204);
    expect(gw.calls).toHaveLength(1);
    expect(sent(gw.calls[0]!)).toEqual({ refresh_token: "rt1" });
    expect(hdr(gw.calls[0]!, "chelabase-user")).toBeNull();
    expect(cleared(res)).toBe(true);

    for (const failing of [err(500, "internal"), err(401, "refresh_token_invalid"), err(429, "rate_limited")]) {
      const gw2 = gateway({ "/auth/v1/app/sign-out": () => failing });
      const r2 = await handler(gw2)(req("sign-out", { cookies: { [ACCESS]: expired, [REFRESH]: "rt1" } }));
      expect(cleared(r2)).toBe(true);
      expect(r2.status).toBe(failing.status);
    }
    // The network failing too.
    const down = Object.assign((async () => {
      throw new TypeError("fetch failed");
    }) as typeof fetch);
    const r3 = await handler(down)(req("sign-out", { cookies: { [REFRESH]: "rt1" } }));
    expect(r3.status).toBe(502);
    expect(cleared(r3)).toBe(true);
    // No refresh cookie: nothing to send, still cleared.
    const gw4 = gateway({});
    const r4 = await handler(gw4)(req("sign-out", { cookies: { [ACCESS]: expired } }));
    expect(gw4.calls).toHaveLength(0);
    expect(r4.status).toBe(204);
    expect(cleared(r4)).toBe(true);
  });

  test("refresh_token_invalid_clears_nothing_while_the_access_cookie_is_recent", async () => {
    // Within 5 min of its `exp` (clock skew between the app server and auth).
    for (const ago of [30, 240]) {
      const gw = gateway({ "/auth/v1/app/refresh": () => err(401, "refresh_token_invalid") });
      const res = await handler(gw)(req("refresh", { cookies: { [ACCESS]: jwt(nowSecs() - ago, 0), [REFRESH]: "rt0" } }));
      expect(res.status).toBe(401);
      expect(res.headers.getSetCookie(), String(ago)).toEqual([]);
      expect(((await res.json()) as { error: { code: string } }).error.code).toBe("refresh_token_invalid");
    }
  });

  test("refresh_token_invalid_clears_a_long_dead_session", async () => {
    // Missing, unreadable, or more than 5 min past its `exp`.
    for (const access of [undefined, "at0", jwt(nowSecs() - 400, 0)]) {
      const gw = gateway({ "/auth/v1/app/refresh": () => err(401, "refresh_token_invalid") });
      const cookies = { ...(access === undefined ? {} : { [ACCESS]: access }), [REFRESH]: "rt0" };
      const res = await handler(gw)(req("refresh", { cookies }));
      expect(res.status).toBe(401);
      expect(cleared(res), String(access)).toBe(true);
      expect(((await res.json()) as { error: { code: string } }).error.code).toBe("refresh_token_invalid");
    }
  });

  test("refresh_uses_the_cookie_never_the_body", async () => {
    const gw = gateway({ "/auth/v1/app/refresh": () => ({ body: session(2) }) });
    const res = await handler(gw)(req("refresh", { cookies: { [REFRESH]: "rt1" }, body: { refresh_token: "attacker" } }));
    expect(sent(gw.calls[0]!)).toEqual({ refresh_token: "rt1" });
    expect(hdr(gw.calls[0]!, "chelabase-user")).toBeNull();
    const c = setCookies(res);
    expect(c.get(REFRESH)!.value).toBe("rt2");
    expect(c.get(ACCESS)!.value).toBe("at2");
    expect(await res.text()).not.toContain("rt2");
    // Without a refresh cookie: refused here, nothing cleared, no upstream call.
    const gw2 = gateway({});
    const r2 = await handler(gw2)(req("refresh", { body: { refresh_token: "attacker" } }));
    expect(gw2.calls).toHaveLength(0);
    expect(r2.status).toBe(401);
    expect(r2.headers.getSetCookie()).toEqual([]);
  });

  test("fatal_refresh_errors_clear_both_cookies", async () => {
    for (const code of ["refresh_token_reused", "user_inactive", "user_session_revoked"]) {
      const gw = gateway({ "/auth/v1/app/refresh": () => err(401, code) });
      const res = await handler(gw)(req("refresh", { cookies: { [ACCESS]: "at0", [REFRESH]: "rt0" } }));
      expect(res.status).toBe(401);
      expect(cleared(res), code).toBe(true);
      expect(await res.json()).toEqual({ error: { code, message: code } });
    }
  });

  test("change_password_sends_the_access_cookie_and_rotates", async () => {
    const gw = gateway({ "/auth/v1/app/change-password": () => ({ body: session(3) }) });
    const res = await handler(gw)(
      req("change-password", {
        cookies: { [ACCESS]: "at2", [REFRESH]: "rt2" },
        body: { current_password: "a", password: "b" },
      }),
    );
    expect(hdr(gw.calls[0]!, "chelabase-user")).toBe("at2");
    expect(sent(gw.calls[0]!)).toEqual({ current_password: "a", password: "b" });
    expect(setCookies(res).get(REFRESH)!.value).toBe("rt3");
    expect(await res.text()).not.toContain("rt3");
    // A revoked session clears.
    const gw2 = gateway({ "/auth/v1/app/change-password": () => err(401, "user_session_revoked") });
    const r2 = await handler(gw2)(req("change-password", { cookies: { [ACCESS]: "at2" }, body: {} }));
    expect(cleared(r2)).toBe(true);
    // No access cookie: refused here.
    const gw3 = gateway({});
    const r3 = await handler(gw3)(req("change-password", { body: {} }));
    expect(r3.status).toBe(401);
    expect(gw3.calls).toHaveLength(0);
  });

  test("sign_up_with_and_without_a_session", async () => {
    const gw = gateway({ "/auth/v1/app/sign-up": () => ({ body: session(1) }) });
    const res = await handler(gw)(req("sign-up", { body: { email: "a@b.co", password: "pw", data: { name: "A" } } }));
    expect(sent(gw.calls[0]!)).toEqual({ email: "a@b.co", password: "pw", data: { name: "A" } });
    expect(setCookies(res).get(REFRESH)!.value).toBe("rt1");
    expect(await res.text()).not.toContain("rt1");
    const gw2 = gateway({ "/auth/v1/app/sign-up": () => ({ body: { user, verification_required: true } }) });
    const r2 = await handler(gw2)(req("sign-up", { body: { email: "a@b.co", password: "pw" } }));
    expect(r2.headers.getSetCookie()).toEqual([]);
    expect(await r2.json()).toEqual({ user, verification_required: true });
  });

  test("errors_pass_through_as_the_envelope_and_sign_in_failures_clear_nothing", async () => {
    const gw = gateway({ "/auth/v1/app/sign-in": () => err(401, "user_inactive") });
    const res = await handler(gw)(req("sign-in", { cookies: { [ACCESS]: "at0", [REFRESH]: "rt0" }, body: { email: "a@b.co", password: "x" } }));
    expect(res.status).toBe(401);
    expect(res.headers.getSetCookie()).toEqual([]);
    expect(await res.json()).toEqual({ error: { code: "user_inactive", message: "user_inactive" } });
    // A body that isn't a JSON object is refused before the gateway.
    const gw2 = gateway({});
    for (const raw of ["not json", "[1]", "\"s\"", "null", ""]) {
      const r = new Request("http://localhost:4000/api/chelabase/auth/sign-in", {
        method: "POST",
        headers: { origin: siteUrl, "content-type": "application/json" },
        body: raw,
      });
      expect((await handler(gw2)(r)).status, raw).toBe(400);
    }
    expect(gw2.calls).toHaveLength(0);
  });

  test("cookies_are_never_forwarded_and_requests_are_not_redirected", async () => {
    const gw = gateway({ "/auth/v1/app/sign-in": () => ({ body: session(1) }) });
    await handler(gw)(req("sign-in", { cookies: { [ACCESS]: "at0", other: "x" }, body: { email: "a@b.co", password: "pw" } }));
    expect(hdr(gw.calls[0]!, "cookie")).toBeNull();
    expect(hdr(gw.calls[0]!, "chelabase-user")).toBeNull();
    expect(gw.calls[0]!.init.redirect).toBe("manual");
  });

  test("duplicated_session_cookies_are_ambiguous", async () => {
    // A sibling subdomain planted a same-named cookie under a longer path: it comes first.
    const twice = (name: string, a: string, b: string) => `${name}=${a}; ${name}=${b}`;
    const raw = (flow: string, cookie: string, body: unknown = {}) =>
      new Request(`http://localhost:4000/api/chelabase/auth/${flow}`, {
        method: "POST",
        headers: { origin: siteUrl, "content-type": "application/json", cookie },
        body: JSON.stringify(body),
      });
    const gw = gateway({
      "/auth/v1/app/refresh": () => ({ body: session(2) }),
      "/auth/v1/app/change-password": () => ({ body: session(2) }),
      "/auth/v1/app/sign-out": () => ({ status: 204 }),
    });
    const h = handler(gw);
    const dupRefresh = `${twice(REFRESH, "planted", "rt1")}; ${ACCESS}=at1`;
    const dupAccess = `${twice(ACCESS, "planted", "at1")}; ${REFRESH}=rt1`;
    for (const cookie of [dupRefresh, dupAccess]) {
      const r1 = await h(raw("refresh", cookie));
      expect(r1.status).toBe(401);
      expect(r1.headers.getSetCookie()).toEqual([]);
      const r2 = await h(raw("change-password", cookie, { current_password: "a", password: "b" }));
      expect(r2.status).toBe(401);
      const r3 = await h(raw("sign-out", cookie));
      expect(r3.status).toBe(204);
      expect(cleared(r3)).toBe(true);
    }
    expect(gw.calls).toHaveLength(0);
  });
});
