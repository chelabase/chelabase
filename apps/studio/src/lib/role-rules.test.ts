import { describe, expect, it } from "vitest";
import type { BranchRole, MeResponse as Me } from "@chelabase/management-client";
import { roleChoices } from "@/components/members/role-picker";
import { actAsRoles, projectCaps } from "./capabilities";
import { actAsReserved, closure, refusal } from "./role-rules";
import fixtures from "./role-rules.fixtures.json";

function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [],
    member_no_set: [], member_admin: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false,
    ...extra,
  } as BranchRole;
}

/** A small seeded PRNG (mulberry32): every run explores the same graphs, and a failure names its seed. */
function rng(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const NAMES = ["a", "b", "c", "d", "e", "f", "pg_x", "chelabase_y", "cb_m_z", "neon_superuser", "anon", "authenticated", "postgres"];

/** A random branch role list: the owner, then plain, LOGIN, owner-level, BYPASSRLS and reserved roles, with random memberships (cycles included). */
function graph(r: () => number): BranchRole[] {
  const pick = <T,>(xs: readonly T[]) => xs[Math.floor(r() * xs.length)];
  const names = NAMES.filter(() => r() < 0.75);
  const rows = names.map((name) =>
    role(name, {
      origin: name === "postgres" ? "owner" : "sql",
      login: name === "postgres" || r() < 0.15,
      level: name === "postgres" || r() < 0.1 ? "owner_level" : "plain",
      bypass_rls: r() < 0.1,
      member_of: [],
    }),
  );
  for (const row of rows) {
    const n = Math.floor(r() * 3);
    // Memberships may name roles that aren't listed (as 6a omits neon_superuser).
    row.member_of = [...new Set(Array.from({ length: n }, () => pick([...NAMES, "ghost"])))].filter((m) => m !== row.name);
    // Options (I2): some memberships lack SET, some carry ADMIN.
    row.member_no_set = row.member_of.filter(() => r() < 0.3);
    row.member_admin = row.member_of.filter(() => r() < 0.15);
  }
  return rows;
}

/** The reference: `access.rs`'s act-as rule, name by name, from the ported `refusal`. */
function reference(rows: BranchRole[], granted: string[]): string[] {
  const listed = rows.filter((x) => x.origin !== "owner").map((x) => x.name);
  return listed.filter((n) => {
    if (actAsReserved(n) || refusal(rows, n) !== null) return false;
    if (granted.includes(n)) return true;
    return granted.some((g) => g !== n && refusal(rows, g) === null && closure(rows, g).has(n));
  });
}

const me = (roles: string[]): Me => ({
  sub: "u", email: null, name: null, tenant_id: "t", instance_admin: false, functions_available: true, tenant_admins: [], user: { sub: "u" },
  member: { id: "m", status: "active", tenant_admin: false },
  projects: [{ project_id: "p", platform_role: "developer", roles, databases: [] }],
  tenant: { id: "t", slug: "t", display_name: null, projects: [] },
});

// I2: the orchestrator's answers on generated graphs (services/orchestrator/tests/role_rules_agree.rs
// writes the file; its proptest holds the worker to the same answers).
describe("the port against the server's fixtures", () => {
  it("has graphs", () => {
    expect(fixtures.graphs.length).toBeGreaterThan(100);
  });

  it("gives the server's refusal and closure for every role of every graph", () => {
    for (const g of fixtures.graphs) {
      const rows = g.rows as unknown as BranchRole[];
      for (const name of fixtures.names) {
        const want = (g.expected as Record<string, { refusal: unknown; closure: string[] }>)[name];
        expect(refusal(rows.map((r) => role(r.name, r)), name), `seed ${g.seed} refusal of ${name}`).toEqual(want.refusal);
        expect([...closure(rows.map((r) => role(r.name, r)), name)].sort(), `seed ${g.seed} closure of ${name}`).toEqual(want.closure);
      }
    }
  });
});

describe("options (I2)", () => {
  const rows = [
    role("postgres", { origin: "owner", login: true, level: "owner_level" }),
    role("inherits", { member_of: ["plain", "boss"], member_no_set: ["boss"] }),
    role("boss", { level: "owner_level" }),
    role("plain"),
    role("lead", { member_of: ["plain"], member_admin: ["plain"] }),
    role("via_lead", { member_of: ["lead"] }),
  ];

  it("reach follows SET edges only", () => {
    expect([...closure(rows, "inherits")].sort()).toEqual(["inherits", "plain"]);
  });

  it("refusal walks every edge, and an ADMIN holder is refused", () => {
    expect(refusal(rows, "inherits")).toEqual({ chain: ["inherits", "boss"], why: "owner_level" });
    expect(refusal(rows, "lead")).toEqual({ chain: ["lead"], why: "admin" });
    expect(refusal(rows, "via_lead")).toEqual({ chain: ["via_lead", "lead"], why: "admin" });
    expect(refusal(rows, "plain")).toBeNull();
  });
});

describe("refusal (a port of access.rs)", () => {
  it("names the shortest chain and why", () => {
    const rows = [
      role("postgres", { origin: "owner", login: true, level: "owner_level" }),
      role("reports", { member_of: ["app_login"] }),
      role("app_login", { login: true }),
      role("billing_admin", { member_of: ["billing"] }),
      role("billing", { member_of: ["postgres"] }),
      role("audit", { bypass_rls: true }),
      role("ops", { member_of: ["pg_read_all_data"] }),
      role("plain"),
    ];
    expect(refusal(rows, "reports")).toEqual({ chain: ["reports", "app_login"], why: "login" });
    expect(refusal(rows, "billing_admin")).toEqual({ chain: ["billing_admin", "billing", "postgres"], why: "owner_level" });
    expect(refusal(rows, "audit")).toEqual({ chain: ["audit"], why: "owner_level" });
    expect(refusal(rows, "ops")).toEqual({ chain: ["ops", "pg_read_all_data"], why: "reserved" });
    expect(refusal(rows, "plain")).toBeNull();
    // reports → app_login: granting reports gives nothing below Admin.
    const caps = projectCaps(me(["reports", "plain"]), "p");
    expect(actAsRoles(caps, rows)).toEqual({ owner: false, roles: ["plain"] });
  });

  it("actAsRoles and the role picker agree with refusal on random graphs", () => {
    for (let seed = 1; seed <= 400; seed++) {
      const r = rng(seed);
      const rows = graph(r);
      const granted = NAMES.filter(() => r() < 0.4);
      const got = actAsRoles(projectCaps(me(granted), "p"), rows).roles;
      expect([...got].sort(), `seed ${seed}`).toEqual(reference(rows, granted).sort());
      // The picker never lists the owner or a reserved name, and disables exactly the roles
      // refusal refuses (below Admin).
      const choices = roleChoices([{ database: "d", roles: rows }], "developer");
      for (const c of choices) {
        expect(actAsReserved(c.value), `seed ${seed} ${c.value}`).toBe(false);
        expect(c.disabledReason !== undefined, `seed ${seed} ${c.value}`).toBe(refusal(rows, c.value) !== null);
      }
      expect(choices.some((c) => rows.find((x) => x.name === c.value)?.origin === "owner"), `seed ${seed}`).toBe(false);
    }
  });
});
