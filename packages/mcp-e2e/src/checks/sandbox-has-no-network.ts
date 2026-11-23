import assert from "node:assert/strict";
import { admin, type Check } from "../lib.ts";

export const check: Check = {
  name: "sandbox-has-no-network",
  async run(e) {
    const s = await admin(e);
    try {
      const out = await s.execute(
        `async () => { const r = await fetch("https://example.com"); return r.status; }`,
      );
      assert.equal(out.isError, true, `fetch got out of the sandbox: ${out.text}`);
      assert.equal(out.error.code, "threw", out.text);
    } finally {
      await s.close();
    }
  },
};
