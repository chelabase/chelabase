import { describe, expect, it } from "vitest";
import { RAIL_ENTRIES, isRailItemActive } from "./rail-items";

describe("isRailItemActive", () => {
  it("projects is active on /app, /app/ and /app/projects/p1", () => {
    expect(isRailItemActive("projects", "/app")).toBe(true);
    expect(isRailItemActive("projects", "/app/")).toBe(true);
    expect(isRailItemActive("projects", "/app/projects/p1")).toBe(true);
  });

  it("projects is not active on /app/api", () => {
    expect(isRailItemActive("projects", "/app/api")).toBe(false);
    expect(isRailItemActive("projects", "/app/api/")).toBe(false);
    expect(isRailItemActive("projects", "/app/apix")).toBe(false);
  });

  it("api is active on /app/api only", () => {
    expect(isRailItemActive("api", "/app/api")).toBe(true);
    expect(isRailItemActive("api", "/app/api/")).toBe(true);
    expect(isRailItemActive("api", "/app/apix")).toBe(false);
    expect(isRailItemActive("api", "/app")).toBe(false);
    expect(isRailItemActive("api", "/app/projects/p1")).toBe(false);
  });
});

describe("RAIL_ENTRIES", () => {
  it("lists Projects, API reference and two placeholders in order", () => {
    expect(RAIL_ENTRIES.map((e) => [e.kind, e.id, e.label])).toEqual([
      ["link", "projects", "Projects"],
      ["link", "api", "API reference"],
      ["placeholder", "placeholder-1", "Placeholder"],
      ["placeholder", "placeholder-2", "Placeholder"],
    ]);
    expect(RAIL_ENTRIES.flatMap((e) => (e.kind === "link" ? [e.to] : []))).toEqual(["/app", "/app/api"]);
  });
});
