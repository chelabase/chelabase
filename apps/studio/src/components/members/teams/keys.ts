import type { QueryClient } from "@tanstack/react-query";
import { refreshAccess } from "../keys";

const isTeamKey = (key: readonly unknown[]) => {
  const k = key[0];
  return typeof k === "string" && (k === "/v1/teams" || k.startsWith("/v1/teams/") || (k.startsWith("/v1/members/") && k.endsWith("/teams")) || k.endsWith("/team-grants"));
};

/** After any team write: every team read (lists, a team, a member's teams, a project's team grants), the roster's access and the log. */
export function refreshTeams(queryClient: QueryClient): void {
  void queryClient.invalidateQueries({ predicate: (q) => isTeamKey(q.queryKey) });
  refreshAccess(queryClient);
}
