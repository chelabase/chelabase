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
  /** `"all"` for a tenant admin, else the granted role names (owner never listed: Admin implies it). */
  roles: "all" | string[];
  tenantAdmin: boolean;
  atLeast: (level: PlatformRole) => boolean;
}

const RANK: Record<PlatformRole, number> = { viewer: 0, developer: 1, admin: 2 };

function caps(role: PlatformRole | null, roles: "all" | string[], tenantAdmin: boolean): ProjectCaps {
  return { role, roles, tenantAdmin, atLeast: (level) => role !== null && RANK[role] >= RANK[level] };
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
  if (narrowed) return caps(narrowed.platform_role, narrowed.roles, false);
  return caps(grant.platform_role, grant.roles, false);
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
 * The roles the caller may act as on a branch, in the branch's order.
 * Admin: the owner and every other role. Below Admin: the granted roles and every role their
 * chains reach through `member_of` (R21), minus the owner, LOGIN roles and owner-level roles (R16, R22).
 */
export function actAsRoles(c: ProjectCaps, branchRoles: BranchRole[]): { owner: boolean; roles: string[] } {
  const others = branchRoles.filter((r) => r.origin !== "owner");
  if (c.atLeast("admin")) return { owner: true, roles: others.map((r) => r.name) };
  const granted = c.roles === "all" ? others.map((r) => r.name) : c.roles;
  // A granted role contributes its chain only when nothing in it is refused (`chain_grants`).
  const reach = new Set<string>(granted);
  for (const g of granted) {
    if (refusal(branchRoles, g) === null) for (const n of closure(branchRoles, g)) reach.add(n);
  }
  // Only listed roles, never reserved ones, and never one whose own chain is refused (R16, R21–R23).
  const usable = (r: BranchRole) => reach.has(r.name) && !actAsReserved(r.name) && refusal(branchRoles, r.name) === null;
  return { owner: false, roles: others.filter(usable).map((r) => r.name) };
}
