import { describe, expect, it } from "vitest";
import type { BranchRole } from "@chelabase/management-client";
import { roleChoices } from "./role-picker";

function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false,
    ...extra,
  } as BranchRole;
}

const orders = [
  role("postgres", { origin: "owner", login: true, level: "owner_level" }),
  role("billing_admin", { member_of: ["billing"] }),
  role("billing"),
  role("app_login", { login: true }),
  role("via_login", { member_of: ["app_login"] }),
  role("auditor", { bypass_rls: true }),
  role("via_owner", { member_of: ["postgres"] }),
];
const events = [role("billing"), role("reports")];

describe("roleChoices", () => {
  it("merges databases, never lists the owner, and marks Admin-only roles below Admin with their reason", () => {
    const choices = roleChoices([{ database: "orders", roles: orders }, { database: "events", roles: events }], "developer");
    const by = Object.fromEntries(choices.map((c) => [c.value, c]));
    expect(Object.keys(by).sort()).toEqual(["app_login", "auditor", "billing", "billing_admin", "reports", "via_login", "via_owner"]);
    expect(by.billing_admin).toMatchObject({ chain: ["billing"], detail: "also gives billing · only on orders", disabledReason: undefined });
    expect(by.billing.detail).toBeUndefined();
    expect(by.reports.detail).toBe("only on events");
    expect(by.app_login.disabledReason).toBe("Admin only: LOGIN role");
    expect(by.auditor.disabledReason).toBe("Admin only: Bypasses RLS");
    expect(by.via_login.disabledReason).toBe("Admin only: Reaches app_login, a LOGIN role");
    expect(by.via_owner.disabledReason).toBe("Admin only: Reaches postgres, which is owner-level");
  });

  // I2: "also gives" is what the role can SET ROLE into (SET edges); a membership without SET
  // is not listed there, but still disables the role when the group it reaches is refused.
  it("lists only SET memberships as what a role also gives, and refuses on every membership", () => {
    const rows = [
      role("postgres", { origin: "owner", login: true, level: "owner_level" }),
      role("team", { member_of: ["plain", "inherited"], member_no_set: ["inherited"] }),
      role("plain"),
      role("inherited"),
      role("sneaky", { member_of: ["postgres"], member_no_set: ["postgres"] }),
      role("lead", { member_of: ["plain"], member_admin: ["plain"] }),
    ];
    const by = Object.fromEntries(roleChoices([{ database: "orders", roles: rows }], "developer").map((c) => [c.value, c]));
    expect(by.team.chain).toEqual(["plain"]);
    expect(by.team.disabledReason).toBeUndefined();
    expect(by.sneaky.chain).toEqual([]);
    expect(by.sneaky.disabledReason).toBe("Admin only: Reaches postgres, which is owner-level");
    expect(by.lead.disabledReason).toBe("Admin only: Holds ADMIN on a role");
  });

  it("offers every listed role to Admin", () => {
    const choices = roleChoices([{ database: "orders", roles: orders }], "admin");
    expect(choices.every((c) => c.disabledReason === undefined)).toBe(true);
  });
});
