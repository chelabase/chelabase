import { describe, expect, it } from "vitest";
import { deleteConsequence, teamName, validateTeamForm } from "./team-model";
import { membershipChanges } from "./team-members";

describe("validateTeamForm", () => {
  it("accepts a trimmed name of 1 to 80 characters and a description of at most 500", () => {
    expect(validateTeamForm({ name: " Billing ", description: "" })).toBeNull();
    expect(validateTeamForm({ name: "x".repeat(80), description: "y".repeat(500) })).toBeNull();
  });
  it("refuses an empty, blank or too long name and a too long description", () => {
    expect(validateTeamForm({ name: "", description: "" })?.name).toBeDefined();
    expect(validateTeamForm({ name: "   ", description: "" })?.name).toBeDefined();
    expect(validateTeamForm({ name: "x".repeat(81), description: "" })?.name).toBeDefined();
    expect(validateTeamForm({ name: "ok", description: "y".repeat(501) })?.description).toBeDefined();
  });
  it("counts characters, not UTF-16 units", () => {
    expect(validateTeamForm({ name: "😀".repeat(80), description: "" })).toBeNull();
  });
});

describe("teamName and deleteConsequence", () => {
  it("falls back for a blank name", () => {
    expect(teamName({ name: "  " })).toBe("Unnamed team");
  });
  it("says how many members get access through the team", () => {
    const projects = [{ project_id: "p", platform_role: "viewer" as const, pg_roles: null }];
    expect(deleteConsequence({ name: "Billing", member_count: 1, projects })).toContain("1 member gets access to 1 project through Billing");
    expect(deleteConsequence({ name: "Billing", member_count: 3, projects })).toContain("3 members get access");
    expect(deleteConsequence({ name: "Billing", member_count: 3, projects: [] })).toContain("nobody loses any");
    expect(deleteConsequence({ name: "Billing", member_count: 0, projects })).toContain("nobody loses access");
  });
});

describe("membershipChanges", () => {
  const roster = [{ id: "a" }, { id: "b" }, { id: "c" }];
  it("is the symmetric difference, in roster order", () => {
    expect(membershipChanges(roster, new Set(["a", "b"]), new Set(["b", "c"]))).toEqual([
      { memberId: "a", add: false },
      { memberId: "c", add: true },
    ]);
    expect(membershipChanges(roster, new Set(["a"]), new Set(["a"]))).toEqual([]);
  });
  it("ignores people not in the roster", () => {
    expect(membershipChanges(roster, new Set(["zz"]), new Set())).toEqual([]);
  });
});
