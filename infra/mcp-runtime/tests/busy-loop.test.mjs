// A run in a busy loop stalls workerd's single thread (the pinned workerd enforces no CPU limit
// on loaded workers). The watchdog probes every 2s and kills workerd after 3 misses; the restart
// policy brings it back. This measures the time from the loop starting to /run answering again.
//
//   node --test infra/mcp-runtime/tests/
import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { job, runCode, startHost, startRuntime, waitReady } from "./harness.mjs";

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

test("the watchdog restarts a stalled runtime", async (t) => {
  assert.equal((await runCode(rt, job("async () => 1", { host }))).body.result, 1);
  const restartsBefore = (await rt.inspect()).RestartCount;

  const t0 = Date.now();
  const spinning = runCode(rt, job("async () => { for (;;) {} }", { host, timeout_ms: 1000 }), { timeout: 60_000 }).catch((e) => e);
  await new Promise((r) => setTimeout(r, 300));
  const stalled = await runCode(rt, job("async () => 1", { host }), { timeout: 1000 }).then(() => false, () => true);
  assert.ok(stalled, "the runtime is stalled while the loop runs");

  const deadline = Date.now() + 40_000;
  let restarts = restartsBefore;
  while (Date.now() < deadline && restarts === restartsBefore) {
    await new Promise((r) => setTimeout(r, 250));
    restarts = (await rt.inspect()).RestartCount;
  }
  assert.ok(restarts > restartsBefore, "the watchdog restarted the stuck runtime within 40s");
  const killed = Date.now() - t0;
  assert.match(await rt.logs(), /watchdog: .*unresponsive/);
  await rt.refresh();
  for (;;) {
    try {
      const { body } = await runCode(rt, job("async () => 2", { host }), { timeout: 1000 });
      if (body.result === 2) break;
    } catch {}
    if (Date.now() - t0 > 60_000) assert.fail("the runtime never answered again");
    await new Promise((r) => setTimeout(r, 100));
  }
  const back = Date.now() - t0;
  t.diagnostic(`busy loop -> watchdog kill: ${killed}ms; -> answering again: ${back}ms`);
  console.log(`watchdog restart: killed after ${killed}ms, answering again after ${back}ms`);
  assert.ok(back < 30_000, `answering again within 30s (took ${back}ms)`);
  await spinning;
  await waitReady(rt, 5000);
});
