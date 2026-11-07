import { beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "./error";
import { configureManagementClient, consoleQueryExact, customFetch, fetchDataApiTypes, fetchMergePlanSql, fetchStorageObject, parseExact, streamRequest } from "./fetch";
import { exportMergePlan } from "./generated/endpoints";

function res(body: string | null, status = 200, type = "application/json"): Response {
  return new Response(body, { status, headers: { "content-type": type } });
}

function setup(fetchImpl: typeof fetch, token: string | undefined = "tok", onUnauthorized: () => boolean | void = () => {}) {
  configureManagementClient({ baseUrl: "https://api.test", getToken: () => token, onUnauthorized, fetchImpl });
}

describe("customFetch", () => {
  beforeEach(() => setup(vi.fn() as unknown as typeof fetch));

  it("bearer_and_json_headers", async () => {
    const f = vi.fn(async () => res('{"ok":true}'));
    setup(f as unknown as typeof fetch);
    const out = await customFetch<{ ok: boolean }>("/v1/x", { method: "POST", body: '{"a":1}', headers: { "X-Extra": "1" } });
    expect(out).toEqual({ ok: true });
    const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://api.test/v1/x");
    const h = new Headers(init.headers);
    expect(h.get("authorization")).toBe("Bearer tok");
    expect(h.get("accept")).toBe("application/json");
    expect(h.get("content-type")).toBe("application/json");
    expect(h.get("x-extra")).toBe("1");
    expect(init.method).toBe("POST");
  });

  it("empty_2xx_is_undefined", async () => {
    setup((async () => res(null, 204)) as typeof fetch);
    expect(await customFetch("/v1/x", { method: "DELETE" })).toBeUndefined();
    setup((async () => res("", 200)) as typeof fetch);
    expect(await customFetch("/v1/x", { method: "GET" })).toBeUndefined();
  });

  it("error_body_becomes_api_error_with_extras", async () => {
    setup((async () => res('{"error":{"code":"quota_exceeded","message":"Too many","limit":3,"plan":"free"}}', 409)) as typeof fetch);
    const e = await customFetch("/v1/x", { method: "POST" }).catch((x) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ name: "ApiError", status: 409, code: "quota_exceeded", message: "Too many", signInRestarted: false });
    expect((e as ApiError).extra).toEqual({ limit: 3, plan: "free" });
  });

  it("html_502_is_unavailable_not_a_parse_error", async () => {
    setup((async () => res("<html>Bad gateway</html>", 502, "text/html")) as typeof fetch);
    const e = await customFetch("/v1/x", { method: "GET" }).catch((x) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 502, code: "unavailable", message: "Request failed with status 502" });
    setup((async () => res('{"other":1}', 500)) as typeof fetch);
    const e2 = await customFetch("/v1/x", { method: "GET" }).catch((x) => x);
    expect(e2).toMatchObject({ status: 500, code: "internal", message: "Request failed with status 500" });
  });

  it("network_failure_is_network", async () => {
    setup((async () => {
      throw new TypeError("fetch failed");
    }) as typeof fetch);
    const e = await customFetch("/v1/x", { method: "GET" }).catch((x) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 0, code: "network", message: "Can't reach Chelabase; check your connection" });
  });

  it("401_calls_on_unauthorized_and_sets_sign_in_restarted", async () => {
    const onUnauthorized = vi.fn(() => true);
    setup((async () => res('{"error":{"code":"unauthenticated","message":"Sign in"}}', 401)) as typeof fetch, "tok", onUnauthorized);
    const e = await customFetch("/v1/x", { method: "GET" }).catch((x) => x);
    expect(onUnauthorized).toHaveBeenCalledTimes(1);
    expect(e).toMatchObject({ status: 401, code: "unauthenticated", signInRestarted: true });
    const none = vi.fn();
    setup((async () => res("", 401)) as typeof fetch, undefined, none);
    const e2 = await customFetch("/v1/x", { method: "GET" }).catch((x) => x);
    expect(none).toHaveBeenCalled();
    expect(e2).toMatchObject({ status: 401, signInRestarted: false });
  });
});

describe("edge cases", () => {
  it("aborted_request_rethrows_abort_error", async () => {
    const ac = new AbortController();
    ac.abort();
    const abort = new DOMException("The operation was aborted", "AbortError");
    setup((async () => {
      throw abort;
    }) as typeof fetch);
    const e = await customFetch("/v1/x", { method: "GET", signal: ac.signal }).catch((x) => x);
    expect(e).toBe(abort);
    expect(e).not.toBeInstanceOf(ApiError);
  });

  it("non_json_2xx_is_api_error_internal", async () => {
    setup((async () => res("<html>ok</html>", 200, "text/html")) as typeof fetch);
    const e = await customFetch("/v1/x", { method: "GET" }).catch((x) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 200, code: "internal", message: "Unexpected response from Chelabase" });
  });

  it("flat_string_error_body_falls_back", async () => {
    setup((async () => res('{"error":"x"}', 500)) as typeof fetch);
    const e = await customFetch("/v1/x", { method: "GET" }).catch((x) => x);
    expect(e).toMatchObject({ status: 500, code: "internal", message: "Request failed with status 500" });
  });

  it("headers_instance_is_merged", async () => {
    const f = vi.fn(async () => res("{}"));
    setup(f as unknown as typeof fetch);
    await customFetch("/v1/x", { method: "GET", headers: new Headers({ "X-A": "1" }) });
    const init = (f.mock.calls[0] as unknown as [string, RequestInit])[1];
    const h = new Headers(init.headers);
    expect(h.get("x-a")).toBe("1");
    expect(h.get("authorization")).toBe("Bearer tok");
  });
});

describe("unconfigured", () => {
  it("unconfigured_client_throws", async () => {
    vi.resetModules();
    const fresh = await import("./fetch");
    await expect(fresh.customFetch("/x", {})).rejects.toThrow(/configureManagementClient/);
  });
});

describe("parseExact", () => {
  it("keeps numbers that don't survive a double as their source text", () => {
    const out = parseExact('[{"id":9007199254740993,"n":0.12345678901234567890,"ok":1.5,"i":42,"p":1.50,"s":"x","z":null}]') as Record<string, unknown>[];
    // gf item 3: `1.50` keeps its scale (it was `p: 1.5`)
    expect(out[0]).toEqual({ id: "9007199254740993", n: "0.12345678901234567890", ok: 1.5, i: 42, p: "1.50", s: "x", z: null });
  });
});

describe("consoleQueryExact", () => {
  it("posts the console request and reads the answer exactly", async () => {
    const f = vi.fn(async () => res('[{"id":9007199254740993}]'));
    setup(f as unknown as typeof fetch);
    const out = await consoleQueryExact("d", "b", { role: null, database: null, tree: { v: 1 } });
    expect(out).toEqual([{ id: "9007199254740993" }]);
    const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://api.test/v1/databases/d/branches/b/console/query");
    expect(init.method).toBe("POST");
    f.mockImplementationOnce(async () => res('{"error":{"code":"unknown_table","message":"m"}}', 404));
    await expect(consoleQueryExact("d", "b", { tree: {} })).rejects.toMatchObject({ code: "unknown_table" });
  });
});

describe("streamRequest", () => {
  it("streamRequest_throws_ApiError_on_409", async () => {
    setup((async () => res('{"error":{"code":"sql_confirm_required","message":"Confirm","statements":["DROP TABLE t"]}}', 409)) as typeof fetch);
    const e = await streamRequest("/v1/sql", { sql: "x" }, new AbortController().signal).catch((x) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 409, code: "sql_confirm_required" });
    expect((e as ApiError).extra).toEqual({ statements: ["DROP TABLE t"] });
  });

  it("streamRequest_calls_onUnauthorized_on_401", async () => {
    const onUnauthorized = vi.fn(() => true);
    setup((async () => res('{"error":{"code":"unauthorized","message":"No"}}', 401)) as typeof fetch, "tok", onUnauthorized);
    const e = await streamRequest("/v1/sql", {}, new AbortController().signal).catch((x) => x);
    expect(onUnauthorized).toHaveBeenCalledTimes(1);
    expect(e).toMatchObject({ status: 401, signInRestarted: true });
  });

  it("returns_the_body_stream_with_bearer_and_ndjson_accept", async () => {
    const f = vi.fn(async () => res('{"t":"done"}\n', 200, "application/x-ndjson"));
    setup(f as unknown as typeof fetch);
    const stream = await streamRequest("/v1/sql", { a: 1 }, new AbortController().signal);
    expect(await new Response(stream).text()).toBe('{"t":"done"}\n');
    const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://api.test/v1/sql");
    expect(init.method).toBe("POST");
    expect(init.body).toBe('{"a":1}');
    const h = new Headers(init.headers);
    expect(h.get("authorization")).toBe("Bearer tok");
    expect(h.get("content-type")).toBe("application/json");
    expect(h.get("accept")).toContain("application/x-ndjson");
  });
});

describe("fetchMergePlanSql", () => {
  it("returns_the_sql_text_unparsed_with_bearer", async () => {
    const sql = "-- phase 1\nBEGIN;\nCREATE TABLE t (id int);\nCOMMIT;\n";
    const f = vi.fn(async () => res(sql, 200, "text/plain; charset=utf-8"));
    setup(f as unknown as typeof fetch);
    expect(await fetchMergePlanSql("d", "m")).toBe(sql);
    const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://api.test/v1/databases/d/merges/m/plan.sql");
    expect(init.method).toBe("GET");
    expect(new Headers(init.headers).get("authorization")).toBe("Bearer tok");
  });

  it("the_generated_export_client_returns_the_text_too", async () => {
    const sql = "-- phase 1\nBEGIN;\nCOMMIT;\n";
    setup((async () => res(sql, 200, "text/plain; charset=utf-8")) as typeof fetch);
    expect(await exportMergePlan("d", "m")).toBe(sql);
    // A JSON answer is still parsed.
    setup((async () => res('{"a":1}', 200)) as typeof fetch);
    expect(await customFetch("/v1/x", { method: "GET" })).toEqual({ a: 1 });
  });

  it("errors_are_api_errors", async () => {
    setup((async () => res('{"error":{"code":"conflict","message":"the merge has no plan yet"}}', 409)) as typeof fetch);
    await expect(fetchMergePlanSql("d", "m")).rejects.toMatchObject({ status: 409, code: "conflict" });
  });
});

describe("fetchDataApiTypes", () => {
  it("returns_the_module_text_unparsed_with_the_options", async () => {
    const module = "export type Database = { a: 1 };\n";
    const f = vi.fn(async () => res(module, 200, "text/typescript; charset=utf-8"));
    setup(f as unknown as typeof fetch);
    expect(await fetchDataApiTypes("d", "b", { role: "a b", database: "analytics" })).toBe(module);
    const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://api.test/v1/databases/d/branches/b/data-api/types?role=a+b&database=analytics");
    expect(init.method).toBe("GET");
    expect(new Headers(init.headers).get("authorization")).toBe("Bearer tok");
  });

  it("the_json_map_comes_back_as_its_text_too", async () => {
    setup((async () => res('{"defaultSchema":"public"}', 200)) as typeof fetch);
    expect(await fetchDataApiTypes("d", "b", { format: "json" })).toBe('{"defaultSchema":"public"}');
  });

  it("errors_are_api_errors", async () => {
    setup((async () => res('{"error":{"code":"forbidden","message":"this needs role:x","required":"role:x"}}', 403)) as typeof fetch);
    await expect(fetchDataApiTypes("d", "b")).rejects.toMatchObject({ status: 403, code: "forbidden" });
  });
});

describe("fetchStorageObject", () => {
  it("returns_the_bytes_as_a_blob_with_the_role_and_an_encoded_key", async () => {
    const f = vi.fn(async () => new Response(new Uint8Array([0, 159, 255]), { status: 200, headers: { "content-type": "image/png" } }));
    setup(f as unknown as typeof fetch);
    const blob = await fetchStorageObject("d", "b", "avatars", "photos/a b.png", { role: "reporter" });
    expect(blob).toBeInstanceOf(Blob);
    expect(blob.type).toBe("image/png");
    expect([...new Uint8Array(await blob.arrayBuffer())]).toEqual([0, 159, 255]);
    const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://api.test/v1/databases/d/branches/b/storage/object/avatars/photos%2Fa%20b.png?role=reporter");
    expect(init.method).toBe("GET");
    const h = new Headers(init.headers);
    expect(h.get("authorization")).toBe("Bearer tok");
    expect(h.get("range")).toBeNull();
  });

  it("sends_a_range_and_reads_a_206", async () => {
    const f = vi.fn(async () => new Response("hello", { status: 206, headers: { "content-type": "text/plain" } }));
    setup(f as unknown as typeof fetch);
    const blob = await fetchStorageObject("d", "b", "docs", "a.txt", undefined, { range: "bytes=0-4" });
    expect(await blob.text()).toBe("hello");
    const [, init] = f.mock.calls[0] as unknown as [string, RequestInit];
    expect(new Headers(init.headers).get("range")).toBe("bytes=0-4");
  });

  it("errors_are_api_errors_and_a_401_calls_onUnauthorized", async () => {
    setup((async () => res('{"error":{"code":"object_not_found","message":"no such object"}}', 404)) as typeof fetch);
    await expect(fetchStorageObject("d", "b", "docs", "a.txt")).rejects.toMatchObject({ status: 404, code: "object_not_found", message: "no such object" });
    const onUnauthorized = vi.fn(() => true);
    setup((async () => res('{"error":{"code":"unauthorized","message":"expired"}}', 401)) as typeof fetch, "tok", onUnauthorized);
    await expect(fetchStorageObject("d", "b", "docs", "a.txt")).rejects.toMatchObject({ status: 401, signInRestarted: true });
    expect(onUnauthorized).toHaveBeenCalledTimes(1);
  });

  it("a_network_failure_is_a_network_error_but_an_abort_stays_an_abort", async () => {
    setup((async () => Promise.reject(new TypeError("fetch failed"))) as typeof fetch);
    await expect(fetchStorageObject("d", "b", "docs", "a.txt")).rejects.toMatchObject({ code: "network" });
    const controller = new AbortController();
    controller.abort();
    setup((async () => Promise.reject(new DOMException("aborted", "AbortError"))) as typeof fetch);
    await expect(fetchStorageObject("d", "b", "docs", "a.txt", undefined, { signal: controller.signal })).rejects.toMatchObject({ name: "AbortError" });
  });
});
