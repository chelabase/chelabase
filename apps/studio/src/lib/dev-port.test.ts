import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { devPort } from "./dev-port";

describe("devPort", () => {
  it("is 3000 when no slot sets it", () => {
    expect(devPort({})).toBe(3000);
    expect(devPort({ CHELADB_STUDIO_PORT: "" })).toBe(3000);
  });

  it("follows the worktree slot's CHELADB_STUDIO_PORT", () => {
    expect(devPort({ CHELADB_STUDIO_PORT: "3300" })).toBe(3300);
  });

  it("refuses a value that isn't a port", () => {
    for (const bad of ["x", "0", "70000", "3000.5"]) {
      expect(() => devPort({ CHELADB_STUDIO_PORT: bad })).toThrow(/CHELADB_STUDIO_PORT/);
    }
  });

  it("isn't overridden by a port flag in the dev script", () => {
    // A CLI --port beats server.port, so slot N's Studio would bind 3000 and clash with slot 0.
    // vitest runs from apps/studio.
    const pkg = JSON.parse(readFileSync("package.json", "utf8"));
    expect(pkg.scripts.dev).not.toMatch(/--port/);
  });
});
