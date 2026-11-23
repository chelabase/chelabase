// Runs the checks one after another, each in its own MCP session, and names the one that fails.
// Exits non-zero when any check fails.
import { env } from "./lib.ts";
import { checks } from "./checks/index.ts";

const e = env();
const failed: string[] = [];
const t0 = Date.now();
for (const check of checks) {
  const t = Date.now();
  try {
    await check.run(e);
    console.log(`PASS ${check.name} (${((Date.now() - t) / 1000).toFixed(1)} s)`);
  } catch (err) {
    failed.push(check.name);
    console.error(`FAIL ${check.name}: ${err instanceof Error ? (err.stack ?? err.message) : String(err)}`);
  }
}
const seconds = ((Date.now() - t0) / 1000).toFixed(0);
if (failed.length > 0) {
  console.error(`${failed.length} failed: ${failed.join(", ")} (${seconds} s)`);
  process.exit(1);
}
console.log(`all checks passed (${seconds} s)`);
