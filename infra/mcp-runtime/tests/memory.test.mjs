// Memory: a run that allocates without bound ends and the next run works, whether it fills the
// V8 heap (workerd aborts on the heap cap, the watchdog exits with it and the restart policy
// starts it again) or ArrayBuffers outside it (the container's memory limit, swap included,
// kills workerd); and memory levels off over many runs.
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

  // TODO: workerd itself keeps ~20-24 KiB a run (1.20261023.1 and 1.20261119.1 alike, even for
  // `async () => 1`), so this fails until that is fixed or the runtime recycles workerd; how is
  // a human's call (mcp-core m3, #347). `todo` reports the failure without failing the suite.
  test("memory levels off over many runs", { todo: "workerd keeps ~20-24 KiB a run (#347)" }, async (t) => {
    // Each run holds ~2 MB, so a kept isolate would show at once. Past warm-up, two back-to-back
    // windows of runs: memory may move in the first, but the second must stay level. A steady
    // per-run leak (even a few KB a run) grows the second window as much as the first and fails.
    const WARM = 200;
    const WINDOW = 1000;
    const LEVEL_MIB = 4;
    const code = `async () => { const big = new Array(250000).fill(0).map((_, i) => i); return big.length }`;
    const runs = async (n) => {
      for (let i = 0; i < n; i++) {
        const { body } = await runCode(rt, job(code, { host }));
        assert.equal(body.result, 250000, JSON.stringify(body));
      }
      return rt.anonBytes();
    };
    const mib = (b) => (b / (1 << 20)).toFixed(1);
    const warm = await runs(WARM);
    const first = await runs(WINDOW);
    const second = await runs(WINDOW);
    const grew = [(first - warm) / (1 << 20), (second - first) / (1 << 20)];
    const report = `anon memory after ${WARM} warm-up runs ${mib(warm)} MiB, after ${WINDOW} more ${mib(first)} MiB (${grew[0] >= 0 ? "+" : ""}${grew[0].toFixed(1)}), after ${WINDOW} more ${mib(second)} MiB (${grew[1] >= 0 ? "+" : ""}${grew[1].toFixed(1)}; ${((second - first) / WINDOW / 1024).toFixed(1)} KiB a run)`;
    t.diagnostic(report);
    console.log(`memory: ${report}`);
    assert.equal((await rt.inspect()).RestartCount, 0, "no restart during the runs");
    assert.ok(grew[1] < LEVEL_MIB, `memory is still growing: ${report}`);
  });

  const unbounded = async (t, label, code) => {
    const restartsBefore = (await rt.inspect()).RestartCount;
    const t0 = Date.now();
    const outcome = await runCode(rt, job(code, { host, timeout_ms: 30_000 }), { timeout: 60_000 }).then(
      (r) => `answered ${r.status} ${JSON.stringify(r.body)}`,
      (e) => `connection ended: ${e.cause?.code ?? e.message}`,
    );
    t.diagnostic(`${label}: ${outcome}`);
    console.log(`${label}: ${outcome}`);
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
    console.log(`${label}: answering again after ${Date.now() - t0}ms, restarts ${restarts}`);
    assert.ok(restarts >= 1 || /"ok":false/.test(outcome), outcome);
  };

  test("unbounded heap allocation ends and the next run works", (t) =>
    unbounded(t, "unbounded heap allocation", `async () => { const a = []; for (;;) a.push(new Array(1e6).fill(Math.random())) }`));

  test("unbounded ArrayBuffer allocation ends and the next run works", (t) =>
    // 64 MiB at a time, every page written so it counts; the V8 heap cap doesn't see it.
    unbounded(t, "unbounded ArrayBuffer allocation", `async () => { const a = []; for (;;) a.push(new Uint8Array(64 << 20).fill(1)) }`));
});
