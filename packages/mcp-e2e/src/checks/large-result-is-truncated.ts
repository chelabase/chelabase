import assert from "node:assert/strict";
import { admin, type Check } from "../lib.ts";

export const check: Check = {
  name: "large-result-is-truncated",
  async run(e) {
    const s = await admin(e);
    try {
      const out = await s.execute(`async () => "x".repeat(200000)`);
      assert.equal(out.isError, false, out.text);
      assert.match(out.text, /--- TRUNCATED: \d+ of \d+ bytes/);
      assert.ok(out.text.length < 40_000, `the cut result is ${out.text.length} characters`);
    } finally {
      await s.close();
    }
  },
};
