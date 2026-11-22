// The MCP sandbox runtime's /run against the built image
// (`docker buildx bake -f infra/images/docker-bake.hcl --load mcp-runtime`).
//
//   node --test infra/mcp-runtime/tests/
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { job, runCode, startHost, startRuntime } from "./harness.mjs";

const SPEC = {
  paths: {
    "/v1/databases": { get: { summary: "List databases" } },
    "/v1/databases/{id}": { get: { summary: "Get a database" }, delete: { summary: "Delete a database" } },
    "/v1/branches": { get: { summary: "List branches" } },
  },
};

describe("mcp runtime /run", () => {
  let host;
  let rt;
  before(async () => {
    host = await startHost({ spec: SPEC });
    rt = await startRuntime(host);
  });
  after(async () => {
    await rt?.stop();
    await host?.close();
  });

  test("search returns the filtered value", async () => {
    const code = `async () => Object.keys(spec.paths).filter((p) => p.includes("databases"))`;
    const { status, body } = await runCode(rt, job(code, { host }));
    assert.equal(status, 200, JSON.stringify(body));
    assert.equal(body.ok, true, JSON.stringify(body));
    assert.deepEqual(body.result, ["/v1/databases", "/v1/databases/{id}"]);
    // The spec is fetched once per hash, not per run.
    await runCode(rt, job("async () => 1", { host }));
    assert.equal(host.requests.filter((r) => r.line === "GET /internal/spec").length, 1);
  });

  test("console output comes back as logs", async () => {
    const { body } = await runCode(rt, job(`async () => { console.log("hi", 1); console.warn({a: 2}); return 0 }`, { host }));
    assert.equal(body.ok, true, JSON.stringify(body));
    assert.equal(body.logs.length, 2);
    assert.match(body.logs[0], /hi 1/);
    assert.match(body.logs[1], /"a":2/);
  });

  test("search sees spec but no chelabase, and env is empty", async () => {
    const code = `async () => {
      const { env } = await import("cloudflare:workers");
      return { chelabase: typeof chelabase, spec: typeof spec, env: Object.keys(env) };
    }`;
    const { body } = await runCode(rt, job(code, { host }));
    assert.deepEqual(body, { ok: true, result: { chelabase: "undefined", spec: "object", env: [] }, logs: [] });
  });

  test("execute reaches only the host callback", async () => {
    host.requests.length = 0;
    const execId = "exec_only_callback";
    const code = `async () => {
      const { env } = await import("cloudflare:workers");
      const out = { env: Object.keys(env), spec: typeof spec };
      out.res = await chelabase.request({ method: "GET", path: "/v1/databases", query: { limit: 5 }, headers: { "x-test": "1" } });
      // Tries to pick another path, host or id: they only travel inside the body.
      out.odd = await chelabase.request({ method: "POST", path: "/../../internal/spec", body: { a: 1 } });
      out.badPath = await chelabase.request({ method: "GET", path: "http://evil.example/x" }).then(() => "accepted", (e) => e.message);
      out.fetchOnStub = await Promise.resolve().then(() => chelabase.fetch("http://mcp-internal/internal/spec")).then(() => "fetched", (e) => "refused");
      out.connectOnStub = await Promise.resolve().then(() => chelabase.connect("mcp-internal:8080").opened).then(() => "connected", (e) => "refused");
      return out;
    }`;
    const { body } = await runCode(rt, job(code, { kind: "execute", host, exec_id: execId }));
    assert.equal(body.ok, true, JSON.stringify(body));
    const r = body.result;
    assert.deepEqual(r.env, ["chelabase"]);
    assert.equal(r.spec, "undefined");
    assert.equal(r.res.status, 200);
    assert.equal(r.res.ok, true);
    assert.deepEqual(r.res.body.echo, { method: "GET", path: "/v1/databases", query: { limit: 5 }, headers: { "x-test": "1" } });
    assert.equal(r.odd.body.echo.path, "/../../internal/spec");
    assert.notEqual(r.badPath, "accepted");
    assert.equal(r.fetchOnStub, "refused");
    assert.equal(r.connectOnStub, "refused");
    const lines = host.requests.map((q) => q.line);
    assert.deepEqual(lines, [`POST /internal/exec/${execId}/request`, `POST /internal/exec/${execId}/request`]);
  });

  test("the runner's caller headers never reach the host or the sandbox", async () => {
    host.requests.length = 0;
    const res = await fetch(new URL("/run", rt.url), {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer member-token", cookie: "s=1" },
      body: JSON.stringify(job(`async () => (await chelabase.request({ method: "GET", path: "/v1/x" })).status`, { kind: "execute", host })),
    });
    assert.equal((await res.json()).result, 200);
    const sent = host.requests[0];
    assert.equal(sent.headers.authorization, undefined);
    assert.equal(sent.headers.cookie, undefined);
  });

  test("a run cannot see an earlier run's globals", async () => {
    const first = await runCode(rt, job(`async () => { globalThis.leak = 42; spec.paths.added = 1; return "set" }`, { host }));
    assert.equal(first.body.result, "set");
    const second = await runCode(rt, job(`async () => [typeof globalThis.leak, "added" in spec.paths]`, { host }));
    assert.deepEqual(second.body.result, ["undefined", false]);
    const exec = await runCode(rt, job(`async () => typeof globalThis.leak`, { kind: "execute", host }));
    assert.equal(exec.body.result, "undefined");
  });

  test("timeout", async () => {
    // A pending timer, and a promise nothing can settle (workerd may end that one sooner).
    for (const code of [`async () => new Promise((r) => setTimeout(r, 60000))`, `async () => new Promise(() => {})`]) {
      const t0 = Date.now();
      const { body } = await runCode(rt, job(code, { host, timeout_ms: 300 }));
      assert.equal(body.ok, false, code);
      assert.equal(body.error.code, "timeout", `${code}: ${JSON.stringify(body)}`);
      assert.ok(Date.now() - t0 < 5000);
    }
    // The runtime still answers.
    assert.equal((await runCode(rt, job("async () => 1", { host }))).body.result, 1);
  });

  test("throwing code", async () => {
    const { body } = await runCode(rt, job(`async () => { throw new Error("boom") }`, { host }));
    assert.equal(body.ok, false);
    assert.equal(body.error.code, "threw");
    assert.equal(body.error.message, "boom");
  });

  test("a returned function is not serializable", async () => {
    for (const code of [`async () => () => 1`, `async () => ({ f() {} })`, `async () => { const a = {}; a.a = a; return a }`, `async () => 1n`]) {
      const { body } = await runCode(rt, job(code, { host }));
      assert.equal(body.ok, false, code);
      assert.equal(body.error.code, "not_serializable", code);
    }
  });

  test("unparsable code", async () => {
    for (const code of [`async () => {`, `42`, `"not a function"`, `async () => 1); (`]) {
      const { status, body } = await runCode(rt, job(code, { host }));
      assert.equal(status, 200, code);
      assert.equal(body.ok, false, code);
      assert.equal(body.error.code, "invalid_code", `${code}: ${JSON.stringify(body)}`);
    }
  });

  test("oversized code is refused", async () => {
    const big = `async () => "${"x".repeat(256 * 1024)}"`;
    const { status } = await runCode(rt, job(big, { host }));
    assert.equal(status, 413);
    const fits = `async () => ${"1".repeat(1000)}`.length;
    assert.ok(fits < 256 * 1024);
  });

  test("malformed requests are refused", async () => {
    assert.equal((await runCode(rt, "{")).status, 400);
    assert.equal((await runCode(rt, { ...job("async () => 1", { host }), kind: "other" })).status, 400);
    assert.equal((await runCode(rt, { ...job("async () => 1", { host }), exec_id: "../x" })).status, 400);
    assert.equal((await runCode(rt, { ...job("async () => 1", { host }), timeout_ms: 0 })).status, 400);
    assert.equal((await runCode(rt, { ...job("async () => 1", { host }), spec_hash: "nope" })).status, 400);
    // A spec whose bytes don't hash to spec_hash is never used.
    const { status } = await runCode(rt, { ...job("async () => 1", { host }), spec_hash: "0".repeat(64) });
    assert.equal(status, 502);
  });
});
