import { readFileSync } from "node:fs";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { createBrowserClient } from "./browser.ts";
import type { Recorded } from "./test-fetch.ts";

const url = "http://gateway.test:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const ACCESS = "__Host-chelabase.chk_pub_0123.access";
const user = { id: "u1", email: "a@b.co", email_verified: true, created_at: "2026-10-03T00:00:00Z" };
const nowSecs = () => Math.floor(Date.now() / 1000);

function jwt(exp: number, n: number | string, sub = "u1"): string {
  const b64 = (o: unknown) => btoa(JSON.stringify(o)).replace(/=+$/, "").replace(/\+/g, "-").replace(/\//g, "_");
  return `${b64({ alg: "EdDSA" })}.${b64({ exp, sub, n })}.sig`;
}
/** What the route answers: a session without its refresh token. */
const routeSession = (access: string) => ({ access_token: access, token_type: "bearer", expires_in: 900, user });

/** `document.cookie` as the page sees it: readable cookies only, one per name. */
const cookieJar = new Map<string, string>();
/** Cookies a sibling subdomain planted under another path: same name, listed as well. */
const planted: string[] = [];
/** Every `document.cookie = …` line the client wrote. */
const written: string[] = [];
const fakeDocument = {
  get cookie() {
    return [...[...cookieJar].map(([k, v]) => `${k}=${encodeURIComponent(v)}`), ...planted].join("; ");
  },
  set cookie(line: string) {
    written.push(line);
    const [pair] = line.split(";");
    const eq = pair!.indexOf("=");
    const name = pair!.slice(0, eq);
    if (/;\s*Max-Age=0(;|$)/i.test(line)) cookieJar.delete(name);
    else cookieJar.set(name, decodeURIComponent(pair!.slice(eq + 1)));
  },
};
const store = new Map<string, string>();
const fakeLocalStorage = {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, v),
  removeItem: (k: string) => void store.delete(k),
};

beforeEach(() => {
  cookieJar.clear();
  store.clear();
  planted.length = 0;
  written.length = 0;
  cookieJar.set("other", "x");
  Object.assign(globalThis, { document: fakeDocument, localStorage: fakeLocalStorage });
});
afterEach(() => {
  delete (globalThis as { document?: unknown }).document;
  delete (globalThis as { localStorage?: unknown }).localStorage;
});

type Reply = { status?: number; body?: unknown };

/** Routes by URL path (relative route URLs resolve against the page), logging each path. */
function router(routes: Record<string, (c: Recorded) => Reply>) {
  const calls: Recorded[] = [];
  const f = async (input: string | URL | Request, init?: RequestInit) => {
    const c = { url: String(input), init: init ?? {} };
    calls.push(c);
    const h = routes[new URL(c.url, "https://app.example.test").pathname];
    if (!h) return new Response(JSON.stringify({ error: { code: "not_found", message: c.url } }), { status: 404 });
    const r = h(c);
    if (r.status === 204) return new Response(null, { status: 204 });
    return new Response(JSON.stringify(r.body ?? null), { status: r.status ?? 200 });
  };
  return Object.assign(f as typeof fetch, {
    calls,
    paths: () => calls.map((c) => new URL(c.url, "https://app.example.test").pathname),
  });
}
const hdr = (c: Recorded, name: string) => new Headers(c.init.headers).get(name);
const err = (status: number, code: string): Reply => ({ status, body: { error: { code, message: code } } });
const Q = "/data/v1/query";

/** The route's refresh: rotates the cookie the way the handler's Set-Cookie would. */
function refreshRoute(next: () => string) {
  return () => {
    const a = next();
    cookieJar.set(ACCESS, a);
    return { body: routeSession(a) };
  };
}

describe("createBrowserClient", () => {
  test("another_tab_changing_the_cookie_is_adopted", async () => {
    const a1 = jwt(nowSecs() + 600, 1, "u1");
    const a2 = jwt(nowSecs() + 600, 2, "u2");
    cookieJar.set(ACCESS, a1);
    const fetch = router({ [Q]: () => ({ body: [] }) });
    const db = createBrowserClient({ url, key, fetch });
    const events: string[] = [];
    db.auth.onChange((e) => events.push(e));
    await db.from("t").select("id");
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBe(a1);
    cookieJar.set(ACCESS, a2);
    await db.from("t").select("id");
    expect(hdr(fetch.calls[1]!, "chelabase-user")).toBe(a2);
    expect(events).toEqual(["signed_in"]);
    // Same user, new token (another tab refreshed): adopted quietly.
    const a3 = jwt(nowSecs() + 600, 3, "u2");
    cookieJar.set(ACCESS, a3);
    await db.from("t").select("id");
    expect(hdr(fetch.calls[2]!, "chelabase-user")).toBe(a3);
    expect(events).toEqual(["signed_in"]);
    cookieJar.delete(ACCESS);
    await db.from("t").select("id");
    expect(hdr(fetch.calls[3]!, "chelabase-user")).toBeNull();
    expect(events).toEqual(["signed_in", "signed_out"]);
    expect(fetch.paths()).toEqual([Q, Q, Q, Q]);
  });

  test("the_legacy_local_storage_refresh_token_is_removed", () => {
    store.set("chelabase.chk_pub_0123.refresh", "legacy");
    store.set("unrelated", "keep");
    createBrowserClient({ url, key, fetch: router({}) });
    expect(store.has("chelabase.chk_pub_0123.refresh")).toBe(false);
    expect(store.get("unrelated")).toBe("keep");
  });

  test("no_access_cookie_means_no_auth_request", async () => {
    const fetch = router({ [Q]: () => ({ body: [] }) });
    const db = createBrowserClient({ url, key, fetch });
    await db.from("t").select("id");
    expect(fetch.paths()).toEqual([Q]);
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBeNull();
  });

  test("a_valid_cookie_is_used_and_an_expired_one_refreshes_once_first", async () => {
    const valid = jwt(nowSecs() + 600, 1);
    cookieJar.set(ACCESS, valid);
    const f1 = router({ [Q]: () => ({ body: [] }) });
    await createBrowserClient({ url, key, fetch: f1 }).from("t").select("id");
    expect(f1.paths()).toEqual([Q]);
    expect(hdr(f1.calls[0]!, "chelabase-user")).toBe(valid);

    cookieJar.set(ACCESS, jwt(nowSecs() - 10, 0));
    const fresh = jwt(nowSecs() + 600, 2);
    const f2 = router({ "/api/chelabase/auth/refresh": refreshRoute(() => fresh), [Q]: () => ({ body: [] }) });
    const db = createBrowserClient({ url, key, fetch: f2 });
    await Promise.all([db.from("t").select("id"), db.from("u").select("id")]);
    expect(f2.paths()).toEqual(["/api/chelabase/auth/refresh", Q, Q]);
    const r = f2.calls[0]!;
    expect(r.url).toBe("/api/chelabase/auth/refresh");
    expect(r.init.method).toBe("POST");
    expect(r.init.credentials).toBe("same-origin");
    expect(r.init.body).toBeUndefined();
    expect(hdr(r, "content-type")).toBe("application/json");
    expect(hdr(r, "authorization")).toBeNull();
    for (const q of f2.calls.slice(1)) expect(hdr(q, "chelabase-user")).toBe(fresh);
  });

  test("sign_in_goes_through_the_route", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    const fetch = router({
      "/auth/sign-in": (c) => {
        expect(JSON.parse(c.init.body as string)).toEqual({ email: "a@b.co", password: "pw" });
        cookieJar.set(ACCESS, a1);
        return { body: routeSession(a1) };
      },
      [Q]: () => ({ body: [] }),
    });
    const db = createBrowserClient({ url, key, fetch, authPath: "/auth/" });
    const events: [string, unknown][] = [];
    db.auth.onChange((e, u) => events.push([e, u]));
    const s = await db.auth.signIn({ email: "a@b.co", password: "pw" });
    expect(s.access_token).toBe(a1);
    expect(fetch.calls[0]!.url).toBe("/auth/sign-in");
    expect(fetch.calls[0]!.init.credentials).toBe("same-origin");
    expect(fetch.calls.some((c) => c.url.startsWith(url))).toBe(false);
    expect(events).toEqual([["signed_in", user]]);
    await db.from("t").select("id");
    expect(hdr(fetch.calls[1]!, "chelabase-user")).toBe(a1);
    expect(events).toHaveLength(1);
  });

  test("get_user_calls_the_gateway_with_the_access_token", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    cookieJar.set(ACCESS, a1);
    const fetch = router({ "/auth/v1/app/user": () => ({ body: user }) });
    const db = createBrowserClient({ url, key, fetch });
    expect(await db.auth.getUser()).toEqual(user);
    expect(fetch.calls[0]!.url).toBe(`${url}/auth/v1/app/user`);
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBe(a1);
    expect(hdr(fetch.calls[0]!, "authorization")).toBe(`Bearer ${key}`);
  });

  test("an_expired_answer_refreshes_through_the_route_and_retries_once", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    const a2 = jwt(nowSecs() + 600, 2);
    cookieJar.set(ACCESS, a1);
    const fetch = router({
      "/api/chelabase/auth/refresh": refreshRoute(() => a2),
      [Q]: (c) => (hdr(c, "chelabase-user") === a1 ? err(401, "user_token_expired") : { body: [] }),
    });
    const db = createBrowserClient({ url, key, fetch });
    await db.from("t").select("id");
    expect(fetch.paths()).toEqual([Q, "/api/chelabase/auth/refresh", Q]);
    expect(hdr(fetch.calls[2]!, "chelabase-user")).toBe(a2);
  });

  test("sign_out_posts_to_the_route_and_signs_out_locally", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    cookieJar.set(ACCESS, a1);
    const fetch = router({
      "/api/chelabase/auth/sign-out": () => {
        cookieJar.delete(ACCESS);
        return { status: 204 };
      },
      [Q]: () => ({ body: [] }),
    });
    const db = createBrowserClient({ url, key, fetch });
    const events: string[] = [];
    db.auth.onChange((e) => events.push(e));
    await db.auth.signOut();
    expect(fetch.paths()).toEqual(["/api/chelabase/auth/sign-out"]);
    expect(fetch.calls[0]!.init.body).toBeUndefined();
    expect(events).toEqual(["signed_out"]);
    await db.from("t").select("id");
    expect(hdr(fetch.calls[1]!, "chelabase-user")).toBeNull();
  });

  test("a_stale_refresh_with_an_unchanged_cookie_is_not_retried_every_query", async () => {
    cookieJar.set(ACCESS, jwt(nowSecs() - 10, 0));
    const fetch = router({ "/api/chelabase/auth/refresh": () => err(401, "refresh_token_invalid"), [Q]: () => ({ body: [] }) });
    const db = createBrowserClient({ url, key, fetch });
    await db.from("t").select("id");
    await db.from("t").select("id");
    expect(fetch.paths()).toEqual(["/api/chelabase/auth/refresh", Q, Q]);
    for (const q of fetch.calls.slice(1)) expect(hdr(q, "chelabase-user")).toBeNull();
  });

  test("change_password_goes_through_the_route", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    const a2 = jwt(nowSecs() + 600, 2);
    cookieJar.set(ACCESS, a1);
    const fetch = router({
      "/api/chelabase/auth/change-password": (c) => {
        expect(JSON.parse(c.init.body as string)).toEqual({ current_password: "a", password: "b" });
        cookieJar.set(ACCESS, a2);
        return { body: routeSession(a2) };
      },
      [Q]: () => ({ body: [] }),
    });
    const db = createBrowserClient({ url, key, fetch });
    await db.auth.changePassword({ currentPassword: "a", password: "b" });
    await db.from("t").select("id");
    expect(fetch.paths()).toEqual(["/api/chelabase/auth/change-password", Q]);
    expect(hdr(fetch.calls[1]!, "chelabase-user")).toBe(a2);
  });

  test("cookie_options_pick_the_cookie_name", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    cookieJar.set("chelabase.chk_pub_0123.access", a1); // plain name: not read by default
    const f1 = router({ [Q]: () => ({ body: [] }) });
    await createBrowserClient({ url, key, fetch: f1 }).from("t").select("id");
    expect(hdr(f1.calls[0]!, "chelabase-user")).toBeNull();
    const f2 = router({ [Q]: () => ({ body: [] }) });
    await createBrowserClient({ url, key, fetch: f2, cookieOptions: { secure: false } }).from("t").select("id");
    expect(hdr(f2.calls[0]!, "chelabase-user")).toBe(a1);
  });

  test("the_browser_entry_never_reaches_server_code", () => {
    // Walk the static import graph from browser.ts.
    const seen = new Set<string>();
    const walk = (file: string) => {
      if (seen.has(file)) return;
      seen.add(file);
      const src = readFileSync(new URL(`./${file}`, import.meta.url), "utf8");
      for (const m of src.matchAll(/(?:from|import)\s*\(?\s*["']\.\/([^"']+)["']/g)) walk(m[1]!);
    };
    walk("browser.ts");
    expect(seen.has("auth.ts")).toBe(true);
    expect(seen.has("server.ts")).toBe(false);
    expect(seen.has("handler.ts")).toBe(false);
    for (const f of seen) expect(readFileSync(new URL(`./${f}`, import.meta.url), "utf8")).not.toMatch(/node:/);
    const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as {
      exports: Record<string, unknown>;
    };
    expect(pkg.exports["./browser"]).toBe("./src/browser.ts");
  });

  test("sign_out_with_the_route_down_still_drops_the_access_cookie", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    cookieJar.set(ACCESS, a1);
    const fetch = Object.assign(
      (async (input: string | URL | Request) => {
        if (String(input).startsWith("/api/")) throw new TypeError("Failed to fetch");
        return new Response("[]");
      }) as typeof globalThis.fetch,
    );
    const db = createBrowserClient({ url, key, fetch });
    await expect(db.auth.signOut()).rejects.toThrow(/Failed to fetch/);
    expect(cookieJar.has(ACCESS)).toBe(false);
    expect(written).toHaveLength(1);
    expect(written[0]).toMatch(/^__Host-chelabase\.chk_pub_0123\.access=; Path=\/; Max-Age=0; Secure; SameSite=Lax$/);
    // A reload: a fresh client on the same document is anonymous.
    const f2 = router({ [Q]: () => ({ body: [] }) });
    await createBrowserClient({ url, key, fetch: f2 }).from("t").select("id");
    expect(f2.paths()).toEqual([Q]);
    expect(hdr(f2.calls[0]!, "chelabase-user")).toBeNull();
  });

  test("a_revoked_session_drops_the_access_cookie", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    // The plain name (`secure: false`), so the attributes written back differ too.
    cookieJar.set("chelabase.chk_pub_0123.access", a1);
    const fetch = router({ [Q]: (c) => (hdr(c, "chelabase-user") ? err(401, "user_session_revoked") : { body: [] }) });
    const db = createBrowserClient({ url, key, fetch, cookieOptions: { secure: false } });
    await expect(db.from("t").select("id")).rejects.toMatchObject({ code: "user_session_revoked" });
    expect(cookieJar.has("chelabase.chk_pub_0123.access")).toBe(false);
    expect(written[0]).toMatch(/^chelabase\.chk_pub_0123\.access=; Path=\/; Max-Age=0; SameSite=Lax$/);
    const f2 = router({ [Q]: () => ({ body: [] }) });
    await createBrowserClient({ url, key, fetch: f2, cookieOptions: { secure: false } }).from("t").select("id");
    expect(hdr(f2.calls[0]!, "chelabase-user")).toBeNull();
  });

  test("a_revoked_answer_for_an_older_token_leaves_a_newer_cookie", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    const a2 = jwt(nowSecs() + 600, 2, "u2");
    cookieJar.set(ACCESS, a1);
    const fetch = router({
      [Q]: (c) => {
        if (hdr(c, "chelabase-user") === a1) {
          cookieJar.set(ACCESS, a2); // another tab signed in meanwhile
          return err(401, "user_session_revoked");
        }
        return { body: [] };
      },
    });
    const db = createBrowserClient({ url, key, fetch });
    await expect(db.from("t").select("id")).rejects.toMatchObject({ code: "user_session_revoked" });
    expect(cookieJar.get(ACCESS)).toBe(a2);
    expect(written).toEqual([]);
    // And the newer session is taken up.
    await db.from("t").select("id");
    expect(hdr(fetch.calls[1]!, "chelabase-user")).toBe(a2);
  });

  test("a_duplicated_access_cookie_is_no_session", async () => {
    const a1 = jwt(nowSecs() + 600, 1);
    cookieJar.set(ACCESS, a1);
    planted.push(`${ACCESS}=${jwt(nowSecs() + 600, 9, "attacker")}`);
    const fetch = router({ [Q]: () => ({ body: [] }) });
    const db = createBrowserClient({ url, key, fetch });
    await db.from("t").select("id");
    expect(fetch.paths()).toEqual([Q]);
    expect(hdr(fetch.calls[0]!, "chelabase-user")).toBeNull();
  });
});

describe("createBrowserClient refreshIfNeeded and currentUser", () => {
  test("refresh_if_needed_sends_nothing_for_a_fresh_cookie_and_refreshes_a_stale_one", async () => {
    cookieJar.set(ACCESS, jwt(nowSecs() + 600, 1));
    const f1 = router({});
    await createBrowserClient({ url, key, fetch: f1 }).auth.refreshIfNeeded();
    expect(f1.calls).toHaveLength(0);

    cookieJar.set(ACCESS, jwt(nowSecs() - 10, 0));
    const fresh = jwt(nowSecs() + 600, 2);
    const f2 = router({ "/api/chelabase/auth/refresh": refreshRoute(() => fresh) });
    const db = createBrowserClient({ url, key, fetch: f2 });
    await db.auth.refreshIfNeeded();
    expect(f2.paths()).toEqual(["/api/chelabase/auth/refresh"]);
    await db.auth.refreshIfNeeded();
    expect(f2.calls).toHaveLength(1);
  });

  test("current_user_is_null_without_a_cookie_or_when_revoked", async () => {
    const f1 = router({});
    expect(await createBrowserClient({ url, key, fetch: f1 }).auth.currentUser()).toBeNull();
    expect(f1.calls).toHaveLength(0);

    cookieJar.set(ACCESS, jwt(nowSecs() + 600, 1));
    const f2 = router({ "/auth/v1/app/user": () => err(401, "user_session_revoked") });
    expect(await createBrowserClient({ url, key, fetch: f2 }).auth.currentUser()).toBeNull();
    expect(cookieJar.has(ACCESS)).toBe(false);

    cookieJar.set(ACCESS, jwt(nowSecs() + 600, 1));
    const f3 = router({ "/auth/v1/app/user": () => ({ body: user }) });
    expect(await createBrowserClient({ url, key, fetch: f3 }).auth.currentUser()).toEqual(user);

    const f4 = router({ "/auth/v1/app/user": () => err(503, "unavailable") });
    await expect(createBrowserClient({ url, key, fetch: f4 }).auth.currentUser()).rejects.toMatchObject({ status: 503 });
  });
});

describe("createBrowserClient options", () => {
  test("a_secret_or_malformed_key_throws_without_quoting_it", () => {
    const secret = "chk_sec_zyxwvutsrqponmlkjihgfedcbaZYXWVUTSRQPONMLKJIH";
    const f = () => createBrowserClient({ url, key: secret });
    expect(f).toThrow(/secret key never belongs in the browser/);
    expect(f).not.toThrow(secret.slice(8));
    for (const bad of ["chk_pub_", "chk_pub_abc;x", "chk_pub_abc\r\n", "whatever"]) {
      expect(() => createBrowserClient({ url, key: bad }), JSON.stringify(bad)).toThrow(/`key`/);
    }
  });

  test("a_bad_cookie_domain_throws", () => {
    expect(() => createBrowserClient({ url, key, cookieOptions: { domain: "a.test;x" } })).toThrow(/cookieOptions\.domain/);
    expect(() => createBrowserClient({ url, key, cookieOptions: { domain: "example.test" } })).not.toThrow();
  });
});
