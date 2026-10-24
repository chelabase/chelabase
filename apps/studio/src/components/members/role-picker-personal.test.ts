import { describe, expect, it } from "vitest";
import type { BranchRole } from "@chelabase/management-client";
import { roleChoices } from "./role-picker";

function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false,
    ...extra,
  } as BranchRole;
}

describe("roleChoices", () => {
  it("personal_logins_are_never_role_choices", () => {
    const orders = [role("postgres", { origin: "owner", level: "owner_level", login: true }), role("billing"), role("sofia_pg", { origin: "personal", login: true })];
    const events = [role("sofia_pg", { origin: "personal", login: true }), role("reports")];
    for (const platformRole of ["admin", "developer", "viewer"] as const) {
      const names = roleChoices([{ database: "orders", roles: orders }, { database: "events", roles: events }], platformRole).map((c) => c.value);
      expect(names.sort(), platformRole).toEqual(["billing", "reports"]);
    }
  });

  it("does not offer a personal login as part of another role's 'also gives'", () => {
    const rows = [role("sofia_pg", { origin: "personal", login: true }), role("billing", { member_of: ["sofia_pg"] })];
    const c = roleChoices([{ database: "orders", roles: rows }], "admin").find((x) => x.value === "billing");
    expect(c).toBeDefined();
    expect(c!.chain).not.toContain("sofia_pg");
  });
});
