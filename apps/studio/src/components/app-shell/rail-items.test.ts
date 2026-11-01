import { describe, expect, it } from "vitest";
import { meAs } from "@/test/table-editor-fixtures";
import { RAIL_ENTRIES, canUseSql, isRailItemActive } from "./rail-items";

describe("isRailItemActive", () => {
  it("tables_item_active_on_editor_routes", () => {
    expect(isRailItemActive("tables", "/app/tables")).toBe(true);
    expect(isRailItemActive("tables", "/app/tables/p1/d1")).toBe(true);
    expect(isRailItemActive("tables", "/app/tablesx")).toBe(false);
    expect(isRailItemActive("projects", "/app/tables/p1")).toBe(false);
  });

  it("sql_item_active_on_sql_routes", () => {
    expect(isRailItemActive("sql", "/app/sql")).toBe(true);
    expect(isRailItemActive("sql", "/app/sql/p1/d1/b1")).toBe(true);
    expect(isRailItemActive("sql", "/app/sqlx")).toBe(false);
    expect(isRailItemActive("sql", "/app/tables/p1")).toBe(false);
    expect(isRailItemActive("tables", "/app/sql/p1")).toBe(false);
    expect(isRailItemActive("projects", "/app/sql")).toBe(false);
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
  it("lists Projects, Table editor, SQL editor, API reference and Members (last) in order", () => {
    expect(RAIL_ENTRIES.map((e) => [e.kind, e.id, e.label])).toEqual([
      ["link", "projects", "Projects"],
      ["link", "tables", "Table editor"],
      ["link", "sql", "SQL editor"],
      ["link", "api", "API reference"],
      ["link", "members", "Members"],
    ]);
    expect(RAIL_ENTRIES.flatMap((e) => (e.kind === "link" ? [e.to] : []))).toEqual(["/app", "/app/tables", "/app/sql", "/app/api", "/app/members"]);
  });
});

describe("canUseSql", () => {
  it("is_for_a_developer_on_some_database_not_a_viewer", () => {
    expect(canUseSql(meAs("developer") as never)).toBe(true);
    expect(canUseSql(meAs("admin") as never)).toBe(true);
    expect(canUseSql(meAs("viewer") as never)).toBe(false);
    expect(canUseSql(undefined)).toBe(false);
  });
});
