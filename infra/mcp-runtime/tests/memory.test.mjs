// Memory: a run that allocates without bound ends (workerd aborts on the V8 heap cap, the
// watchdog exits with it and the restart policy starts it again) and the next run works; and
// finished runs' isolates are released rather than kept under their unique ids.
//
//   node --test infra/mcp-runtime/tests/
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { job, runCode, startHost, startRuntime } from "./harness.mjs";

describe("mcp runtime: memory", () => {
  let host;
  let rt;
  before(async () => {
    host = await startHost();
    rt = await startRuntime(host, { restart: "always" });
  });
  after(async () => {
    await rt?.stop();
    await host?.close();
  });

  test("many runs do not accumulate isolates", async (t) => {
    // Each run holds ~2 MB; 500 kept isolates would be over 1 GB. What remains is workerd's own
    // small per-load overhead (tens of KB a run in the pinned version), well under the bound.
    const code = `async () => { const big = new Array(250000).fill(0).map((_, i) => i); return big.length }`;
    for (let i = 0; i < 50; i++) assert.equal((await runCode(rt, job(code, { host }))).body.result, 250000);
    const base = await rt.anonBytes();
    for (let i = 0; i < 500; i++) {
      const { body } = await runCode(rt, job(code, { host }));
      assert.equal(body.result, 250000, JSON.stringify(body));
    }
    const end = await rt.anonBytes();
    const grew = (end - base) / (1 << 20);
    t.diagnostic(`anon memory after 50 runs: ${(base / (1 << 20)).toFixed(1)} MiB; after 500 more: ${(end / (1 << 20)).toFixed(1)} MiB (+${grew.toFixed(1)} MiB)`);
    assert.ok(grew < 64, `memory grew ${grew.toFixed(1)} MiB over 500 runs`);
    assert.equal((await rt.inspect()).RestartCount, 0, "no restart during the runs");
  });

  test("unbounded allocation ends and the next run works", async (t) => {
    const restartsBefore = (await rt.inspect()).RestartCount;
    const t0 = Date.now();
    const code = `async () => { const a = []; for (;;) a.push(new Array(1e6).fill(Math.random())) }`;
    const outcome = await runCode(rt, job(code, { host, timeout_ms: 30_000 }), { timeout: 60_000 }).then(
      (r) => `answered ${r.status} ${JSON.stringify(r.body)}`,
      (e) => `connection ended: ${e.cause?.code ?? e.message}`,
    );
    t.diagnostic(`unbounded allocation: ${outcome}`);
    console.log(`unbounded allocation: ${outcome}`);
    const deadline = Date.now() + 40_000;
    for (;;) {
      try {
        await rt.refresh();
        const { body } = await runCode(rt, job("async () => 3", { host }), { timeout: 1000 });
        if (body.result === 3) break;
      } catch {}
      if (Date.now() > deadline) assert.fail(`the runtime never answered again\n${await rt.logs()}`);
      await new Promise((r) => setTimeout(r, 200));
    }
    const restarts = (await rt.inspect()).RestartCount - restartsBefore;
    const logs = await rt.logs();
    t.diagnostic(`answering again after ${Date.now() - t0}ms; restarts: ${restarts}; heap abort logged: ${/heap out of memory|Reached heap limit/.test(logs)}`);
    console.log(`unbounded allocation: answering again after ${Date.now() - t0}ms, restarts ${restarts}`);
    assert.ok(restarts >= 1 || /"ok":false/.test(outcome), outcome);
  });
});
