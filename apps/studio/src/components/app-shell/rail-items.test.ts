import { describe, expect, it } from "vitest";
import { meAs } from "@/test/table-editor-fixtures";
import { RAIL_ENTRIES, canUseSql, hiddenRailItems, isRailItemActive } from "./rail-items";

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

  it("storage_item_active_on_storage_routes", () => {
    expect(isRailItemActive("storage", "/app/storage")).toBe(true);
    expect(isRailItemActive("storage", "/app/storage/p1/d1/b1/avatars/a/b.png")).toBe(true);
    expect(isRailItemActive("storage", "/app/storagex")).toBe(false);
    expect(isRailItemActive("storage", "/app/sql/p1")).toBe(false);
    expect(isRailItemActive("sql", "/app/storage")).toBe(false);
    expect(isRailItemActive("tables", "/app/storage/p1")).toBe(false);
    expect(isRailItemActive("projects", "/app/storage")).toBe(false);
  });

  it("data_api_item_active_on_data_api_routes", () => {
    expect(isRailItemActive("data-api", "/app/data-api")).toBe(true);
    expect(isRailItemActive("data-api", "/app/data-api/p1/d1/b1/app")).toBe(true);
    expect(isRailItemActive("data-api", "/app/data-apix")).toBe(false);
    expect(isRailItemActive("data-api", "/app/api")).toBe(false);
    expect(isRailItemActive("api", "/app/data-api")).toBe(false);
    expect(isRailItemActive("projects", "/app/data-api")).toBe(false);
  });

  it("functions_item_active_on_functions_routes", () => {
    expect(isRailItemActive("functions", "/app/functions")).toBe(true);
    expect(isRailItemActive("functions", "/app/functions/")).toBe(true);
    expect(isRailItemActive("functions", "/app/functions/p1/d1/b1")).toBe(true);
    expect(isRailItemActive("functions", "/app/functionsx")).toBe(false);
    expect(isRailItemActive("functions", "/app/data-api/p1")).toBe(false);
    expect(isRailItemActive("data-api", "/app/functions")).toBe(false);
    expect(isRailItemActive("projects", "/app/functions")).toBe(false);
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
  it("lists Projects, Table editor, SQL editor, Storage, Data API, Functions, API reference and Members (last) in order", () => {
    expect(RAIL_ENTRIES.map((e) => [e.kind, e.id, e.label])).toEqual([
      ["link", "projects", "Projects"],
      ["link", "tables", "Table editor"],
      ["link", "sql", "SQL editor"],
      ["link", "storage", "Storage"],
      ["link", "data-api", "Data API"],
      ["link", "functions", "Functions"],
      ["link", "api", "API reference"],
      ["link", "members", "Members"],
    ]);
    expect(RAIL_ENTRIES.flatMap((e) => (e.kind === "link" ? [e.to] : []))).toEqual(["/app", "/app/tables", "/app/sql", "/app/storage", "/app/data-api", "/app/functions", "/app/api", "/app/members"]);
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

describe("hiddenRailItems", () => {
  const withFunctions = (me: object, functions_available: boolean | undefined) => ({ ...me, functions_available }) as never;

  it("hides_functions_when_me_says_unavailable", () => {
    expect(hiddenRailItems(withFunctions(meAs("developer"), true))).toEqual([]);
    expect(hiddenRailItems(withFunctions(meAs("developer"), false))).toEqual(["functions"]);
  });

  it("keeps_functions_until_me_says_otherwise", () => {
    // Not loaded yet, or a response from before the field: shown (the API still refuses in platform mode).
    expect(hiddenRailItems(withFunctions(meAs("developer"), undefined))).toEqual([]);
    expect(hiddenRailItems(undefined)).toEqual(["sql"]);
  });

  it("hides_sql_for_a_viewer_alongside", () => {
    expect(hiddenRailItems(withFunctions(meAs("viewer"), true))).toEqual(["sql"]);
    expect(hiddenRailItems(withFunctions(meAs("viewer"), false))).toEqual(["sql", "functions"]);
  });
});
