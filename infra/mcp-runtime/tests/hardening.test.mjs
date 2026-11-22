// Runs hardening.sh (non-root, read-only root, no capabilities, no secret env) under node --test.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { test } from "node:test";
import { promisify } from "node:util";

test("the image runs non-root with a read-only root and no capabilities", async (t) => {
  const script = new URL("./hardening.sh", import.meta.url).pathname;
  const { stdout, stderr } = await promisify(execFile)("bash", [script]).catch((e) => {
    assert.fail(`hardening.sh failed:\n${e.stdout}\n${e.stderr}`);
  });
  t.diagnostic(stdout + stderr);
  assert.match(stdout, /^hardening: ok$/m);
});
