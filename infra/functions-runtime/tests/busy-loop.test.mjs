// busy_loop_isolation: one bundle spins in `while (true) {}`.
//
// workerd runs every isolate on one thread and does not enforce CPU limits outside Cloudflare,
// so another bundle does not answer while the loop runs (the spike's answer). The watchdog
// covers it: it probes the runtime every 2s and ends the container after 3 failed probes, and
// the restart policy brings it back. This test asserts the guarantee either way: within 30s
// the other bundle answers again, and if it could not answer within 1s, the runtime restarted.
import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { invoke, manifest, startBundleStore, startRuntime, waitReady } from "./harness.mjs";

const DB = "db_busyloop";
let store;
let rt;
before(async () => {
  store = await startBundleStore();
  rt = await startRuntime(store, { restart: "always" });
});
after(async () => {
  await rt?.stop();
  await store?.close();
});

test("busy_loop_isolation", async (t) => {
  const busy = store.add(DB, manifest("busy"));
  const hello = store.add(DB, manifest("hello-a"));
  assert.equal(await (await invoke(rt, DB, hello)).text(), "A");
  const restartsBefore = (await rt.inspect()).RestartCount;

  const spinning = invoke(rt, DB, busy, { timeout: 30_000 }).catch((e) => e);
  await new Promise((r) => setTimeout(r, 300));

  let isolated = false;
  try {
    isolated = (await (await invoke(rt, DB, hello, { timeout: 1000 })).text()) === "A";
  } catch {}
  t.diagnostic(`another bundle answered within 1s while one loops: ${isolated}`);

  if (!isolated) {
    // The watchdog ends the runtime; the restart policy starts it again.
    const deadline = Date.now() + 30_000;
    let restarts = restartsBefore;
    while (Date.now() < deadline && restarts === restartsBefore) {
      await new Promise((r) => setTimeout(r, 500));
      restarts = (await rt.inspect()).RestartCount;
    }
    assert.ok(restarts > restartsBefore, "the watchdog restarted the stuck runtime within 30s");
    assert.match(await rt.logs(), /watchdog: .*unresponsive/);
    await rt.refresh();
    await waitReady(rt, 20_000);
  }
  assert.equal(await (await invoke(rt, DB, hello)).text(), "A", "the other bundle answers again");
  await spinning;
});
