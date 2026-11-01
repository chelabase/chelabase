import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  useGetDatabase,
  useGetMe,
  useListBranchDatabases,
  useListBranches,
  useListProjects,
  useListRoles,
  type Branch,
  type Project,
} from "@chelabase/management-client";
import { DATA_API_ROLES, actAsRoles, dataApiActAs, projectCaps, type ProjectCaps } from "@/lib/capabilities";
import { isTransitional } from "@/lib/branches";
import { pollWhile } from "@/lib/query";
import { errorText } from "@/lib/errors";
import { useRealtime } from "@/lib/realtime/context";
import { fallbackInterval, invalidateAll, invalidateForEvent } from "@/lib/realtime/events";
import { branchTree, type EditorParams } from "./context";
import { useTableEditor } from "./store";

/**
 * The projects with their databases. `/v1/projects` lists the projects; the caller's databases
 * per project come with `/v1/me` (`tenant.projects`). Absent until both have loaded.
 */
function withDatabases(list: ProjectInfoLike[] | undefined, me: { tenant: { projects: Project[] } | null } | undefined): Project[] | undefined {
  if (!list || !me) return undefined;
  const mine = new Map((me.tenant?.projects ?? []).map((p) => [p.id, p]));
  return list.map((p) => ({ ...p, databases: mine.get(p.id)?.databases ?? p.databases ?? [] }) as Project);
}
type ProjectInfoLike = { id: string; name: string; slug: string; status: Project["status"]; databases?: Project["databases"] };

/**
 * Act as's roles with the data API's last (the default stays the first role of the caller's own):
 * `anon` and `authenticated` only when the grant names them (or a granted role's chain reaches
 * them), even when the branch's role list doesn't show them (R18: an Admin's every role doesn't
 * hold them).
 */
function withDataApiRoles(caps: ProjectCaps, allowed: { owner: boolean; roles: string[] }) {
  const api: string[] = DATA_API_ROLES.filter((r) => dataApiActAs(caps).includes(r) || allowed.roles.includes(r));
  return { owner: allowed.owner, roles: [...allowed.roles.filter((r) => !api.includes(r)), ...api] };
}

/**
 * The project's `project:` channel (§5.3): its events invalidate exactly the queries they concern;
 * a rejoin after a reconnect, or a `resync`, invalidates everything the editor shows. True while joined.
 */
export function useProjectChannel(projectId: string | undefined, databaseId: string | undefined, branchId: string | undefined): boolean {
  const realtime = useRealtime();
  const queryClient = useQueryClient();
  const [live, setLive] = useState(false);
  const scope = useRef({ databaseId: "", branchId: "" });
  scope.current = { databaseId: databaseId ?? "", branchId: branchId ?? "" };
  useEffect(() => {
    if (!realtime || !projectId) return;
    let joinedBefore = false;
    const sub = realtime.joinProject(projectId, {
      onEvent: (event) => invalidateForEvent(queryClient, event),
      onResync: () => invalidateAll(queryClient, scope.current),
      onLive: (next) => {
        setLive(next);
        if (!next) return;
        // Events may have been missed while the socket was down.
        if (joinedBefore) invalidateAll(queryClient, scope.current);
        joinedBefore = true;
      },
      onError: () => setLive(false),
    });
    return () => {
      sub.leave();
      setLive(false);
    };
  }, [realtime, projectId, queryClient]);
  return live;
}

/**
 * Where the caller is and what they may act as: the choices for each level of `params` (projects,
 * branches, Postgres databases, roles), the project channel, and the Act as role. Act as is kept per
 * database and branch in the table editor's store, so every view using this hook shares the choice.
 */
export function usePlace(params: EditorParams) {
  // P10: while the project channel is live, events drive the refetches and a slow poll is the fallback.
  const projectLive = useProjectChannel(params.projectId, params.databaseId, params.branchId);
  const me = useGetMe({ query: { refetchInterval: fallbackInterval(projectLive, false) } });
  const projectList = useListProjects();
  const projects = withDatabases(projectList.data, me.data);
  const project = projects?.find((p) => p.id === params.projectId);
  const databaseOk = !!params.databaseId && !!project?.databases.some((d) => d.database_id === params.databaseId);
  const databaseId = databaseOk ? params.databaseId! : "";
  const detail = useGetDatabase(databaseId, { query: { enabled: databaseOk } });
  // Only a branch created here is polled until it settles; opening the Branch picker refetches the list.
  // While live, branch events invalidate the list instead (the slow poll stays as the fallback).
  const [createdBranchId, setCreatedBranchId] = useState<string | null>(null);
  const settling = pollWhile<Branch[]>((bs) => bs.some((b) => b.id === createdBranchId && isTransitional(b)));
  const branches = useListBranches(databaseId, {
    query: {
      enabled: databaseOk,
      refetchInterval: (query) => fallbackInterval(projectLive, settling(query)),
    },
  });
  const branch = branches.data?.find((b) => b.id === params.branchId);
  const branchOk = databaseOk && !!branch && branchTree(branches.data ?? []).some((r) => r.branch.id === branch.id);
  const branchId = branchOk ? params.branchId! : "";
  const pgDatabases = useListBranchDatabases(databaseId, branchId, { query: { enabled: branchOk } });
  // The system database (`postgres`) is the platform's: the table API refuses it, so it isn't offered (M2).
  const pgList = pgDatabases.data?.filter((d) => d.origin !== "system");
  const pg = pgList?.find((d) => d.name === params.pgDatabase);
  const roles = useListRoles(databaseId, branchId, { query: { enabled: branchOk } });

  const caps = projectCaps(me.data, params.projectId ?? "", params.databaseId);
  const actAsKey = `${params.databaseId}/${params.branchId}`;
  const picked = useTableEditor((s) => s.actAs[actAsKey]);
  const setActAsRole = useTableEditor((s) => s.setActAs);
  // A roles error degrades to no roles (the owner stays for an Admin); the error shows by Act as.
  const allowed = withDataApiRoles(caps, actAsRoles(caps, roles.isSuccess ? roles.data : []));
  // The chosen role while the caller may still act as it, else the owner (Admin) or the first allowed role.
  const chosenOk = picked === null ? allowed.owner : picked !== undefined && allowed.roles.includes(picked);
  const actAs = chosenOk ? picked! : allowed.owner ? null : (allowed.roles[0] ?? null);
  const actAsReady = branchOk && me.isSuccess && (roles.isSuccess || roles.isError);
  const rolesError = branchOk && roles.isError ? errorText(roles.error) : null;
  // Why nothing may go out as the Act as role: it isn't known yet (it would go as the owner), or there is none.
  const actAsReason = !actAsReady ? "Loading roles…" : !allowed.owner && actAs === null ? "You can't act as any Postgres role here." : null;

  return {
    projectLive,
    me,
    projectList,
    projects,
    project,
    databaseOk,
    detail,
    createdBranchId,
    setCreatedBranchId,
    branches,
    branch,
    branchOk,
    pgDatabases,
    pgList,
    pg,
    roles,
    caps,
    actAsKey,
    allowed,
    actAs,
    actAsReady,
    actAsReason,
    rolesError,
    /** Sets the Act as role (null = the owner) for this database and branch. */
    setActAs: (role: string | null) => setActAsRole(actAsKey, role),
  };
}
