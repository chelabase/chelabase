import { createContext, useContext } from "react";
import type { BranchRole, MeResponse as Me, PlatformRole } from "@chelabase/management-client";
import { actAsReserved, closure, refusal } from "./role-rules";

/**
 * What the caller may do on one project, read from `/v1/me`. Studio hides what this rules out;
 * the API still decides, and a 403 still shows its message.
 */
export interface ProjectCaps {
  /** The effective platform role; null when the caller has no access to the project. */
  role: PlatformRole | null;
  /**
   * `"all"` for an Admin (a tenant admin, or a project Admin, R17; never the data API's roles,
   * R18), else the granted role names (`owner` may be listed with Admin).
   */
  roles: "all" | string[];
  tenantAdmin: boolean;
  /**
   * The level each named role is held at (D1), from `role_levels`. Under `"all"` it lists only
   * the roles held below Admin (a named data API role); every other role is at `role`. Empty
   * when the API gives none: every role is then at `role`.
   */
  roleLevels: Record<string, PlatformRole>;
  atLeast: (level: PlatformRole) => boolean;
}

const RANK: Record<PlatformRole, number> = { viewer: 0, developer: 1, admin: 2 };

function caps(
  role: PlatformRole | null,
  roles: "all" | string[],
  tenantAdmin: boolean,
  roleLevels: Record<string, PlatformRole> = {},
): ProjectCaps {
  return { role, roles, tenantAdmin, roleLevels, atLeast: (level) => role !== null && RANK[role] >= RANK[level] };
}

/** Everything allowed: used while `me` is unknown and outside a project (the API decides). */
export const ALL_CAPS: ProjectCaps = caps("admin", "all", true);

/**
 * The caller's caps on a project, or, with `databaseId`, on one of its databases: where the grant
 * is narrowed there (`/v1/me` lists those in `projects[].databases`) the role and roles are the
 * narrowed ones, else the grant's (I3).
 */
export function projectCaps(me: Me | undefined, projectId: string, databaseId?: string): ProjectCaps {
  if (!me) return ALL_CAPS;
  const tenantAdmin = me.member?.tenant_admin === true;
  if (tenantAdmin) return caps("admin", "all", true);
  const grant = (me.projects ?? []).find((p) => p.project_id === projectId);
  if (!grant) return caps(null, [], false);
  const narrowed = databaseId ? (grant.databases ?? []).find((d) => d.database_id === databaseId) : undefined;
  // A field the narrowing leaves out is the grant's.
  if (narrowed) {
    return caps(narrowed.platform_role ?? grant.platform_role, narrowed.roles ?? grant.roles, false, narrowed.role_levels ?? grant.role_levels ?? {});
  }
  return caps(grant.platform_role, grant.roles, false, grant.role_levels ?? {});
}

/**
 * The caps while acting as `role` (null: the owner, which keeps the caller's level): the level
 * is the one the role is held at, a role its chain reaches counting at the granting role's level
 * (R21), so a Viewer-level role gets the Viewer gates whatever the platform level is.
 */
export function actingCaps(c: ProjectCaps, role: string | null, branchRoles: BranchRole[]): ProjectCaps {
  if (role === null || c.role === null) return c;
  let level: PlatformRole | undefined;
  const hold = (l: PlatformRole) => {
    if (level === undefined || RANK[l] > RANK[level]) level = l;
  };
  // An Admin's every role is at the platform level, a named data API role at its own.
  if (c.roles === "all") level = c.roleLevels[role] ?? c.role;
  else if (Object.keys(c.roleLevels).length === 0) return c;
  else {
    for (const [granted, l] of Object.entries(c.roleLevels)) {
      if (granted === role || closure(branchRoles, granted).has(role)) hold(l);
    }
  }
  const acted = level ?? "viewer";
  // Never above the caller's own level on the database.
  return caps(RANK[acted] < RANK[c.role] ? acted : c.role, c.roles, c.tenantAdmin, c.roleLevels);
}

/**
 * A member with no project who isn't a tenant admin, or a signed-in person of the org with no
 * member row at all (sign-up closed, no invite; M6): the no-access screen stands in for Projects.
 */
export function noAccess(me: Me): boolean {
  const none = Array.isArray(me.projects) && me.projects.length === 0;
  if (me.tenant === null || !none) return false;
  return me.member === null || (me.member.status === "active" && !me.member.tenant_admin);
}

/**
 * The member statuses whose whole app gives way to one screen: `pending` (waiting for approval,
 * R40), `removed` (M6) and `invited` (signed in on an invite that expired before it was
 * accepted, R49). Null for everyone else.
 */
export function memberGate(me: Me | undefined): "pending" | "removed" | "invite-expired" | null {
  switch (me?.member?.status) {
    case "pending":
      return "pending";
    case "removed":
      return "removed";
    case "invited":
      return "invite-expired";
    default:
      return null;
  }
}

/** The project page provides its caller's caps; without a provider everything is allowed. */
export const ProjectCapsContext = createContext<ProjectCaps>(ALL_CAPS);

export function useProjectCaps(): ProjectCaps {
  return useContext(ProjectCapsContext);
}

/**
 * The roles the caller may act as on a branch, in the branch's order: only granted ones (R18).
 * - The owner with Admin.
 * - Every role (`"all"`, an Admin's, R17): every other role of the branch except reserved ones
 *   and the data API's `anon` and `authenticated`, which need a grant naming them.
 * - Named roles: those and every role their chains reach through `member_of` (R21), never
 *   reserved ones; below Admin also never the owner, LOGIN or owner-level roles, or one whose
 *   own chain is refused (R16, R22, R23). An Admin narrowed to named roles acts as exactly
 *   those (and what they reach), LOGIN roles included.
 */
export function actAsRoles(c: ProjectCaps, branchRoles: BranchRole[]): { owner: boolean; roles: string[] } {
  const others = branchRoles.filter((r) => r.origin !== "owner");
  const admin = c.atLeast("admin");
  if (c.roles === "all") {
    const usable = (r: BranchRole) => !actAsReserved(r.name) && !isDataApiRole(r.name) && (admin || refusal(branchRoles, r.name) === null);
    return { owner: admin, roles: others.filter(usable).map((r) => r.name) };
  }
  const granted = c.roles;
  // A granted role contributes its chain only when nothing in it is refused (`chain_grants`).
  const reach = new Set<string>(granted);
  for (const g of granted) {
    if (refusal(branchRoles, g) === null) for (const n of closure(branchRoles, g)) reach.add(n);
  }
  const usable = (r: BranchRole) => reach.has(r.name) && !actAsReserved(r.name) && (admin || refusal(branchRoles, r.name) === null);
  return { owner: admin, roles: others.filter(usable).map((r) => r.name) };
}

/** The data API's roles. */
export const DATA_API_ROLES = ["anon", "authenticated"] as const;

const isDataApiRole = (name: string) => (DATA_API_ROLES as readonly string[]).includes(name);

/**
 * The data API roles the caller may act as, whether or not the branch's role list shows them:
 * only those the grant names (R18). Every role (`"all"`, an Admin's) doesn't hold them.
 */
export function dataApiActAs(c: ProjectCaps): string[] {
  if (c.roles === "all") return [];
  const named = c.roles;
  return DATA_API_ROLES.filter((r) => named.includes(r));
}
