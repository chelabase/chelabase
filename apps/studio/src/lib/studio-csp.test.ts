// The shipped Content-Security-Policy (#306): the Studio image's entry point and Caddyfile.
import { describe, expect, it } from "vitest";
import { directives, entrypointOrigins, policyTemplate, renderedPolicy, type EntryEnv } from "@/test/studio-csp";

const selfHosted: EntryEnv = { CHELABASE_PUBLIC_SCHEME: "http", CHELABASE_API_HOST: "api.chelabase.localhost:8000" };

describe("the entry point's API origin", () => {
  it.each<[string, EntryEnv, string, string]>([
    ["the self-hosted default", selfHosted, "http://api.chelabase.localhost:8000", "ws://api.chelabase.localhost:8000"],
    ["a custom https domain, normalized as Entry does", { CHELABASE_PUBLIC_SCHEME: " HTTPS ", CHELABASE_API_HOST: " API.Example.COM. " }, "https://api.example.com", "wss://api.example.com"],
    ["a blank scheme is https", { CHELABASE_PUBLIC_SCHEME: "", CHELABASE_API_HOST: "api.example.com" }, "https://api.example.com", "wss://api.example.com"],
    ["an unset scheme is https", { CHELABASE_API_HOST: "api.example.com:8443" }, "https://api.example.com:8443", "wss://api.example.com:8443"],
    ["plain http on a non-localhost host", { CHELABASE_PUBLIC_SCHEME: "http", CHELABASE_API_HOST: "db.internal.example:8080" }, "http://db.internal.example:8080", "ws://db.internal.example:8080"],
    ["an IPv4 address", { CHELABASE_PUBLIC_SCHEME: "http", CHELABASE_API_HOST: "10.0.0.5:8000" }, "http://10.0.0.5:8000", "ws://10.0.0.5:8000"],
    ["a bracketed IPv6 address", { CHELABASE_PUBLIC_SCHEME: "http", CHELABASE_API_HOST: "[FD00::5]:8000" }, "http://[fd00::5]:8000", "ws://[fd00::5]:8000"],
    ["the workbench's hosts", { CHELABASE_PUBLIC_SCHEME: "http", CHELABASE_API_HOST: "api.stack-a1.chelabase.localhost:18000" }, "http://api.stack-a1.chelabase.localhost:18000", "ws://api.stack-a1.chelabase.localhost:18000"],
  ])("%s", (_name, env, api, ws) => {
    expect(entrypointOrigins(env)).toEqual({ api, ws });
  });

  it.each<[string, EntryEnv, RegExp]>([
    ["no API host", { CHELABASE_PUBLIC_SCHEME: "https" }, /CHELABASE_API_HOST is required/],
    ["a blank API host", { CHELABASE_API_HOST: "  " }, /CHELABASE_API_HOST is required/],
    ["an unknown scheme", { CHELABASE_PUBLIC_SCHEME: "ftp", CHELABASE_API_HOST: "api.example.com" }, /must be "http" or "https"/],
    ["a directive smuggled into the host", { CHELABASE_API_HOST: "api.example.com; script-src *" }, /not a valid/],
    ["a quote in the host", { CHELABASE_API_HOST: "api.example.com'" }, /not a valid/],
    ["a space in the host", { CHELABASE_API_HOST: "api example.com" }, /not a valid/],
    ["a wildcard host", { CHELABASE_API_HOST: "*.example.com" }, /not a valid/],
    ["a scheme in the host", { CHELABASE_API_HOST: "https://api.example.com" }, /invalid port/],
    ["a path in the host", { CHELABASE_API_HOST: "api.example.com/v1" }, /not a valid/],
    ["https's default port", { CHELABASE_PUBLIC_SCHEME: "https", CHELABASE_API_HOST: "api.example.com:443" }, /default for https/],
    ["http's default port", { CHELABASE_PUBLIC_SCHEME: "http", CHELABASE_API_HOST: "api.example.com:80" }, /default for http/],
    ["port 0", { CHELABASE_API_HOST: "api.example.com:0" }, /invalid port/],
    ["a port past 65535", { CHELABASE_API_HOST: "api.example.com:65536" }, /invalid port/],
    ["an empty port", { CHELABASE_API_HOST: "api.example.com:" }, /invalid port/],
    ["a label starting with a hyphen", { CHELABASE_API_HOST: "-api.example.com" }, /not a valid/],
    ["an all-digit name that isn't IPv4", { CHELABASE_API_HOST: "300.1.1.1" }, /not a valid/],
    ["an unbracketed IPv6 address", { CHELABASE_API_HOST: "fd00::5" }, /not a valid/],
  ])("refuses %s", (_name, env, message) => {
    let stderr = "";
    try {
      entrypointOrigins(env);
    } catch (e) {
      stderr = String((e as { stderr?: string }).stderr ?? e);
    }
    expect(stderr).toMatch(message);
  });
});

describe("the shipped policy", () => {
  const unsafe = ["'unsafe-inline'", "'unsafe-eval'"];
  const schemeOnly = /^[a-z][a-z0-9+.-]*:$/;

  it("has no 'unsafe-inline' or 'unsafe-eval' anywhere (tokens, not substrings)", () => {
    for (const [name, sources] of directives(renderedPolicy(selfHosted))) {
      for (const bad of unsafe) expect(sources, name).not.toContain(bad);
    }
  });

  it("has no wildcard source in any directive", () => {
    for (const [name, sources] of directives(renderedPolicy(selfHosted))) {
      expect(sources.filter((s) => s.includes("*")), name).toEqual([]);
    }
  });

  it("connect-src is Studio's own origin, the API origin and its websocket form, nothing else", () => {
    const d = directives(renderedPolicy(selfHosted));
    expect(d.get("connect-src")).toEqual(["'self'", "http://api.chelabase.localhost:8000", "ws://api.chelabase.localhost:8000"]);
    const https = directives(renderedPolicy({ CHELABASE_API_HOST: "api.example.com" }));
    expect(https.get("connect-src")).toEqual(["'self'", "https://api.example.com", "wss://api.example.com"]);
  });

  it("names no bare scheme in connect-src or media-src (a scheme allows any host)", () => {
    const d = directives(renderedPolicy(selfHosted));
    expect(d.get("connect-src")!.filter((s) => schemeOnly.test(s))).toEqual([]);
    // blob: is a scheme but names no host: previews of fetched bytes.
    expect(d.get("media-src")).toEqual(["'self'", "blob:", "http://api.chelabase.localhost:8000"]);
  });

  it("style-src allows Studio's stylesheets, the response's nonce and fixed hashes only", () => {
    const d = directives(renderedPolicy(selfHosted));
    const styles = d.get("style-src")!;
    expect(styles.slice(0, 2)).toEqual(["'self'", "'nonce-{http.request.uuid}'"]);
    for (const s of styles.slice(2)) expect(s).toMatch(/^'sha256-[A-Za-z0-9+/]{43}='$/);
    const attrs = d.get("style-src-attr")!;
    expect(attrs[0]).toBe("'unsafe-hashes'");
    for (const s of attrs.slice(1)) expect(s).toMatch(/^'sha256-[A-Za-z0-9+/]{43}='$/);
    // style-src-elem would override style-src for <style> elements: it must not exist.
    expect(d.has("style-src-elem")).toBe(false);
  });

  it("scripts stay same-origin (wasm compilation only)", () => {
    expect(directives(policyTemplate()).get("script-src")).toEqual(["'self'", "'wasm-unsafe-eval'"]);
  });
});
