import { describe, expect, it } from "vitest";
import { RAIL_ENTRIES, isRailItemActive } from "./rail-items";

describe("isRailItemActive", () => {
  it("tables_item_active_on_editor_routes", () => {
    expect(isRailItemActive("tables", "/app/tables")).toBe(true);
    expect(isRailItemActive("tables", "/app/tables/p1/d1")).toBe(true);
    expect(isRailItemActive("tables", "/app/tablesx")).toBe(false);
    expect(isRailItemActive("projects", "/app/tables/p1")).toBe(false);
  });

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

  it("members_item_active_on_members_routes", () => {
    expect(isRailItemActive("members", "/app/members")).toBe(true);
    expect(isRailItemActive("members", "/app/members/")).toBe(true);
    expect(isRailItemActive("members", "/app/members/anything")).toBe(true);
    expect(isRailItemActive("members", "/app/membersx")).toBe(false);
    expect(isRailItemActive("members", "/app")).toBe(false);
    expect(isRailItemActive("projects", "/app/members")).toBe(false);
  });
});

describe("RAIL_ENTRIES", () => {
  it("lists Projects, Table editor, API reference and Members (last) in order", () => {
    expect(RAIL_ENTRIES.map((e) => [e.kind, e.id, e.label])).toEqual([
      ["link", "projects", "Projects"],
      ["link", "tables", "Table editor"],
      ["link", "api", "API reference"],
      ["link", "members", "Members"],
    ]);
    expect(RAIL_ENTRIES.flatMap((e) => (e.kind === "link" ? [e.to] : []))).toEqual(["/app", "/app/tables", "/app/api", "/app/members"]);
  });
});
