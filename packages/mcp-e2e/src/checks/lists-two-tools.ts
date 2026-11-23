import assert from "node:assert/strict";
import { admin, type Check } from "../lib.ts";

export const check: Check = {
  name: "lists-two-tools",
  async run(e) {
    const s = await admin(e);
    try {
      const { tools } = await s.client.listTools();
      assert.deepEqual(tools.map((t) => t.name).sort(), ["execute", "search"]);
      for (const t of tools) assert.equal(t.inputSchema.type, "object", t.name);
    } finally {
      await s.close();
    }
  },
};
