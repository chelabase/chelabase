import { describe, expect, it } from "vitest";
import { connectionFocus, parseProjectSearch } from "./project-search";

describe("project page search", () => {
  it("keeps the tab and a branch's connection panel", () => {
    expect(parseProjectSearch({ tab: "settings" })).toEqual({ tab: "settings" });
    expect(parseProjectSearch({ database: "d1", branch: "b1", panel: "connection", role: "reporter" })).toEqual({
      database: "d1",
      branch: "b1",
      panel: "connection",
      role: "reporter",
    });
    expect(parseProjectSearch({ tab: "nope", panel: "other", role: 7, database: "" })).toEqual({});
  });

  it("names the panel to open only when database, branch and panel are all there", () => {
    expect(connectionFocus({ database: "d1", branch: "b1", panel: "connection", role: "reporter" })).toEqual({
      databaseId: "d1",
      branchId: "b1",
      role: "reporter",
    });
    expect(connectionFocus({ database: "d1", branch: "b1", panel: "connection" })).toEqual({
      databaseId: "d1",
      branchId: "b1",
      role: null,
    });
    expect(connectionFocus({ database: "d1", panel: "connection" })).toBeNull();
  });
});
