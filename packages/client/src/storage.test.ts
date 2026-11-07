import { describe, expect, test } from "vitest";
import { createClient, StorageError } from "./index.ts";

const url = "http://localhost:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const branch = "7d4f0a52-3b1c-4c2e-9a51-0f6e5d8c9b10";
const MiB = 1024 * 1024;

interface Call {
  method: string;
  url: URL;
  headers: Headers;
  body: BodyInit | null | undefined;
}

/** A fake `fetch` routed by a handler; records every call (the body as sent). */
function routed(handler: (c: Call) => Response | Promise<Response>) {
  const calls: Call[] = [];
  const f = async (input: string | URL | Request, init?: RequestInit) => {
    const c: Call = {
      method: init?.method ?? "GET",
      url: new URL(String(input)),
      headers: new Headers(init?.headers),
      body: init?.body,
    };
    calls.push(c);
    init?.signal?.throwIfAborted();
    return handler(c);
  };
  return Object.assign(f as typeof fetch, { calls });
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
const fail = (status: number, code: string) => json({ error: { code, message: `${code} happened` } }, status);
const stored = (name: string, size: number) => json({ bucket: "b", name, version_id: "v1", size, etag: "e1" });

async function sizeOf(body: BodyInit | null | undefined): Promise<number> {
  return (await new Response(body).arrayBuffer()).byteLength;
}

/** A multipart server: one session `s1`, parts kept by number, `failPart` refused by `onPart`. */
function multipartServer(onPart?: (n: number, attempt: number) => Response | Error | undefined) {
  const parts = new Map<number, number>();
  const attempts = new Map<number, number>();
  const fetch = routed(async (c) => {
    const p = c.url.pathname;
    if (c.method === "POST" && p === "/storage/v1/upload/b/big.bin") {
      return json({ id: "s1", bucket: "b", name: "big.bin", expires_at: 1, min_part_size: 5 * MiB, max_part_size: 100 * MiB });
    }
    if (c.method === "GET" && p === "/storage/v1/upload/session/s1") {
      return json({
        bucket: "b",
        name: "big.bin",
        expires_at: 1,
        parts: [...parts].map(([n, size]) => ({ part_number: n, size, etag: `p${n}` })),
      });
    }
    const m = /^\/storage\/v1\/upload\/session\/s1\/(\d+)$/.exec(p);
    if (c.method === "PUT" && m) {
      const n = Number(m[1]);
      const attempt = (attempts.get(n) ?? 0) + 1;
      attempts.set(n, attempt);
      const refused = onPart?.(n, attempt);
      if (refused instanceof Error) throw refused;
      if (refused) return refused;
      const size = await sizeOf(c.body);
      parts.set(n, size);
      return json({ part_number: n, size, etag: `p${n}` });
    }
    if (c.method === "POST" && p === "/storage/v1/upload/session/s1/complete") {
      const total = [...parts.values()].reduce((a, b) => a + b, 0);
      return stored("big.bin", total);
    }
    return fail(404, "not_routed");
  });
  return { fetch, parts };
}

const partCalls = (calls: Call[]) =>
  calls.filter((c) => c.method === "PUT").map((c) => Number(c.url.pathname.split("/").pop()));

describe("upload", () => {
  test("AC1: a 12 MiB Blob goes up in parts of the configured size, progress monotonic to 100%", async () => {
    const { fetch } = multipartServer();
    const db = createClient({ url, key, fetch });
    const blob = new Blob([new Uint8Array(12 * MiB)], { type: "application/octet-stream" });
    const seen: { loaded: number; total: number | undefined }[] = [];
    const res = await db.storage.from("b").upload("big.bin", blob, {
      partSize: 5 * MiB,
      multipartThreshold: 5 * MiB,
      onProgress: (p) => seen.push(p),
    });
    expect(res).toEqual({ bucket: "b", name: "big.bin", version_id: "v1", size: 12 * MiB, etag: "e1" });
    const puts = fetch.calls.filter((c) => c.method === "PUT");
    expect(await Promise.all(puts.map((c) => sizeOf(c.body)))).toEqual([5 * MiB, 5 * MiB, 2 * MiB]);
    expect(partCalls(fetch.calls)).toEqual([1, 2, 3]);
    expect(seen.length).toBeGreaterThan(1);
    for (let i = 1; i < seen.length; i++) expect(seen[i]!.loaded).toBeGreaterThanOrEqual(seen[i - 1]!.loaded);
    expect(seen.at(-1)).toEqual({ loaded: 12 * MiB, total: 12 * MiB });
    expect(seen.every((p) => p.total === 12 * MiB)).toBe(true);
    // The session is started with the content type, and completed last.
    expect(fetch.calls[0]!.headers.get("content-type")).toBe("application/octet-stream");
    expect(fetch.calls.at(-1)!.url.pathname).toBe("/storage/v1/upload/session/s1/complete");
  });

  test("AC2: after the network fails on part 2, the same upload again sends only the missing parts", async () => {
    let down = true;
    const { fetch } = multipartServer((n) => (n === 2 && down ? new TypeError("fetch failed") : undefined));
    const db = createClient({ url, key, fetch });
    const bucket = db.storage.from("b");
    const blob = new Blob([new Uint8Array(12 * MiB)]);
    const opts = { partSize: 5 * MiB, multipartThreshold: 5 * MiB, retries: 0 };
    await expect(bucket.upload("big.bin", blob, opts)).rejects.toThrow("fetch failed");
    expect(partCalls(fetch.calls)).toEqual([1, 2]);

    down = false;
    const before = fetch.calls.length;
    const seen: number[] = [];
    await bucket.upload("big.bin", blob, { ...opts, onProgress: (p) => seen.push(p.loaded) });
    const again = fetch.calls.slice(before);
    // No new session: the old one is asked what it holds, and only parts 2 and 3 go up.
    expect(again.some((c) => c.method === "POST" && c.url.pathname === "/storage/v1/upload/b/big.bin")).toBe(false);
    expect(again[0]!.method).toBe("GET");
    expect(partCalls(again)).toEqual([2, 3]);
    expect(seen.at(-1)).toBe(12 * MiB);
    expect(seen[0]).toBeGreaterThanOrEqual(5 * MiB);
  });

  test("a resumed session the server forgot (410) starts a new one", async () => {
    let first = true;
    let expired = false;
    const { fetch: inner } = multipartServer((n) => (n === 2 && first ? new TypeError("fetch failed") : undefined));
    const fetch = routed((c) =>
      c.method === "GET" && expired
        ? fail(410, "upload_expired")
        : inner(c.url, { method: c.method, headers: c.headers, body: c.body as BodyInit }),
    );
    const bucket = createClient({ url, key, fetch }).storage.from("b");
    const blob = new Blob([new Uint8Array(11 * MiB)]);
    const opts = { partSize: 5 * MiB, multipartThreshold: 5 * MiB, retries: 0 };
    await expect(bucket.upload("big.bin", blob, opts)).rejects.toThrow();
    first = false;
    expired = true;
    const before = fetch.calls.length;
    await bucket.upload("big.bin", blob, opts);
    const again = fetch.calls.slice(before);
    expect(again.map((c) => c.method)).toEqual(["GET", "POST", "PUT", "PUT", "PUT", "POST"]);
    expect(partCalls(again)).toEqual([1, 2, 3]);
  });

  test("a finished upload forgets its session: the same upload again starts afresh", async () => {
    const { fetch } = multipartServer();
    const bucket = createClient({ url, key, fetch }).storage.from("b");
    const blob = new Blob([new Uint8Array(6 * MiB)]);
    const opts = { partSize: 5 * MiB, multipartThreshold: 5 * MiB };
    await bucket.upload("big.bin", blob, opts);
    const before = fetch.calls.length;
    await bucket.upload("big.bin", blob, opts);
    expect(fetch.calls[before]!.method).toBe("POST");
  });

  test("a small upload is one POST with the type, upsert and database", async () => {
    const fetch = routed(() => stored("a/b c.txt", 5));
    const db = createClient({ url, key, fetch }).database("other");
    const seen: number[] = [];
    await db.storage.from("my bucket").upload("a/b c.txt", "hello", {
      contentType: "text/plain",
      upsert: true,
      onProgress: (p) => seen.push(p.loaded),
    });
    expect(fetch.calls).toHaveLength(1);
    const c = fetch.calls[0]!;
    expect(c.method).toBe("POST");
    expect(c.url.pathname).toBe("/storage/v1/object/my%20bucket/a/b%20c.txt");
    expect(c.url.searchParams.get("upsert")).toBe("true");
    expect(c.url.searchParams.get("database")).toBe("other");
    expect(c.headers.get("authorization")).toBe(`Bearer ${key}`);
    expect(c.headers.get("content-type")).toBe("text/plain");
    expect(c.headers.has("chelabase-user")).toBe(false);
    expect(await new Response(c.body).text()).toBe("hello");
    expect(seen.at(-1)).toBe(5);
  });

  test("a File's own type is used when none is given", async () => {
    const fetch = routed(() => stored("x.png", 3));
    const db = createClient({ url, key, fetch });
    await db.storage.from("b").upload("x.png", new Blob([new Uint8Array(3)], { type: "image/png" }));
    expect(fetch.calls[0]!.headers.get("content-type")).toBe("image/png");
    expect(fetch.calls[0]!.url.searchParams.has("upsert")).toBe(false);
  });

  test("a part answering 5xx is retried up to the limit, then fails", async () => {
    const { fetch } = multipartServer((n) => (n === 1 ? fail(503, "busy") : undefined));
    const db = createClient({ url, key, fetch });
    const blob = new Blob([new Uint8Array(6 * MiB)]);
    const err = await db.storage
      .from("b")
      .upload("big.bin", blob, { partSize: 5 * MiB, multipartThreshold: 5 * MiB, retries: 2, retryDelayMs: 0 })
      .catch((e: unknown) => e);
    expect(err).toBeInstanceOf(StorageError);
    expect((err as StorageError).code).toBe("unavailable");
    expect(partCalls(fetch.calls)).toEqual([1, 1, 1]);
  });

  test("a part's 5xx then success goes on", async () => {
    const { fetch } = multipartServer((n, attempt) => (n === 2 && attempt === 1 ? fail(502, "storage_error") : undefined));
    const db = createClient({ url, key, fetch });
    const blob = new Blob([new Uint8Array(11 * MiB)]);
    await db.storage.from("b").upload("big.bin", blob, { partSize: 5 * MiB, multipartThreshold: 5 * MiB, retryDelayMs: 0 });
    expect(partCalls(fetch.calls)).toEqual([1, 2, 2, 3]);
  });

  test("an aborted signal stops the upload between parts", async () => {
    const ctl = new AbortController();
    const { fetch } = multipartServer((n) => {
      if (n === 1) ctl.abort();
      return undefined;
    });
    const db = createClient({ url, key, fetch });
    const blob = new Blob([new Uint8Array(11 * MiB)]);
    await expect(
      db.storage.from("b").upload("big.bin", blob, { partSize: 5 * MiB, multipartThreshold: 5 * MiB, signal: ctl.signal }),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(partCalls(fetch.calls)).toEqual([1]);
  });

  test("an already aborted signal sends nothing", async () => {
    const fetch = routed(() => stored("a", 1));
    const db = createClient({ url, key, fetch });
    await expect(db.storage.from("b").upload("a", "x", { signal: AbortSignal.abort() })).rejects.toMatchObject({
      name: "AbortError",
    });
    expect(fetch.calls).toHaveLength(0);
  });

  test("a part size outside 5 to 100 MiB is refused before any request", async () => {
    const fetch = routed(() => stored("a", 1));
    const db = createClient({ url, key, fetch });
    const blob = new Blob([new Uint8Array(10)]);
    await expect(db.storage.from("b").upload("a", blob, { partSize: MiB })).rejects.toThrow(/partSize/);
    expect(fetch.calls).toHaveLength(0);
  });
});

describe("errors", () => {
  test.each([
    [404, "object_not_found", "not_found"],
    [403, "not_allowed", "forbidden"],
    [413, "too_large", "too_large"],
    [415, "mime_not_allowed", "unsupported_type"],
    [409, "conflict", "conflict"],
    [409, "write_once", "write_once"],
    [409, "referenced", "referenced"],
    [400, "invalid_request", "invalid_request"],
    [408, "request_timeout", "timeout"],
    [401, "token_expired", "unauthorized"],
    [503, "busy", "unavailable"],
  ])("AC3: HTTP %i %s is StorageError code %s", async (status, server, code) => {
    const fetch = routed(() => fail(status, server));
    const db = createClient({ url, key, fetch });
    const err = await db.storage
      .from("b")
      .upload("a.txt", "x")
      .catch((e: unknown) => e);
    expect(err).toBeInstanceOf(StorageError);
    expect(err).toMatchObject({ status, code, serverCode: server, message: `${server} happened` });
  });

  test("a status without the error body still gets its code", async () => {
    const fetch = routed(() => new Response("<html>too big</html>", { status: 413 }));
    const db = createClient({ url, key, fetch });
    await expect(db.storage.from("b").upload("a", "x")).rejects.toMatchObject({ code: "too_large", status: 413 });
  });

  test("a redirect is never followed", async () => {
    const fetch = routed(() => new Response(null, { status: 302, headers: { location: "http://evil" } }));
    const db = createClient({ url, key, fetch });
    await expect(db.storage.from("b").list()).rejects.toMatchObject({ code: "internal" });
  });
});

describe("objects", () => {
  test("AC6: getPublicUrl is a stable URL and sends nothing", () => {
    const fetch = routed(() => json({}));
    const db = createClient({ url: `${url}/`, key, fetch, storage: { branch } });
    const a = db.storage.from("avatars").getPublicUrl("u 1/me.png");
    expect(a).toBe(`${url}/storage/v1/object/public/${branch}/avatars/u%201/me.png`);
    expect(db.storage.from("avatars").getPublicUrl("u 1/me.png")).toBe(a);
    expect(db.database("other").storage.from("avatars").getPublicUrl("x")).toBe(
      `${url}/storage/v1/object/public/${branch}/avatars/x?database=other`,
    );
    expect(db.storage.from("avatars").getPublicUrl("x", { download: true })).toBe(
      `${url}/storage/v1/object/public/${branch}/avatars/x?download=`,
    );
    expect(fetch.calls).toHaveLength(0);
  });

  test("getPublicUrl without a branch throws", () => {
    const db = createClient({ url, key, fetch: routed(() => json({})) });
    expect(() => db.storage.from("a").getPublicUrl("x")).toThrow(/branch/);
  });

  test("download sends one Range and returns the response", async () => {
    const fetch = routed(() => new Response("ell", { status: 206, headers: { "content-range": "bytes 1-3/5" } }));
    const db = createClient({ url, key, fetch });
    const res = await db.storage.from("b").download("a.txt", { range: { start: 1, end: 3 } });
    expect(res.status).toBe(206);
    expect(await res.text()).toBe("ell");
    const c = fetch.calls[0]!;
    expect(c.method).toBe("GET");
    expect(c.url.pathname).toBe("/storage/v1/object/b/a.txt");
    expect(c.headers.get("range")).toBe("bytes=1-3");
  });

  test("download of a missing object is not_found", async () => {
    const fetch = routed(() => fail(404, "object_not_found"));
    const db = createClient({ url, key, fetch });
    await expect(db.storage.from("b").download("a.txt")).rejects.toMatchObject({ code: "not_found" });
  });

  test("list maps the options and returns the page with its cursor", async () => {
    const item = { name: "a/", folder: true, size: null, etag: null, content_type: null, updated_at: null };
    const fetch = routed(() => json({ items: [item], next_cursor: "c2" }));
    const db = createClient({ url, key, fetch });
    const page = await db.storage
      .from("b")
      .list({ prefix: "a", folders: true, cursor: "c1", limit: 10, search: "x", sortBy: "updated_at", order: "desc" });
    expect([...page]).toEqual([item]);
    expect(page.nextCursor).toBe("c2");
    const c = fetch.calls[0]!;
    expect(c.method).toBe("POST");
    expect(c.url.pathname).toBe("/storage/v1/object/list/b");
    expect(JSON.parse(c.body as string)).toEqual({
      prefix: "a",
      delimiter: "/",
      cursor: "c1",
      limit: 10,
      search: "x",
      sort_by: "updated_at",
      order: "desc",
    });
  });

  test("remove, move and copy", async () => {
    const fetch = routed((c) => {
      if (c.method === "DELETE") return json({ deleted: ["a"] });
      if (c.url.pathname.endsWith("/move")) return new Response(null, { status: 204 });
      return json({ bucket: "c", name: "z", version_id: "v2" });
    });
    const b = createClient({ url, key, fetch }).storage.from("b");
    expect(await b.remove(["a", "missing"])).toEqual(["a"]);
    expect(await b.remove("a")).toEqual(["a"]);
    await b.move("a", "y", { upsert: true });
    expect(await b.copy("y", "z", { toBucket: "c" })).toEqual({ bucket: "c", name: "z", version_id: "v2" });
    const [del, del1, mv, cp] = fetch.calls;
    expect(del!.url.pathname).toBe("/storage/v1/object/b");
    expect(JSON.parse(del!.body as string)).toEqual({ names: ["a", "missing"] });
    expect(JSON.parse(del1!.body as string)).toEqual({ names: ["a"] });
    expect(mv!.url.pathname).toBe("/storage/v1/object/move");
    expect(mv!.url.searchParams.get("upsert")).toBe("true");
    expect(JSON.parse(mv!.body as string)).toEqual({ bucket: "b", from: "a", to: "y" });
    expect(JSON.parse(cp!.body as string)).toEqual({ bucket: "b", from: "y", to: "z", to_bucket: "c" });
  });

  test("exists and info read HEAD", async () => {
    const fetch = routed((c) =>
      c.url.pathname.endsWith("/there")
        ? new Response(null, {
            status: 200,
            headers: { "content-length": "42", "content-type": "image/png", etag: '"e9"' },
          })
        : new Response(null, { status: 404 }),
    );
    const b = createClient({ url, key, fetch }).storage.from("b");
    expect(await b.exists("there")).toBe(true);
    expect(await b.exists("gone")).toBe(false);
    expect(await b.info("there")).toEqual({ size: 42, contentType: "image/png", etag: "e9" });
    await expect(b.info("gone")).rejects.toMatchObject({ code: "not_found" });
    expect(fetch.calls.every((c) => c.method === "HEAD")).toBe(true);
  });

  test("signed URLs are absolute; signed upload URLs carry upsert", async () => {
    const fetch = routed((c) =>
      json({
        signed_url: `${c.url.pathname.replace("/object/", "/object/").replace(/^\/storage\/v1/, "/storage/v1")}?token=t.k`,
        expires_at: 99,
      }),
    );
    const b = createClient({ url, key, fetch }).storage.from("b");
    const s = await b.createSignedUrl("a.txt", { expiresIn: 60 });
    expect(s).toEqual({ path: "a.txt", signedUrl: `${url}/storage/v1/object/sign/b/a.txt?token=t.k`, expiresAt: 99 });
    expect(JSON.parse(fetch.calls[0]!.body as string)).toEqual({ expires_in: 60 });
    const many = await b.createSignedUrls(["x", "y"]);
    expect(many.map((m) => m.path)).toEqual(["x", "y"]);
    const d = await b.createSignedUrl("a.txt", { download: "report.txt" });
    expect(d.signedUrl).toBe(`${url}/storage/v1/object/sign/b/a.txt?token=t.k&download=report.txt`);
    const u = await b.createSignedUploadUrl("new.txt", { upsert: true, expiresIn: 30 });
    expect(u).toEqual({
      path: "new.txt",
      signedUrl: `${url}/storage/v1/object/upload/sign/b/new.txt?token=t.k`,
      token: "t.k",
      expiresAt: 99,
    });
    expect(JSON.parse(fetch.calls.at(-1)!.body as string)).toEqual({ expires_in: 30, upsert: true });
  });
});

describe("buckets", () => {
  test("list, create, update and delete", async () => {
    const bucket = { id: "u", name: "b", public: false, write_once: false, allowed_mime_types: null, file_size_limit: null };
    const fetch = routed((c) => {
      if (c.method === "GET") return json([bucket]);
      if (c.method === "POST") return json(bucket, 201);
      return new Response(null, { status: 204 });
    });
    const s = createClient({ url, key: "chk_sec_abc", fetch }).storage;
    expect(await s.listBuckets()).toEqual([bucket]);
    expect(await s.createBucket({ name: "b", public: true, file_size_limit: 10 })).toEqual(bucket);
    await s.updateBucket("b", { write_once: true });
    await s.deleteBucket("b");
    expect(fetch.calls.map((c) => `${c.method} ${c.url.pathname}`)).toEqual([
      "GET /storage/v1/bucket",
      "POST /storage/v1/bucket",
      "PATCH /storage/v1/bucket/b",
      "DELETE /storage/v1/bucket/b",
    ]);
    expect(JSON.parse(fetch.calls[1]!.body as string)).toEqual({ name: "b", public: true, file_size_limit: 10 });
    expect(JSON.parse(fetch.calls[2]!.body as string)).toEqual({ write_once: true });
  });

  test("a deleteBucket of a bucket holding objects is a conflict", async () => {
    const fetch = routed(() => fail(409, "bucket_not_empty"));
    const s = createClient({ url, key, fetch }).storage;
    await expect(s.deleteBucket("b")).rejects.toMatchObject({ code: "conflict", serverCode: "bucket_not_empty" });
  });
});

describe("app users", () => {
  test("a caller-owned access token goes as chelabase-user", async () => {
    const fetch = routed(() => json({ items: [], next_cursor: null }));
    const db = createClient({ url, key, fetch, accessToken: () => "tok" });
    await db.storage.from("b").list();
    expect(fetch.calls[0]!.headers.get("chelabase-user")).toBe("tok");
  });
});
