import { describe, expect, it } from "vitest";
import type { AccessLevel, Member } from "@chelabase/management-client";
import { groupRoster } from "./index";
import { P1, P2, person } from "./fixtures";

const keysOf = (gs: ReturnType<typeof groupRoster>) => gs.map((g) => g.key);
const idsOf = (gs: ReturnType<typeof groupRoster>, key: string) => gs.find((g) => g.key === key)?.members.map((m) => m.id);

const roster: Member[] = [
  person("non", { level: "none" }),
  person("vi", { level: "viewer", projects: [[P1, "viewer"]] }),
  person("dev1", { level: "developer", projects: [[P1, "developer"], [P2, "viewer"]] }),
  person("ana", { level: "tenant_admin", projects: [[P1, "admin"], [P2, "admin"]] }),
  person("oth", { level: "other" }),
  person("dev2", { level: "developer", projects: [[P2, "developer"]] }),
  person("pri", { level: "admin", projects: [[P1, "admin"]] }),
  person("inv", { level: "none", status: "invited" }),
  person("sus", { level: "developer", status: "suspended", projects: [[P1, "developer"]] }),
  person("ed", { level: "none", status: "pending" }),
];

describe("groupRoster", () => {
  it("groupRoster_orders_groups_and_hides_empty_ones", () => {
    const groups = groupRoster(roster);
    expect(groups.map((g) => g.title)).toEqual([
      "Tenant admins", "Project admins", "Developers", "Viewers", "Other projects", "No access yet", "Invited", "Suspended",
    ]);
    expect(groups.map((g) => g.members.length)).toEqual([1, 1, 2, 1, 1, 1, 1, 1]);
    // Input order is kept inside a group.
    expect(idsOf(groups, "developers")).toEqual(["dev1", "dev2"]);
    // Empty groups are not returned at all.
    const small = groupRoster([person("v", { level: "viewer", projects: [[P1, "viewer"]] }), person("a", { level: "tenant_admin" })]);
    expect(keysOf(small)).toEqual(["tenant_admins", "viewers"]);
    expect(groupRoster([])).toEqual([]);
  });

  it("leaves pending and removed members out and lets status decide before level", () => {
    const groups = groupRoster(roster);
    const all = groups.flatMap((g) => g.members.map((m) => m.id));
    expect(all).not.toContain("ed");
    expect(idsOf(groups, "invited")).toEqual(["inv"]);
    // A suspended developer is Suspended, not a Developer.
    expect(idsOf(groups, "suspended")).toEqual(["sus"]);
    expect(idsOf(groups, "developers")).not.toContain("sus");
    // A suspended tenant admin and an invited member that somehow has a level: status still wins.
    const tricky = groupRoster([
      person("sa", { level: "tenant_admin", status: "suspended" }),
      person("ia", { level: "admin", status: "invited", projects: [[P1, "admin"]] }),
      person("gone", { level: "viewer", status: "removed" }),
    ]);
    expect(keysOf(tricky)).toEqual(["invited", "suspended"]);
  });

  it("puts other and none levels in their own groups", () => {
    const groups = groupRoster(roster);
    expect(idsOf(groups, "other")).toEqual(["oth"]);
    expect(idsOf(groups, "none")).toEqual(["non"]);
    expect(groups.find((g) => g.key === "other")?.title).toBe("Other projects");
    expect(groups.find((g) => g.key === "none")?.title).toBe("No access yet");
  });

  it("project_filter_regroups_by_that_projects_role", () => {
    const groups = groupRoster(roster, { projectId: P1 });
    expect(keysOf(groups)).toEqual(["admins", "developers", "viewers", "none", "invited", "suspended"]);
    // Tenant admins sit with the Admins of the project.
    expect(idsOf(groups, "admins")).toEqual(["ana", "pri"]);
    expect(idsOf(groups, "developers")).toEqual(["dev1"]);
    expect(idsOf(groups, "viewers")).toEqual(["vi"]);
    // No role on P1: a developer elsewhere, "other", "none" levels all land in No access.
    expect(idsOf(groups, "none")).toEqual(["non", "oth", "dev2"]);
    expect(idsOf(groups, "invited")).toEqual(["inv"]);
    expect(idsOf(groups, "suspended")).toEqual(["sus"]);
    // The same people on P2: dev1 is only a Viewer there.
    const p2 = groupRoster(roster, { projectId: P2 });
    expect(idsOf(p2, "viewers")).toEqual(["dev1"]);
    expect(idsOf(p2, "developers")).toEqual(["dev2"]);
    // A project nobody holds: everyone active is in No access (tenant admins aside, who are Admin everywhere in the API).
    const nobody = groupRoster([person("v", { level: "viewer", projects: [[P1, "viewer"]] })], { projectId: P2 });
    expect(keysOf(nobody)).toEqual(["none"]);
  });

  it("puts every listed member in exactly one group (exhaustive over status, level and filter)", () => {
    const statuses = ["active", "invited", "pending", "suspended", "removed"] as const;
    const levels: AccessLevel[] = ["tenant_admin", "admin", "developer", "viewer", "other", "none"];
    for (const projectId of [undefined, P1]) {
      for (const status of statuses) {
        for (const level of levels) {
          const m = person("x", { status, level, projects: level === "none" || level === "other" ? [] : [[P1, level === "tenant_admin" ? "admin" : (level as "admin")]] });
          const groups = groupRoster([m], { projectId });
          const listed = status !== "pending" && status !== "removed";
          expect(groups.flatMap((g) => g.members), `${status}/${level}/${projectId ?? "all"}`).toHaveLength(listed ? 1 : 0);
        }
      }
    }
  });

  it("does not mutate its input", () => {
    const copy = structuredClone(roster);
    expect(groupRoster(roster)).not.toHaveLength(0);
    expect(groupRoster(roster, { projectId: P1 })).not.toHaveLength(0);
    expect(roster).toEqual(copy);
  });
});
