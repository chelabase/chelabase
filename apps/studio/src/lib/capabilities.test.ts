import { describe, expect, it } from "vitest";
import type { BranchRole, MeResponse as Me } from "@chelabase/management-client";
import { actAsRoles, memberGate, noAccess, projectCaps } from "./capabilities";

const base: Me = {
  sub: "u1",
  email: "u1@example.test",
  name: "U1",
  tenant_id: "t1",
  member: { id: "m-self", status: "active", tenant_admin: false },
  instance_admin: false, functions_available: true, tenant_admins: [],
  projects: [],
  user: { sub: "u1" },
  tenant: { id: "t1", slug: "demo", display_name: null, projects: [] },
};

function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "set_in_sql", pending: false,
    ...extra,
  } as BranchRole;
}

const roles = [
  role("postgres", { origin: "owner", login: true, level: "owner_level" }),
  role("billing_admin", { member_of: ["billing"] }),
  role("billing"),
  role("reports"),
  role("app_login", { login: true }),
  role("anon"),
];

describe("projectCaps", () => {
  it("is admin with every role for a tenant admin, whatever the grants", () => {
    const caps = projectCaps({ ...base, member: { id: "m-self", status: "active", tenant_admin: true } }, "p1");
    expect(caps.role).toBe("admin");
    expect(caps.roles).toBe("all");
    expect(caps.atLeast("admin")).toBe(true);
  });

  it("follows the project's grant below tenant admin", () => {
    const me = { ...base, projects: [{ project_id: "p1", platform_role: "developer" as const, roles: ["billing_admin"], databases: [] }] };
    const caps = projectCaps(me, "p1");
    expect(caps.role).toBe("developer");
    expect(caps.atLeast("viewer")).toBe(true);
    expect(caps.atLeast("developer")).toBe(true);
    expect(caps.atLeast("admin")).toBe(false);
    expect(projectCaps(me, "other").role).toBeNull();
    expect(projectCaps(me, "other").atLeast("viewer")).toBe(false);
  });

  // I3: `/v1/me` lists the databases a grant is narrowed on; a database's caps are those there.
  it("uses the narrowed role and roles on a narrowed database", () => {
    const me = {
      ...base,
      projects: [
        {
          project_id: "p1",
          platform_role: "admin" as const,
          roles: ["owner", "billing", "sales"],
          databases: [{ database_id: "d2", platform_role: "viewer" as const, roles: ["billing"] }],
        },
      ],
    };
    // The project as a whole, and a database without an override, keep the grant.
    expect(projectCaps(me, "p1").role).toBe("admin");
    expect(projectCaps(me, "p1", "d1").role).toBe("admin");
    expect(projectCaps(me, "p1", "d1").roles).toEqual(["owner", "billing", "sales"]);
    // The narrowed one is what the override leaves.
    const narrowed = projectCaps(me, "p1", "d2");
    expect(narrowed.role).toBe("viewer");
    expect(narrowed.roles).toEqual(["billing"]);
    expect(narrowed.atLeast("developer")).toBe(false);
    // A tenant admin is never narrowed; no grant stays no access.
    expect(projectCaps({ ...me, member: { id: "m-self", status: "active", tenant_admin: true } }, "p1", "d2").role).toBe("admin");
    expect(projectCaps(me, "other", "d2").role).toBeNull();
  });

  it("allows everything while me isn't known (the API decides)", () => {
    expect(projectCaps(undefined, "p1").atLeast("admin")).toBe(true);
  });
});

describe("noAccess", () => {
  const member = (status: "active" | "pending", tenant_admin = false) => ({ id: "m", status, tenant_admin });
  const bound = { ...base, tenant: { id: "t1", slug: "demo", display_name: null, projects: [] } };

  it("is true for an active non-admin with no project, and for an org user with no member row (M6)", () => {
    expect(noAccess({ ...bound, member: member("active") })).toBe(true);
    expect(noAccess({ ...bound, member: null })).toBe(true);
  });

  it("is false with a project, as a tenant admin, pending, or before the org is bound", () => {
    const granted = [{ project_id: "p1", platform_role: "viewer" as const, roles: [], databases: [] }];
    expect(noAccess({ ...bound, member: member("active"), projects: granted })).toBe(false);
    expect(noAccess({ ...bound, member: member("active", true) })).toBe(false);
    expect(noAccess({ ...bound, member: member("pending") })).toBe(false);
    expect(noAccess({ ...base, member: null, tenant: null })).toBe(false);
  });
});

describe("memberGate", () => {
  // R40, M6, R49: the statuses whose whole app gives way to one screen.
  it("names the screen for pending, removed and invited (an expired invite), none for active", () => {
    const of = (status: "active" | "pending" | "removed" | "invited") => memberGate({ ...base, member: { id: "m", status, tenant_admin: false } });
    expect([of("pending"), of("removed"), of("invited"), of("active")]).toEqual(["pending", "removed", "invite-expired", null]);
    expect(memberGate({ ...base, member: null })).toBeNull();
    expect(memberGate(undefined)).toBeNull();
  });
});

describe("actAsRoles", () => {
  it("role_picker_lists_allowed_roles: admin gets the owner and every role", () => {
    const caps = projectCaps({ ...base, member: { id: "m-self", status: "active", tenant_admin: true } }, "p1");
    // R18: every role, but not the data API's anon and authenticated (a grant must name them).
    expect(actAsRoles(caps, roles)).toEqual({ owner: true, roles: ["billing_admin", "billing", "reports", "app_login"] });
  });

  it("role_picker_lists_allowed_roles: below admin only granted roles and their chains, never the owner or a LOGIN role", () => {
    const me = {
      ...base,
      projects: [{ project_id: "p1", platform_role: "developer" as const, roles: ["billing_admin", "app_login", "anon", "gone"], databases: [] }],
    };
    expect(actAsRoles(projectCaps(me, "p1"), roles)).toEqual({ owner: false, roles: ["billing_admin", "billing", "anon"] });
  });

  it("a cycle in memberships ends", () => {
    const cyclic = [role("a", { member_of: ["b"] }), role("b", { member_of: ["a"] })];
    const me = { ...base, projects: [{ project_id: "p1", platform_role: "viewer" as const, roles: ["a"], databases: [] }] };
    expect(actAsRoles(projectCaps(me, "p1"), cyclic)).toEqual({ owner: false, roles: ["a", "b"] });
  });
});
