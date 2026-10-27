import type { TeamRef } from "../hub/team-via";
import type { AccessVia, PlatformRole, Team } from "@chelabase/management-client";

/** What a team gives the member on one project (the `listMemberTeams` rows, narrowed to the project). */
export interface TeamOnProject {
  teamId: string;
  name: string;
  platformRole: PlatformRole;
  /** `null` when the caller may not see the team's roles here (P11): no locked rows, but the level still counts. */
  pgRoles: string[] | null;
}

const RANK: Record<PlatformRole, number> = { viewer: 0, developer: 1, admin: 2 };

/** The teams of the member that grant something on `projectId`, by name. */
export function teamsOnProject(teams: readonly Team[], projectId: string): TeamOnProject[] {
  const out: TeamOnProject[] = [];
  for (const t of teams) {
    const p = t.projects.find((x) => x.project_id === projectId);
    if (p) out.push({ teamId: t.id, name: t.name, platformRole: p.platform_role, pgRoles: p.pg_roles });
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}

/** A role a team gives, with every team that names it (by name), for the row's marker. */
export interface LockedRole {
  value: string;
  teams: TeamRef[];
}

/** The member page's team-given rows: each role a team gives once, with every team that names it. */
export function lockedRoles(onProject: readonly TeamOnProject[]): LockedRole[] {
  const seen = new Map<string, TeamRef[]>();
  for (const t of onProject) {
    for (const r of t.pgRoles ?? []) seen.set(r, [...(seen.get(r) ?? []), { id: t.teamId, name: t.name }]);
  }
  return [...seen].map(([value, teams]) => ({ value, teams }));
}

/** The highest platform role among the own grant (when any) and the teams'. */
export function effectiveRole(own: PlatformRole | null, onProject: readonly TeamOnProject[]): PlatformRole | null {
  let best = own;
  for (const t of onProject) if (best === null || RANK[t.platformRole] > RANK[best]) best = t.platformRole;
  return best;
}

/** "Support team", "Billing and Support teams", "Billing, Ledger auditors and Support teams". */
export function viaWords(onProject: readonly TeamOnProject[]): string {
  const names = onProject.map((t) => t.name);
  if (names.length <= 1) return names.map((n) => `${n} team`).join("");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]} teams`;
}

/** The Effective line's "(+ …)": the roles each team gives, grouped per team, in team order (only teams that give some). */
export function rolesPerTeam(onProject: readonly TeamOnProject[]): { team: TeamRef; roles: string[] }[] {
  return onProject.filter((t) => (t.pgRoles ?? []).length > 0).map((t) => ({ team: { id: t.teamId, name: t.name }, roles: t.pgRoles ?? [] }));
}

/** The level a team gives when it is higher than `own` (null when none is), with the teams giving it. */
export function teamLevel(own: PlatformRole | null, onProject: readonly TeamOnProject[]): { role: PlatformRole; teams: TeamRef[] } | null {
  const best = effectiveRole(null, onProject);
  if (best === null || (own !== null && RANK[best] <= RANK[own])) return null;
  return { role: best, teams: onProject.filter((t) => t.platformRole === best).map((t) => ({ id: t.teamId, name: t.name })) };
}

/** Where a card's access comes from: nothing to say for the member's own grant, else the teams that give it. */
export function provenanceNote(via: AccessVia | undefined, onProject: readonly TeamOnProject[]): string | null {
  if (via === undefined || via === "own" || onProject.length === 0) return null;
  const teams = viaWords(onProject);
  return via === "team" ? `Access through ${teams}` : `Own grant and ${teams}`;
}
