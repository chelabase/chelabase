import type { Team } from "@chelabase/management-client";

export const NAME_MAX = 80;
export const DESCRIPTION_MAX = 500;

/** A team's name for display: the stored name, else a placeholder. Callers render it as text (never as markup). */
export function teamName(team: Pick<Team, "name">): string {
  return team.name.trim() || "Unnamed team";
}

const chars = (s: string) => Array.from(s).length;

export interface TeamFormErrors {
  name?: string;
  description?: string;
}

/** The server's rules (P2) said before the request: a trimmed name of 1 to 80 characters, a description of at most 500. */
export function validateTeamForm(form: { name: string; description: string }): TeamFormErrors | null {
  const errors: TeamFormErrors = {};
  const name = form.name.trim();
  if (name === "") errors.name = "Give the team a name.";
  else if (chars(name) > NAME_MAX) errors.name = `Use at most ${NAME_MAX} characters for the name.`;
  if (chars(form.description) > DESCRIPTION_MAX) errors.description = `Use at most ${DESCRIPTION_MAX} characters for the description.`;
  return errors.name || errors.description ? errors : null;
}

const count = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;
export const membersText = (n: number) => count(n, "member", "members");
export const projectsText = (n: number) => count(n, "project", "projects");

/** What deleting a team takes away, in words (the confirm names how many members get access through it). */
export function deleteConsequence(team: Pick<Team, "name" | "member_count" | "projects">): string {
  const name = teamName(team);
  if (team.projects.length === 0) return `${name} gives no project access, so nobody loses any. Its ${membersText(team.member_count)} stay in the tenant.`;
  if (team.member_count === 0) return `${name} has no members, so nobody loses access.`;
  const verb = team.member_count === 1 ? "gets" : "get";
  return `${membersText(team.member_count)} ${verb} access to ${projectsText(team.projects.length)} through ${name}. They lose it when the team is deleted; access they hold on their own stays.`;
}

/** Sorted, case-insensitively, by name: the order the API answers in. */
export function byName<T extends { name: string }>(teams: readonly T[]): T[] {
  return [...teams].sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
}
