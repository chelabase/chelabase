import { describe, expect, it } from "vitest";
import { isUuid, validateMemberPageSearch, validateMembersSearch, validateRolePageSearch, validateTeamPageSearch } from "./members-search";

const UUID = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";

describe("members search", () => {
  it("accepts only UUIDs where a UUID is meant, in any case", () => {
    expect(isUuid(UUID)).toBe(true);
    expect(isUuid(UUID.toUpperCase())).toBe(true);
    for (const bad of ["", "abc", `${UUID}x`, ` ${UUID}`, `${UUID}\n`, 5, null, undefined, [UUID]]) expect(isUuid(bad)).toBe(false);
  });

  it("drops an empty or oversized text value and keeps the people tab out of the URL", () => {
    expect(validateMembersSearch({ q: "" , pg_database: "" })).toEqual({});
    expect(validateMembersSearch({ q: "x".repeat(201) })).toEqual({});
    expect(validateMembersSearch({ q: "x".repeat(200) })).toEqual({ q: "x".repeat(200) });
    expect(validateMembersSearch({ tab: "people" })).toEqual({});
  });

  it("the member and role pages carry only their own params", () => {
    expect(validateMemberPageSearch({ tab: "roles", project: UUID, view: "cards", q: "a", database: UUID, pg_database: "x" })).toEqual({
      tab: "roles", project: UUID, view: "cards", q: "a",
    });
    expect(validateRolePageSearch({ pg_database: "app", project: UUID, view: "cards" })).toEqual({ pg_database: "app" });
    expect(validateRolePageSearch({ pg_database: 4 })).toEqual({});
  });

  it("the teams tab is a tab; the team page carries only the view and the name filter", () => {
    expect(validateMembersSearch({ tab: "teams" })).toEqual({ tab: "teams" });
    expect(validateMembersSearch({ tab: "teamz" })).toEqual({});
    expect(validateTeamPageSearch({ tab: "teams", view: "cards", q: "bill", project: UUID })).toEqual({ view: "cards", q: "bill" });
    expect(validateTeamPageSearch({ view: "grid", q: "" })).toEqual({});
  });
});
