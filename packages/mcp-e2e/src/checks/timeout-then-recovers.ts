import assert from "node:assert/strict";
import { admin, type Check } from "../lib.ts";

// m3 (#347) measured a busy loop killed by the runtime's watchdog and the runtime answering again
// 8.5 s after the loop began; the margin covers a loaded machine.
const RESTART_MS = 8_512;
const MARGIN_MS = 10_000;
const LIMIT_MS = RESTART_MS + MARGIN_MS;

export const check: Check = {
  name: "timeout-then-recovers",
  async run(e) {
    const s = await admin(e);
    try {
      // Code that awaits forever runs into the server's limit (30 s by default).
      const hung = await s.execute("async () => { await new Promise(() => {}); }");
      assert.equal(hung.error?.code, "timeout", hung.text);
      assert.equal(await s.run("async () => 1 + 1"), 2);

      // A busy loop stalls the runtime until its watchdog restarts it: the call fails (the
      // runtime goes away before the limit, `runtime_unavailable`, or the limit comes first,
      // `timeout`) and a call works again within the restart time plus a margin.
      const loop = await s.execute("async () => { for (;;) {} }");
      assert.equal(loop.isError, true, loop.text);
      assert.ok(
        ["timeout", "runtime_unavailable"].includes(loop.error.code),
        `the busy loop gave ${loop.text}`,
      );
      // Until the runtime answers again, calls fail fast with `runtime_unavailable`.
      const t = Date.now();
      let next = await s.execute("async () => 1 + 1");
      while (next.error?.code === "runtime_unavailable" && Date.now() - t < LIMIT_MS) {
        await new Promise((r) => setTimeout(r, 500));
        next = await s.execute("async () => 1 + 1");
      }
      const took = Date.now() - t;
      assert.equal(next.value, 2, `no answer ${took} ms after the loop: ${next.text}`);
      assert.ok(took < LIMIT_MS, `the next call succeeded after ${took} ms`);
    } finally {
      await s.close();
    }
  },
};
