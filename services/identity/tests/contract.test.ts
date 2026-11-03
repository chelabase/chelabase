// openapi.json and the route table agree: every documented operation routes, its x-callers
// are exactly the callers the allow-list lets through, and on a pool route its x-caller-pools
// are exactly the kinds of pool the allow-list gives each of them. No database needed.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { CALLERS } from "../src/config.ts";
import { ALLOW, match } from "../src/routes.ts";

const doc = JSON.parse(readFileSync(new URL("../openapi.json", import.meta.url), "utf8"));

interface Op {
  "x-callers": string[];
  "x-caller-pools"?: Record<string, string[]>;
}

describe("openapi contract", () => {
  it("openapi_matches_routes_and_allow_list", () => {
    let operations = 0;
    for (const [path, item] of Object.entries<Record<string, Op>>(doc.paths)) {
      const concrete = path.replace("{pool}", "members").replace("{id}", "abc123");
      for (const [method, op] of Object.entries(item)) {
        if (method === "parameters") continue;
        operations += 1;
        const m = match(method.toUpperCase(), concrete);
        expect(m, `${method} ${path}`).not.toBeNull();
        const allowed = CALLERS.filter((c) => ALLOW[c].has(m!.route.id));
        expect([...op["x-callers"]].sort(), `${method} ${path}`).toEqual(allowed);
        if (m!.global) {
          expect(op["x-caller-pools"], `${method} ${path}`).toBeUndefined();
          continue;
        }
        const pools = Object.fromEntries(allowed.map((c) => [c, [...ALLOW[c].get(m!.route.id)!]]));
        expect(op["x-caller-pools"], `${method} ${path}`).toEqual(pools);
      }
    }
    const routeIds = new Set(CALLERS.flatMap((c) => [...ALLOW[c].keys()]));
    expect(operations).toBe(routeIds.size);
  });
});
