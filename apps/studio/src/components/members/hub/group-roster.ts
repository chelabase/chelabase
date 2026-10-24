import type { Member, PlatformRole } from "@chelabase/management-client";

/** The MH-35 groups; with a project filter the same keys mean that project's Admins, Developers, Viewers and "No access on <project>". */
export type GroupKey = "tenant_admins" | "admins" | "developers" | "viewers" | "other" | "none" | "invited" | "suspended";

export interface Group {
  key: GroupKey;
  title: string;
  members: Member[];
}

const ORDER: GroupKey[] = ["tenant_admins", "admins", "developers", "viewers", "other", "none", "invited", "suspended"];
const PLAIN: Record<GroupKey, string> = {
  tenant_admins: "Tenant admins",
  admins: "Project admins",
  developers: "Developers",
  viewers: "Viewers",
  other: "Other projects",
  none: "No access yet",
  invited: "Invited",
  suspended: "Suspended",
};
const FILTERED: Partial<Record<GroupKey, string>> = {
  admins: "Admins on",
  developers: "Developers on",
  viewers: "Viewers on",
  none: "No access on",
};
const BY_ROLE: Record<PlatformRole, GroupKey> = { admin: "admins", developer: "developers", viewer: "viewers" };

/** The group's title; with a project, "Admins on {project}" and so on (Invited and Suspended keep theirs). */
function titleOf(key: GroupKey, projectName: string | undefined, filtered: boolean): string {
  const filteredTitle = filtered ? FILTERED[key] : undefined;
  return filteredTitle ? `${filteredTitle} ${projectName ?? "this project"}` : PLAIN[key];
}

function keyOf(m: Member, projectId: string | undefined): GroupKey | null {
  // Status decides first: a pending request waits in Requests, a removed member isn't listed.
  if (m.status === "pending" || m.status === "removed") return null;
  if (m.status === "invited") return "invited";
  if (m.status === "suspended") return "suspended";
  const level = m.access.level;
  if (projectId === undefined) {
    if (level === "tenant_admin" || m.tenant_admin) return "tenant_admins";
    return level === "admin" ? "admins" : level === "developer" ? "developers" : level === "viewer" ? "viewers" : level === "other" ? "other" : "none";
  }
  // A tenant admin is Admin on every project.
  if (level === "tenant_admin" || m.tenant_admin) return "admins";
  const role = m.access.projects.find((p) => p.project_id === projectId)?.platform_role;
  return role ? BY_ROLE[role] : "none";
}

/** The roster in the MH-35 groups, empty ones left out; input order is kept inside a group. Pure. */
export function groupRoster(members: readonly Member[], opts: { projectId?: string; projectName?: string } = {}): Group[] {
  const buckets = new Map<GroupKey, Member[]>();
  for (const m of members) {
    const key = keyOf(m, opts.projectId);
    if (key === null) continue;
    buckets.set(key, [...(buckets.get(key) ?? []), m]);
  }
  return ORDER.filter((k) => buckets.has(k)).map((key) => ({
    key,
    title: titleOf(key, opts.projectName, opts.projectId !== undefined),
    members: buckets.get(key)!,
  }));
}
