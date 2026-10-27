import { TABLES_ROUTE } from "@/components/app-shell/rail-items";

/** One breadcrumb. `to` is a route template (values go in `params`); the last crumb has none. */
export interface Crumb {
  key: string;
  label: string;
  to?: string;
  params?: Record<string, string>;
  search?: Record<string, unknown>;
  /** The dynamic label has not arrived: shown as "…" and never a link (P6). */
  loading?: boolean;
}

/** A label together with the id it belongs to, so a stale name never shows for another id (S1). */
export type CrumbLabel = { id: string; label: string };
/** Keyed by dynamic param name: projectId, databaseId, memberId, role, table. */
export type CrumbLabels = Readonly<Record<string, CrumbLabel>>;
/** The router's decoded params of the deepest match. */
export type CrumbParams = Readonly<Record<string, string | undefined>>;

export const LOADING_LABEL = "…";

const TABLE_PARAMS = ["projectId", "databaseId", "branchId", "pgDatabase", "schema"] as const;

function dynamic(key: string, param: string, params: CrumbParams, labels: CrumbLabels, link?: Partial<Crumb>): Crumb {
  const value = params[param];
  const entry = labels[param];
  if (value && entry && entry.id === value && entry.label) return { key, label: entry.label, ...link };
  return { key, label: LOADING_LABEL, loading: true };
}

/** Drops `to`, `params` and `search` from the last crumb: it is the current page. */
function finish(crumbs: Crumb[]): Crumb[] {
  return crumbs.map((c, i) => {
    if (i < crumbs.length - 1) return c;
    const { to: _to, params: _params, search: _search, ...rest } = c;
    return rest;
  });
}

/**
 * Crumbs for the signed-in routes. The pathname picks the shape only; every dynamic value comes from
 * the router's decoded `params` (S2). Unknown paths give no crumbs.
 */
export function buildCrumbs(pathname: string, params: CrumbParams, labels: CrumbLabels): Crumb[] {
  if (pathname === "/app" || pathname === "/app/") return [{ key: "projects", label: "Projects" }];

  if (pathname.startsWith("/app/projects/")) {
    const projects: Crumb = { key: "projects", label: "Projects", to: "/app" };
    const project = dynamic("project", "projectId", params, labels, {
      to: "/app/projects/$projectId",
      params: { projectId: params.projectId ?? "" },
    });
    if (params.mergeId !== undefined) {
      const database = dynamic("database", "databaseId", params, labels);
      return finish([projects, project, database, { key: "merge", label: "Merge" }]);
    }
    return finish([projects, project]);
  }

  if (pathname === "/app/tables" || pathname.startsWith("/app/tables/")) {
    if (!params.table) return [{ key: "tables", label: "Tables" }];
    const tableParams: Record<string, string> = {};
    for (const k of TABLE_PARAMS) {
      const v = params[k];
      if (v) tableParams[k] = v;
    }
    return finish([
      { key: "tables", label: "Tables", to: TABLES_ROUTE, params: tableParams },
      dynamic("table", "table", params, labels),
    ]);
  }

  if (pathname === "/app/api" || pathname.startsWith("/app/api/")) return [{ key: "api", label: "API" }];

  if (pathname === "/app/members" || pathname.startsWith("/app/members/")) {
    const members: Crumb = { key: "members", label: "Members", to: "/app/members" };
    if (params.role !== undefined) {
      const search: Record<string, unknown> = { tab: "roles" };
      if (params.databaseId) search.database = params.databaseId;
      if (params.branchId) search.branch = params.branchId;
      return finish([members, { key: "roles", label: "Roles", to: "/app/members", search }, dynamic("role", "role", params, labels)]);
    }
    if (params.teamId !== undefined) {
      return finish([members, { key: "teams", label: "Teams", to: "/app/members", search: { tab: "teams" } }, dynamic("team", "teamId", params, labels)]);
    }
    if (params.memberId !== undefined) return finish([members, dynamic("member", "memberId", params, labels)]);
    return [{ key: "members", label: "Members" }];
  }

  return [];
}
