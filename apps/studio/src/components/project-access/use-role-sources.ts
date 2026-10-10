import { useQueries } from "@tanstack/react-query";
import { getListBranchesQueryOptions, getListRolesQueryOptions, type DatabaseSummary } from "@chelabase/management-client";
import { roleChoices, type RoleSource } from "@/components/members/role-picker";

/**
 * Each ready database's main branch roles (Q3), on the same queries the role picker uses, so the
 * table's chains cost no extra requests. Viewer-level routes: safe for every caller.
 */
export function useRoleSources(databases: readonly DatabaseSummary[]): { sources: RoleSource[]; loading: boolean } {
  const ready = databases.filter((d) => d.status === "ready");
  const branches = useQueries({ queries: ready.map((d) => getListBranchesQueryOptions(d.database_id)) });
  const mains = ready.map((d, i) => ({ db: d, main: branches[i].data?.find((b) => b.parent_branch_id === null) ?? null }));
  const roles = useQueries({
    queries: mains.map(({ db, main }) => getListRolesQueryOptions(db.database_id, main?.id ?? "", { query: { enabled: main !== null } })),
  });
  const sources: RoleSource[] = [];
  let loading = false;
  mains.forEach(({ db }, i) => {
    if (roles[i].data) sources.push({ database: db.name, roles: roles[i].data });
    else if (!branches[i].error && !roles[i].error) loading = true;
  });
  return { sources, loading };
}

/** role name → the roles it also gives (its `SET ROLE` chain, R21), merged across databases. */
export function chainsOf(sources: readonly RoleSource[]): Map<string, string[]> {
  return new Map(roleChoices(sources, "admin").map((c) => [c.value, c.chain]));
}
