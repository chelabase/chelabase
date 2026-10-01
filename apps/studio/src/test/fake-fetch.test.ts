import { describe, expect, it } from "vitest";
import { fakeFetch, jsonResponse, sequence } from "./fake-fetch";

describe("fakeFetch", () => {
  it("matches by method and pathname", async () => {
    const f = fakeFetch({
      "GET /a": () => jsonResponse({ ok: "get" }),
      "POST /a": () => jsonResponse({ ok: "post" }, 201),
    });
    expect(await (await f("http://x.test/a")).json()).toEqual({ ok: "get" });
    const res = await f("http://x.test/a", { method: "POST" });
    expect(res.status).toBe(201);
    expect(await res.json()).toEqual({ ok: "post" });
  });

  it("ignores origin and query string, but needs the exact pathname", async () => {
    const f = fakeFetch({ "GET /a": () => jsonResponse(1) });
    expect((await f("http://other.test/a?x=1")).status).toBe(200);
    expect((await f("http://x.test/a/b")).status).toBe(404);
  });

  it("supports async handlers and receives the Request", async () => {
    const f = fakeFetch({
      "POST /a": async (req) => jsonResponse(await req.json()),
    });
    const res = await f("http://x.test/a", { method: "POST", body: JSON.stringify({ n: 1 }) });
    expect(await res.json()).toEqual({ n: 1 });
  });

  it("answers unmatched routes with our 404 body", async () => {
    const f = fakeFetch({});
    const res = await f("http://x.test/nope?q=1", { method: "DELETE" });
    expect(res.status).toBe(404);
    expect(await res.json()).toEqual({
      error: { code: "not_found", message: "no fake route for DELETE /nope" },
    });
  });

  it("records calls in order", async () => {
    const f = fakeFetch({ "GET /a": () => jsonResponse(1) });
    await f("http://x.test/a");
    await f(new URL("http://x.test/b"));
    expect(f.calls.map((r) => `${r.method} ${new URL(r.url).pathname}`)).toEqual(["GET /a", "GET /b"]);
  });
});

describe("sequence", () => {
  it("answers in order, then repeats the last answer", async () => {
    const next = sequence(() => jsonResponse({ n: 1 }), () => jsonResponse({ n: 2 }));
    const ns = [];
    for (let i = 0; i < 3; i++) ns.push(((await (await next(new Request("http://x/"))).json()) as { n: number }).n);
    expect(ns).toEqual([1, 2, 2]);
  });
});
