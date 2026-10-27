import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import userEvent from "@testing-library/user-event";
import type { BranchRole, MeResponse, Member, Team } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { CrumbsRoot } from "@/test/render-crumbs";
import { P1, P2, person } from "../hub/fixtures";
import { TeamPage } from "./team-page";
import { TeamsTab } from "./teams-tab";

/** Test support for the Teams tab and the team page: fixtures, the fake API (the generated client's real URLs) and the router. */

export { P1, P2 };
export const T1 = "a1000000-0000-4000-8000-000000000001";
export const T2 = "a1000000-0000-4000-8000-000000000002";
export const D1 = "d0000000-0000-4000-8000-000000000001";
export const D2 = "d0000000-0000-4000-8000-000000000002";
const B1 = "b0000000-0000-4000-8000-000000000001";
const B2 = "b0000000-0000-4000-8000-000000000002";
export const MARA = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
export const SOFIA = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
export const DEV = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
export const ANA = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
export const ED = "ffffffff-ffff-4fff-8fff-ffffffffffff";

export type Caller = "tenant_admin" | "developer";

const ready = (database_id: string, name: string) => ({ database_id, name, status: "ready", host: "h", port: 5432, postgres_database: name, branch_count: 1 });
const PROJECTS = [
  { id: P1, slug: "billing", name: "Billing app", status: "active", databases: [ready(D1, "billing-prod")] },
  { id: P2, slug: "warehouse", name: "Data warehouse", status: "active", databases: [ready(D2, "warehouse")] },
];

export function meFor(caller: Caller): MeResponse {
  const base = { sub: "u", instance_admin: false, tenant_admins: [], tenant_id: "t1", user: { sub: "u", org_id: "o" } };
  const mp = (id: string, platform_role: string, roles: string[] | "all") => ({ project_id: id, platform_role, roles, databases: [] });
  return caller === "tenant_admin"
    ? ({ ...base, email: "mara@x.test", name: "Mara", member: { id: MARA, status: "active", tenant_admin: true }, projects: [mp(P1, "admin", "all"), mp(P2, "admin", "all")], tenant: { id: "t1", slug: "demo", display_name: null, projects: PROJECTS } } as unknown as MeResponse)
    : ({ ...base, email: "dev@x.test", name: "Dev", member: { id: DEV, status: "active", tenant_admin: false }, projects: [mp(P1, "developer", ["billing_reader"])], tenant: { id: "t1", slug: "demo", display_name: null, projects: [PROJECTS[0]] } } as unknown as MeResponse);
}

export const MEMBERS: Member[] = [
  person(MARA, { name: "Mara Okafor", level: "tenant_admin" }),
  person(SOFIA, { name: "Sofia Marchetti", level: "developer", projects: [[P1, "developer"]] }),
  person(DEV, { name: "Dev Dunn", level: "developer", projects: [[P1, "developer"]] }),
  person(ANA, { name: "Ana Ng" }),
  person(ED, { name: "Ed Pending", status: "pending" }),
];

export function team(id: string, name: string, extra: Partial<Team> = {}): Team {
  return {
    id, name, description: "", member_count: 0, projects: [], created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-02T00:00:00Z", ...extra,
  } as Team;
}

/** Billing: Sofia and Dev, Developer on Billing app with billing_reader, Viewer on Data warehouse. */
export const BILLING = team(T1, "Billing", {
  description: "Finance engineers",
  member_count: 2,
  member_ids: [SOFIA, DEV],
  projects: [
    { project_id: P1, platform_role: "developer", pg_roles: ["billing_reader"] },
    { project_id: P2, platform_role: "viewer", pg_roles: [] },
  ],
});
export const SUPPORT = team(T2, "Support", { member_count: 0, member_ids: [], projects: [] });

export function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "api", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false, guard_exempt: false, valid_until: null, ...extra,
  } as BranchRole;
}
export const ROLES = [role("billing_reader"), role("billing_editor")];

export const ok = (body: unknown, status = 200): FakeHandler => () => jsonResponse(body, status);
export const apiError = (status: number, code: string, message: string): FakeHandler => () => jsonResponse({ error: { code, message } }, status);

export interface Opts {
  caller?: Caller;
  teams?: Team[];
  members?: Member[];
  routes?: Record<string, FakeHandler>;
  initial?: string;
  /** Wraps the router in the crumb labels provider with the header trail (default off). */
  crumbs?: boolean;
}

function routesFor(opts: Opts): Record<string, FakeHandler> {
  const teams = opts.teams ?? [BILLING, SUPPORT];
  const routes: Record<string, FakeHandler> = {
    "GET /v1/me": ok(meFor(opts.caller ?? "tenant_admin")),
    "GET /v1/members": ok(opts.members ?? MEMBERS),
    "GET /v1/teams": ok(teams),
    "GET /v1/access-events": ok({ events: [], next_before: null }),
    ...opts.routes,
  };
  for (const t of teams) routes[`GET /v1/teams/${t.id}`] ??= ok(t);
  for (const [d, b] of [[D1, B1], [D2, B2]]) {
    routes[`GET /v1/databases/${d}/branches`] ??= ok([{ id: b, parent_branch_id: null, name: "main", status: "ready", kind: "main" }]);
    routes[`GET /v1/databases/${d}/branches/${b}/roles`] ??= ok(ROLES);
  }
  return routes;
}

/** The team page in a memory router that also knows the members page (the back link's target). */
export function mountPage(teamId: string, opts: Opts = {}) {
  const f = fakeFetch(routesFor(opts));
  const root = createRootRoute(opts.crumbs ? { component: CrumbsRoot } : undefined);
  const page = createRoute({
    getParentRoute: () => root,
    path: "/app/members/teams/$teamId",
    component: function Page() {
      return <TeamPage teamId={(page.useParams() as unknown as { teamId: string }).teamId} />;
    },
  });
  const list = createRoute({ getParentRoute: () => root, path: "/app/members", component: () => <p>members list route</p> });
  const router = createRouter({
    routeTree: root.addChildren([page, list]),
    history: createMemoryHistory({ initialEntries: [opts.initial ?? `/app/members/teams/${teamId}`] }),
  });
  const view = renderWithQuery(<RouterProvider router={router} />, { fetch: f });
  return { ...view, f, router, user: userEvent.setup() };
}

/** The Teams tab, bare (no router: links are plain anchors) or in a router. */
export function mountTab(opts: Opts = {}) {
  const f = fakeFetch(routesFor(opts));
  const me = meFor(opts.caller ?? "tenant_admin");
  const view = renderWithQuery(<TeamsTab tenantAdmin={me.member?.tenant_admin === true} projects={me.tenant?.projects ?? []} />, { fetch: f });
  return { ...view, f, user: userEvent.setup() };
}

export const calls = (f: { calls: Request[] }, method: string, path: string) =>
  f.calls.filter((r) => r.method === method && new URL(r.url).pathname === path);
