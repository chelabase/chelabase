import { describe, expect, test } from "vitest";
import { cookieNames, parseCookies, readSession, serializeCookie, sessionCookies } from "./cookies.ts";

const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const sess = { access_token: "a.b.c", refresh_token: "rt/1+=" };

describe("session cookies", () => {
  test("default_names_use_the_host_prefix", () => {
    expect(cookieNames(key)).toEqual({
      access: "__Host-chelabase.chk_pub_0123.access",
      refresh: "__Host-chelabase.chk_pub_0123.refresh",
    });
  });

  test("plain_names_only_without_secure_or_with_a_domain", () => {
    const plain = { access: "chelabase.chk_pub_0123.access", refresh: "chelabase.chk_pub_0123.refresh" };
    expect(cookieNames(key, { secure: false })).toEqual(plain);
    expect(cookieNames(key, { domain: "example.test" })).toEqual(plain);
    expect(cookieNames(key, { sameSite: "strict", maxAge: 60 })).toEqual(cookieNames(key));
  });

  test("default_attributes", () => {
    const [access, refresh] = sessionCookies(cookieNames(key), sess);
    expect(access).toEqual({
      name: "__Host-chelabase.chk_pub_0123.access",
      value: "a.b.c",
      options: { path: "/", secure: true, httpOnly: false, sameSite: "lax", maxAge: 400 * 86_400 },
    });
    expect(refresh).toEqual({
      name: "__Host-chelabase.chk_pub_0123.refresh",
      value: "rt/1+=",
      options: { path: "/", secure: true, httpOnly: true, sameSite: "lax", maxAge: 400 * 86_400 },
    });
    expect(serializeCookie(access!)).toBe(
      "__Host-chelabase.chk_pub_0123.access=a.b.c; Path=/; Max-Age=34560000; Secure; SameSite=Lax",
    );
    expect(serializeCookie(refresh!)).not.toMatch(/Domain/i);
    expect(serializeCookie(refresh!)).toMatch(/; HttpOnly/);
  });

  test("overrides_apply_to_both_cookies", () => {
    const o = { secure: false, domain: "example.test", maxAge: 3600, sameSite: "strict" } as const;
    for (const c of sessionCookies(cookieNames(key, o), sess, o)) {
      expect(c.options).toMatchObject({ path: "/", secure: false, domain: "example.test", maxAge: 3600, sameSite: "strict" });
      const line = serializeCookie(c);
      expect(line).toMatch(/; Domain=example\.test/);
      expect(line).toMatch(/; Max-Age=3600/);
      expect(line).toMatch(/; SameSite=Strict/);
      expect(line).not.toMatch(/Secure/);
    }
  });

  test("clearing_empties_both_with_max_age_zero", () => {
    const names = cookieNames(key);
    const cleared = sessionCookies(names, null);
    expect(cleared.map((c) => [c.name, c.value, c.options.maxAge])).toEqual([
      [names.access, "", 0],
      [names.refresh, "", 0],
    ]);
    expect(cleared[1]!.options.httpOnly).toBe(true);
    expect(serializeCookie(cleared[0]!)).toMatch(/=; Path=\/; Max-Age=0; Secure/);
  });

  test("serialize_parse_round_trip", () => {
    const cookies = sessionCookies(cookieNames(key), sess);
    const header = cookies.map((c) => serializeCookie(c).split(";")[0]).join("; ");
    expect(parseCookies(header)).toEqual(cookies.map(({ name, value }) => ({ name, value })));
    expect(readSession(parseCookies(header), cookieNames(key))).toEqual({ access: "a.b.c", refresh: "rt/1+=" });
  });

  test("parse_skips_malformed_pairs", () => {
    expect(parseCookies("a=1; junk; =x; b=%E0%A4%A; c = 3 ")).toEqual([
      { name: "a", value: "1" },
      { name: "b", value: "%E0%A4%A" },
      { name: "c", value: "3" },
    ]);
  });

  test("read_takes_only_the_configured_names", () => {
    const names = cookieNames(key);
    const got = readSession(
      [
        { name: "chelabase.chk_pub_0123.refresh", value: "planted" },
        { name: "__host-chelabase.chk_pub_0123.access", value: "planted" },
        { name: "__Host-chelabase.chk_pub_0123.refresh2", value: "planted" },
        { name: names.refresh, value: "rt1" },
      ],
      names,
    );
    expect(got).toEqual({ access: undefined, refresh: "rt1" });
  });

  test("empty_values_read_as_missing", () => {
    const names = cookieNames(key);
    expect(readSession([{ name: names.access, value: "" }, { name: names.refresh, value: "" }], names)).toEqual({
      access: undefined,
      refresh: undefined,
    });
  });
});
