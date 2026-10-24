import { createMemoryHistory, createRouter } from "@tanstack/react-router";
import { describe, expect, it } from "vitest";
import { routeTree } from "@/routeTree.gen";
import { Route } from "./app.members";

const UUID = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
const validate = (search: Record<string, unknown>) =>
  (Route.options.validateSearch as (s: Record<string, unknown>) => Record<string, unknown>)(search);

describe("/app/members search", () => {
  it("unknown_search_params_are_dropped", () => {
    expect(validate({ tab: "bogus", project: "not-a-uuid", view: "grid", q: 5, database: "x", branch: 7, pg_database: 3, extra: "y" })).toEqual({});
    expect(validate({ tab: ["roles"], project: { a: 1 }, view: null, q: {} })).toEqual({});
    // Unknown values go; the known ones beside them stay.
    expect(validate({ tab: "bogus", project: UUID, view: "grid", q: "ana" })).toEqual({ project: UUID, q: "ana" });
    // Prototype-ish keys and values are not carried.
    expect(validate({ tab: "constructor", view: "__proto__" })).toEqual({});
  });

  it("keeps valid values, each parsed against its own enum", () => {
    expect(validate({ tab: "roles", project: UUID, view: "cards", q: "ana", database: UUID, branch: UUID, pg_database: "app" })).toEqual({
      tab: "roles", project: UUID, view: "cards", q: "ana", database: UUID, branch: UUID, pg_database: "app",
    });
    expect(validate({ tab: "activity", view: "list" })).toEqual({ tab: "activity", view: "list" });
    // One bad value does not take the good ones with it.
    expect(validate({ tab: "activity", view: "grid", project: UUID })).toEqual({ tab: "activity", project: UUID });
  });

  it("back_link_restores_tab_filter_and_view: the hub routes exist in the committed route tree", () => {
    const router = createRouter({ routeTree, history: createMemoryHistory({ initialEntries: ["/"] }) });
    const paths = Object.keys(router.routesByPath);
    expect(paths).toContain("/app/members");
    expect(paths).toContain("/app/members/$memberId");
    expect(paths).toContain("/app/members/roles/$databaseId/$branchId/$role");
  });
});
