import assert from "node:assert/strict";
import { admin, request, type Check } from "../lib.ts";

export const check: Check = {
  name: "unknown-route-is-refused",
  async run(e) {
    const s = await admin(e);
    try {
      // `chelabase.request` doesn't throw on a refusal: the code sees a 403 `operation_not_allowed`.
      for (const req of [
        { method: "GET", path: "/v1/not-in-the-spec" },
        { method: "GET", path: "/auth/v1/instance" },
        { method: "PATCH", path: "/v1/projects" },
      ]) {
        const out = await s.execute(request(req));
        const v = out.isError ? { status: 0, body: out } : out.value;
        assert.equal(v.status, 403, `${req.path}: ${out.text}`);
        assert.equal(v.body.error.code, "operation_not_allowed", `${req.path}: ${out.text}`);
      }
    } finally {
      await s.close();
    }
  },
};
