import { describe, expect, it } from "vitest";
import type { Team } from "@chelabase/management-client";
import { effectiveRole, lockedRoles, provenanceNote, rolesPerTeam, teamLevel, teamsOnProject, viaWords } from "./team-access";

const team = (id: string, name: string, projects: { project_id: string; platform_role: "viewer" | "developer" | "admin"; pg_roles: string[] | null }[]): Team =>
  ({ id, name, description: "", member_count: 1, projects, created_at: "", updated_at: "" }) as unknown as Team;

const billing = team("t1", "Billing", [{ project_id: "p1", platform_role: "developer", pg_roles: ["billing_editor", "shared"] }]);
const support = team("t2", "Support", [
  { project_id: "p1", platform_role: "viewer", pg_roles: ["shared", "tickets"] },
  { project_id: "p2", platform_role: "viewer", pg_roles: null },
]);

describe("team provenance", () => {
  it("teams_on_project_keeps_only_that_project_by_name", () => {
    expect(teamsOnProject([support, billing], "p1").map((t) => t.name)).toEqual(["Billing", "Support"]);
    expect(teamsOnProject([support, billing], "p2").map((t) => t.name)).toEqual(["Support"]);
    expect(teamsOnProject([support, billing], "p9")).toEqual([]);
  });

  it("locked_roles_are_each_role_once_with_every_team_that_names_it", () => {
    const on = teamsOnProject([support, billing], "p1");
    expect(lockedRoles(on)).toEqual([
      { value: "billing_editor", teams: [{ id: "t1", name: "Billing" }] },
      { value: "shared", teams: [{ id: "t1", name: "Billing" }, { id: "t2", name: "Support" }] },
      { value: "tickets", teams: [{ id: "t2", name: "Support" }] },
    ]);
  });

  it("roles_the_caller_may_not_see_are_not_locked_rows", () => {
    expect(lockedRoles(teamsOnProject([support], "p2"))).toEqual([]);
  });

  it("effective_role_is_the_highest_source", () => {
    const on = teamsOnProject([billing], "p1");
    expect(effectiveRole("viewer", on)).toBe("developer");
    expect(effectiveRole("admin", on)).toBe("admin");
    expect(effectiveRole(null, on)).toBe("developer");
    expect(effectiveRole(null, [])).toBeNull();
  });

  it("via_words_name_the_teams", () => {
    expect(viaWords(teamsOnProject([billing], "p1"))).toBe("Billing team");
    expect(viaWords(teamsOnProject([billing, support], "p1"))).toBe("Billing and Support teams");
    const third = team("t3", "Readers", [{ project_id: "p1", platform_role: "viewer", pg_roles: [] }]);
    expect(viaWords(teamsOnProject([billing, support, third], "p1"))).toBe("Billing, Readers and Support teams");
  });

  it("provenance_maps_own_team_and_both", () => {
    const on = teamsOnProject([billing], "p1");
    expect(provenanceNote("own", on)).toBeNull();
    expect(provenanceNote(undefined, on)).toBeNull();
    expect(provenanceNote("team", on)).toBe("Access through Billing team");
    expect(provenanceNote("both", on)).toBe("Own grant and Billing team");
    expect(provenanceNote("team", [])).toBeNull();
  });

  it("roles_per_team_group_each_teams_roles_and_skip_teams_with_none", () => {
    const readers = team("t3", "Readers", [{ project_id: "p1", platform_role: "viewer", pg_roles: [] }]);
    const hidden = team("t4", "Hidden", [{ project_id: "p1", platform_role: "viewer", pg_roles: null }]);
    expect(rolesPerTeam(teamsOnProject([support, billing, readers, hidden], "p1"))).toEqual([
      { team: { id: "t1", name: "Billing" }, roles: ["billing_editor", "shared"] },
      { team: { id: "t2", name: "Support" }, roles: ["shared", "tickets"] },
    ]);
  });

  it("team_level_is_shown_only_when_higher_than_the_own_grant", () => {
    const on = teamsOnProject([billing, support], "p1");
    expect(teamLevel("viewer", on)).toEqual({ role: "developer", teams: [{ id: "t1", name: "Billing" }] });
    expect(teamLevel(null, on)?.role).toBe("developer");
    expect(teamLevel("developer", on)).toBeNull();
    expect(teamLevel("admin", on)).toBeNull();
    expect(teamLevel("viewer", [])).toBeNull();
  });
});
