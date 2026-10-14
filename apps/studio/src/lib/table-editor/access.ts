import type { Branch, BranchDatabaseView } from "@chelabase/management-client";
import type { ProjectCaps } from "@/lib/capabilities";

export type StructureAccess = { allowed: true } | { allowed: false; reason: string };

/** Whether the caller may change the schema on this branch (the API still decides). */
export function structureAccess(caps: ProjectCaps, branch: Branch): StructureAccess {
  if (caps.role === null) return { allowed: false, reason: "You don't have access to this project" };
  if (!caps.atLeast("developer")) return { allowed: false, reason: "Viewers can't change the schema" };
  if (branch.protected && !caps.atLeast("admin")) {
    return { allowed: false, reason: `${branch.name} is protected: schema changes need Admin` };
  }
  return { allowed: true };
}

export interface RowAccess {
  read: boolean;
  write: boolean;
  reason: string | null;
}

/** Whether rows can be read and written through the data API as the chosen role. */
export function rowAccess(caps: ProjectCaps, actAs: { owner: boolean; roles: string[] }, db: BranchDatabaseView): RowAccess {
  if (caps.role === null) return { read: false, write: false, reason: "You don't have access to this project" };
  if (db.data_api !== "on") return { read: false, write: false, reason: `Rows need the data API on ${db.name}` };
  if (!actAs.owner && actAs.roles.length === 0) {
    return { read: false, write: false, reason: "You can't act as any Postgres role here." };
  }
  return { read: true, write: caps.atLeast("developer"), reason: null };
}
