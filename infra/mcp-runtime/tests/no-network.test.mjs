// Loaded code has no network: fetch and connect fail inside the sandbox for every kind of
// address, and nothing reaches the stub host (which every name below resolves to) except the
// runner's own spec fetch.
//
//   node --test infra/mcp-runtime/tests/
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { HOST_NAMES, job, runCode, startHost, startRuntime } from "./harness.mjs";

describe("mcp runtime: no network", () => {
  let host;
  let rt;
  before(async () => {
    host = await startHost();
    rt = await startRuntime(host);
  });
  after(async () => {
    await rt?.stop();
    await host?.close();
  });

  // Runs `probe(target)` for every target in both kinds; each must reject.
  async function allRefused(probe, targets) {
    for (const kind of ["search", "execute"]) {
      const code = `async () => {
        const out = {};
        for (const t of ${JSON.stringify(targets)}) {
          out[t] = await Promise.resolve().then(() => (${probe})(t)).then(() => "reached", (e) => "refused: " + e.message);
        }
        return out;
      }`;
      const { body } = await runCode(rt, job(code, { kind, host }), { timeout: 30_000 });
      assert.equal(body.ok, true, JSON.stringify(body));
      for (const t of targets) assert.match(body.result[t], /^refused: /, `${kind} ${t}: ${body.result[t]}`);
    }
  }

  test("fetch to a public, private and loopback address fails", async () => {
    await allRefused(`(u) => fetch(u)`, [
      "http://1.1.1.1/",
      "https://example.com/",
      "http://10.0.0.1/",
      "http://192.168.1.1/",
      "http://172.17.0.1/",
      "http://169.254.169.254/latest/meta-data/",
      "http://127.0.0.1:8080/run",
      "http://127.0.0.1:9090/",
      "http://localhost:8080/",
      "http://[::1]:8080/",
    ]);
    // Request objects and other fetch forms too.
    await allRefused(`(u) => fetch(new Request(u, { method: "POST", body: "x" }))`, ["http://1.1.1.1/"]);
  });

  test("connect fails", async () => {
    await allRefused(`async (a) => { const { connect } = await import("cloudflare:sockets"); const s = connect(a); await s.opened; }`, [
      "1.1.1.1:80",
      "10.0.0.1:5432",
      "127.0.0.1:8080",
      `mcp-internal:${host.port}`,
    ]);
  });

  test("the host address is not reachable by fetch", async () => {
    host.requests.length = 0;
    await allRefused(`(u) => fetch(u)`, HOST_NAMES.flatMap((n) => [
      `http://${n}:${host.port}/internal/spec`,
      `http://${n}:${host.port}/internal/exec/x/request`,
    ]));
    // Caches, WebSocket and the like have no route out either.
    await allRefused(`async (u) => { const ws = new WebSocket(u); await new Promise((ok, no) => { ws.onopen = ok; ws.onerror = () => no(new Error("ws failed")); }); }`, [
      `ws://mcp-internal:${host.port}/`,
    ]);
    // Only the runner's own spec fetches reached the stub.
    assert.deepEqual(host.requests.filter((r) => r.line !== "GET /internal/spec").map((r) => r.line), []);
  });
});
