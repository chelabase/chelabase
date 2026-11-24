// Memory: the supervisor's long-lived isolate levels off over many invocations and probes
// (config.capnp, `autogates`).
//
//   node --test infra/functions-runtime/tests/
import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";
import { invoke, manifest, startBundleStore, startRuntime } from "./harness.mjs";

const DB = "db_0123456789abcdef";

describe("functions runtime: memory", () => {
  let store;
  let rt;
  before(async () => {
    store = await startBundleStore();
    rt = await startRuntime(store);
  });
  after(async () => {
    await rt?.stop();
    await store?.close();
  });

  test("memory levels off over many invocations and probes", async (t) => {
    // What a request leaves in the supervisor's isolate is about 10 KiB of C++ memory, freed
    // only by a major GC. With the autogate V8 knows about it and collects in time: memory
    // settles at about 155 MiB over the first 20000-30000 requests and stays there. Without it
    // memory climbs about 50 MiB every 5000 requests until the first major GC, after some 25000
    // (near 300 MiB). So: a warm-up, then two back-to-back windows that end at 20000 requests,
    // well before that late GC; the second must grow by less than the bound. Each window is
    // invocations of a loaded bundle (the steady load) and the watchdog's probe (an idle
    // runtime's only load) in the same proportion.
    const WARM = 2;
    const INVOKES = 4000;
    const PROBES = 1000;
    // With the gate the second window grows by 10-14 MiB (still settling); without it by 48.
    const LEVEL_MIB = 24;
    const sha = store.add(DB, manifest("hello-a"));
    const call = async () => {
      const res = await invoke(rt, DB, sha);
      assert.equal(await res.text(), "A");
    };
    const window = async () => {
      for (let i = 0; i < INVOKES; i++) await call();
      await rt.probe(PROBES);
      // The lowest of a few readings, each after one more request.
      let low = Infinity;
      for (let i = 0; i < 4; i++) {
        await call();
        low = Math.min(low, await rt.anonBytes());
      }
      return low;
    };
    const mib = (b) => (b / (1 << 20)).toFixed(1);
    const per = INVOKES + PROBES;
    let warm;
    for (let i = 0; i < WARM; i++) warm = await window();
    const first = await window();
    const second = await window();
    const grew = [(first - warm) / (1 << 20), (second - first) / (1 << 20)];
    const sign = (x) => `${x >= 0 ? "+" : ""}${x.toFixed(1)}`;
    const report = `anon memory after ${WARM * per} warm-up requests ${mib(warm)} MiB, after ${per} more ${mib(first)} MiB (${sign(grew[0])}), after ${per} more ${mib(second)} MiB (${sign(grew[1])}; ${((second - first) / per / 1024).toFixed(1)} KiB a request)`;
    t.diagnostic(report);
    console.log(`memory: ${report}`);
    assert.equal((await rt.inspect()).RestartCount, 0, "no restart during the runs");
    assert.ok(grew[1] < LEVEL_MIB, `memory is still growing: ${report}`);
  });
});
