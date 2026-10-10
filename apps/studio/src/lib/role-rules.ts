import type { BranchRole } from "@chelabase/management-client";

/**
 * A port of the orchestrator's single rule for "a non-Admin member may hold or become this
 * role" (`services/orchestrator/src/access.rs`, `RoleGraph::refusal`, R16, R21–R23, R30). Studio
 * uses it only to hide what the API would refuse; the API decides.
 *
 * The invariant (I2): `closure` (what a role can become) follows SET memberships only;
 * `refusal` (what makes a chain unsafe) walks every membership, whatever its options, and
 * counts a role that holds ADMIN on another role. `role-rules.fixtures.json`, written by
 * `services/orchestrator/tests/role_rules_agree.rs`, holds graphs with the server's answers,
 * and `role-rules.test.ts` checks this port against them.
 */

/** `chelabase_types::role_name::RESERVED_ROLE_NAMES`. */
const RESERVED_ROLE_NAMES = [
  "owner",
  "postgres",
  "anon",
  "authenticated",
  "authenticator",
  "chelabase_admin",
  "cloud_admin",
  "neon_superuser",
  "public",
  "none",
  "chelabase_auth_sync",
];

/** Roles that make whoever reaches them owner-level, besides the database's owner. */
const OWNER_LEVEL_ROLES = ["neon_superuser", "cloud_admin"];

/** `chelabase_types::access::act_as_reserved`: names no member acts as or is granted. */
export function actAsReserved(name: string): boolean {
  if (name === "anon" || name === "authenticated") return false;
  const lower = name.toLowerCase();
  return (
    lower === "owner" ||
    lower.startsWith("pg_") ||
    lower.startsWith("cb_m_") ||
    lower.startsWith("chelabase_") ||
    RESERVED_ROLE_NAMES.includes(lower)
  );
}

export type RefusalReason = "owner_level" | "login" | "admin" | "reserved";

export interface Refusal {
  /** From the role itself to the refused one, inclusive. */
  chain: string[];
  why: RefusalReason;
}

/** The branch's owner: the row 6a marks as the owner. */
function ownerOf(rows: readonly BranchRole[]): string | null {
  return rows.find((r) => r.origin === "owner")?.name ?? null;
}

/** Why `role` itself may not be in a non-Admin member's chain (`RoleGraph::refused`). */
function refused(rows: readonly BranchRole[], owner: string | null, role: string): RefusalReason | null {
  const row = rows.find((r) => r.name === role);
  if (role === owner || OWNER_LEVEL_ROLES.includes(role) || (row && (row.level === "owner_level" || row.bypass_rls))) {
    return "owner_level";
  }
  if (row?.login) return "login";
  if (row && (row.member_admin ?? []).length > 0) return "admin";
  if (actAsReserved(role)) return "reserved";
  return null;
}

/**
 * The shortest chain from `name` (itself included) to a role that is owner-level, LOGIN, holds
 * ADMIN on a role, or is reserved, and why; `null` when `name` and everything it is a member of
 * (every membership, whatever its options) are plain NOLOGIN groups. Memberships are followed in
 * their listed order, breadth first, as the server does.
 */
export function refusal(rows: readonly BranchRole[], name: string): Refusal | null {
  const owner = ownerOf(rows);
  const parent = new Map<string, string | null>([[name, null]]);
  const queue = [name];
  while (queue.length > 0) {
    const role = queue.shift()!;
    const why = refused(rows, owner, role);
    if (why) {
      const chain = [role];
      for (let up = parent.get(role); up; up = parent.get(up)) chain.push(up);
      return { chain: chain.reverse(), why };
    }
    for (const next of rows.find((r) => r.name === role)?.member_of ?? []) {
      if (!parent.has(next)) {
        parent.set(next, role);
        queue.push(next);
      }
    }
  }
  return null;
}

/** `name` and every role it reaches through memberships with the SET option (`RoleGraph::closure`). */
export function closure(rows: readonly BranchRole[], name: string): Set<string> {
  const seen = new Set<string>();
  const stack = [name];
  while (stack.length > 0) {
    const role = stack.pop()!;
    if (seen.has(role)) continue;
    seen.add(role);
    const row = rows.find((r) => r.name === role);
    const noSet = row?.member_no_set ?? [];
    stack.push(...(row?.member_of ?? []).filter((g) => !noSet.includes(g)));
  }
  return seen;
}

/** The refusal in words, as the role picker shows it. */
export function refusalText(r: Refusal): string {
  const last = r.chain[r.chain.length - 1];
  if (r.chain.length === 1) {
    return { login: "LOGIN role", admin: "Holds ADMIN on a role", reserved: "Reserved", owner_level: "Owner-level" }[r.why];
  }
  if (r.why === "login") return `Reaches ${last}, a LOGIN role`;
  if (r.why === "admin") return `Reaches ${last}, which holds ADMIN on a role`;
  return `Reaches ${last}, which is ${r.why === "reserved" ? "reserved" : "owner-level"}`;
}
