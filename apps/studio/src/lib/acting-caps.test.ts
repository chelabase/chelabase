import { describe, expect, it } from "vitest";
import type { Branch, BranchDatabaseView, BranchRole, MeResponse } from "@chelabase/management-client";
import { actingCaps, projectCaps } from "@/lib/capabilities";
import { branch, meAs, pgDb } from "@/test/table-editor-fixtures";
import { rowAccess, structureAccess } from "@/lib/table-editor/access";

const dev = branch("b2", "staging", "b1") as unknown as Branch;
const on = pgDb("app", true) as unknown as BranchDatabaseView;
const role = (name: string, member_of: string[] = []) => ({ name, origin: "member", member_of, member_no_set: [] }) as unknown as BranchRole;
const branchRoles = [role("billing"), role("billing_admin", ["billing"]), role("sales")];

/** A Developer with `sales` at Developer, `billing_admin` (and its chain `billing`) at Viewer. */
function mixed() {
  const me = meAs("developer", ["sales", "billing_admin"]);
  (me.projects[0] as Record<string, unknown>).role_levels = { sales: "developer", billing_admin: "viewer" };
  return projectCaps(me as unknown as MeResponse, "p1");
}

describe("acting as a role", () => {
  it("a Viewer-level role gates structure and rows, a Developer-level one doesn't", () => {
    const c = mixed();
    const viewer = actingCaps(c, "billing_admin", branchRoles);
    expect(structureAccess(viewer, dev)).toEqual({ allowed: false, reason: "Viewers can't change the schema" });
    expect(rowAccess(viewer, { owner: false, roles: ["billing_admin"] }, on).write).toBe(false);
    // A role its chain reaches counts at the granting role's level.
    expect(actingCaps(c, "billing", branchRoles).role).toBe("viewer");
    const developer = actingCaps(c, "sales", branchRoles);
    expect(structureAccess(developer, dev)).toEqual({ allowed: true });
    expect(rowAccess(developer, { owner: false, roles: ["sales"] }, on).write).toBe(true);
  });
  it("Admin (every role) and the owner keep the project level", () => {
    const admin = projectCaps(meAs("admin") as unknown as MeResponse, "p1");
    expect(actingCaps(admin, "sales", branchRoles).role).toBe("admin");
    expect(actingCaps(mixed(), null, branchRoles).role).toBe("developer");
  });
  it("a named data API role under an Admin keeps its own level", () => {
    const me = meAs("admin");
    Object.assign(me.projects[0] as Record<string, unknown>, { role_levels: { anon: "viewer" } });
    const c = projectCaps(me as unknown as MeResponse, "p1");
    expect(actingCaps(c, "anon", branchRoles).role).toBe("viewer");
    expect(actingCaps(c, "sales", branchRoles).role).toBe("admin");
  });
  it("an API without role_levels leaves the project level", () => {
    expect(actingCaps(projectCaps(meAs("developer", ["sales"]) as unknown as MeResponse, "p1"), "sales", branchRoles).role).toBe("developer");
  });
  it("the database's levels win over the project's", () => {
    const me = meAs("developer", ["sales"]);
    Object.assign(me.projects[0] as Record<string, unknown>, {
      role_levels: { sales: "developer" },
      databases: [{ database_id: "d1", platform_role: "viewer", roles: ["sales"], role_levels: { sales: "viewer" } }],
    });
    const c = projectCaps(me as unknown as MeResponse, "p1", "d1");
    expect(actingCaps(c, "sales", branchRoles).role).toBe("viewer");
  });
});
