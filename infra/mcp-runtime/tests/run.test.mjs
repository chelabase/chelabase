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

  test("a body over the host's 1 MiB reaches the host; an envelope over 4 MiB throws", async () => {
    host.requests.length = 0;
    const code = `async () => {
      const r = await chelabase.request({ method: "POST", path: "/v1/x", body: { pad: "x".repeat(2 << 20) } });
      // 1.5 M characters, 4.5 MiB of UTF-8: the cap counts bytes.
      const big = await chelabase.request({ method: "POST", path: "/v1/x", body: { pad: "\u20ac".repeat(1536 * 1024) } })
        .then(() => "sent", (e) => e.message);
      return { status: r.status, echoed: r.body.echo.body.pad.length, big };
    }`;
    const { body } = await runCode(rt, job(code, { kind: "execute", host }));
    assert.equal(body.ok, true, JSON.stringify(body).slice(0, 2000));
    assert.equal(body.result.status, 200);
    assert.equal(body.result.echoed, 2 << 20);
    assert.match(body.result.big, /over 4 MiB/);
    assert.equal(host.requests.length, 1);
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

  test("execute reports the calls whose answer the code never got", async () => {
    const reply = host.reply;
    try {
      // Awaited calls got their answers: none is reported. Each call carries its number.
      host.requests.length = 0;
      const awaited = `async () => {
        await chelabase.request({ method: "GET", path: "/v1/a" });
        await chelabase.request({ method: "GET", path: "/v1/b" });
        return 1;
      }`;
      const first = await runCode(rt, job(awaited, { kind: "execute", host }));
      assert.equal(first.body.ok, true, JSON.stringify(first.body));
      assert.deepEqual(first.body.unreceived, []);
      assert.deepEqual(host.requests.map((q) => q.headers["chelabase-call"]), ["1", "2"]);

      // An un-awaited call the host is still answering when the code returns.
      host.requests.length = 0;
      host.replyDelayMs = 1000;
      const detached = `async () => {
        chelabase.request({ method: "POST", path: "/v1/x", body: { a: 1 } });
        await new Promise((r) => setTimeout(r, 200));
        return 2;
      }`;
      const second = await runCode(rt, job(detached, { kind: "execute", host }));
      assert.equal(second.body.ok, true, JSON.stringify(second.body));
      assert.equal(second.body.result, 2);
      assert.deepEqual(second.body.unreceived, [1]);
      assert.equal(host.requests.length, 1, "the host received the call");
      host.replyDelayMs = 0;

      // An answer the runner can't read: the code gets a throw, and the call is reported,
      // also when the code itself then throws.
      host.reply = () => ({ not: "an answer" });
      const unread = `async () => {
        const e = await chelabase.request({ method: "GET", path: "/v1/a" }).then(() => "read", (e) => e.message);
        await chelabase.request({ method: "GET", path: "/v1/b" }).catch(() => {});
        if (e === "read") return e;
        throw new Error(e);
      }`;
      const third = await runCode(rt, job(unread, { kind: "execute", host }));
      assert.equal(third.body.ok, false, JSON.stringify(third.body));
      assert.equal(third.body.error.code, "threw");
      assert.deepEqual(third.body.unreceived, [1, 2]);

      // A run that times out reports its call in flight.
      host.reply = reply;
      host.replyDelayMs = 5000;
      const hung = `async () => chelabase.request({ method: "GET", path: "/v1/a" })`;
      const hungJob = job(hung, { kind: "execute", host, timeout_ms: 500 });
      const pending = runCode(rt, hungJob);
      await new Promise((r) => setTimeout(r, 100));
      // Its exec_id is taken while it runs.
      assert.equal((await runCode(rt, { ...hungJob, code: "async () => 1" })).status, 409);
      const fourth = await pending;
      assert.equal(fourth.body.error?.code, "timeout", JSON.stringify(fourth.body));
      assert.deepEqual(fourth.body.unreceived, [1]);

      // Search runs make no calls and report none.
      const search = await runCode(rt, job("async () => 1", { host }));
      assert.equal(search.body.unreceived, undefined);
    } finally {
      host.reply = reply;
      host.replyDelayMs = 0;
    }
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

  test("a run that ends its own isolate", async () => {
    // workerd gives the runner the same error for abortIsolate() as for a promise nothing can
    // settle, so the answer is a timeout whose message names both.
    const code = `async () => { const w = await import("cloudflare:workers"); w.abortIsolate("bye") }`;
    const t0 = Date.now();
    const { body } = await runCode(rt, job(code, { host, timeout_ms: 5000 }));
    assert.equal(body.ok, false, JSON.stringify(body));
    assert.equal(body.error.code, "timeout", JSON.stringify(body));
    assert.match(body.error.message, /ended its own isolate/);
    assert.ok(Date.now() - t0 < 4000, `took ${Date.now() - t0}ms`);
    assert.equal((await runCode(rt, job("async () => 1", { host }))).body.result, 1);
  });

  test("a host that never sends the spec ends the run with 502", async () => {
    const saved = host.specText;
    host.specText = JSON.stringify({ paths: { "/stalled": {} } });
    host.stallSpec = true;
    try {
      const t0 = Date.now();
      const { status, body } = await runCode(rt, job("async () => 1", { host }), { timeout: 30_000 });
      assert.equal(status, 502, JSON.stringify(body));
      assert.match(body.error, /did not send the spec/);
      assert.ok(Date.now() - t0 < 15_000, `took ${Date.now() - t0}ms`);
    } finally {
      host.stallSpec = false;
      host.specText = saved;
    }
    // A later run with a host that answers gets the spec.
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
