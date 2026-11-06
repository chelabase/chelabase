import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import userEvent from "@testing-library/user-event";
import type {
  AccessEvent,
  BranchRole,
  MeResponse,
  Member,
  PersonalLoginView,
  ProjectGrant,
} from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { CrumbsRoot } from "@/test/render-crumbs";
import { renderWithQuery } from "@/test/render";
import { P1, P2, person } from "../hub/fixtures";
import { TeamPage } from "../teams";
import { MemberPage } from "./index";

/** Test support for the member page's RED tests (task S2): fixtures, the fake API and the router harness. */

export { P1, P2 };
export const P3 = "33333333-3333-4333-8333-333333333333";
export const D1 = "d0000000-0000-4000-8000-000000000001"; // billing-prod, in Billing app
export const D2 = "d0000000-0000-4000-8000-000000000002"; // warehouse, in Data warehouse
export const D3 = "d0000000-0000-4000-8000-000000000003"; // ops, in Ops console
export const B1 = "b0000000-0000-4000-8000-000000000001";
export const B2 = "b0000000-0000-4000-8000-000000000002";
export const B3 = "b0000000-0000-4000-8000-000000000003";
export const SOFIA = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
export const MARA = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
export const PRI = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
export const DEV = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
export const ANA = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
export const STRANGER = "99999999-9999-4999-8999-999999999999";

const DAY = 86_400_000;
/** `n` days and an hour from now: "in n days" however the page rounds. */
export const inDays = (n: number) => new Date(Date.now() + n * DAY + 3_600_000).toISOString();
export const daysAgo = (n: number) => new Date(Date.now() - n * DAY).toISOString();

export type Caller = "tenant_admin" | "project_admin" | "developer" | "self";

const ready = (database_id: string, name: string) => ({
  database_id, name, status: "ready", host: "h", port: 5432, postgres_database: name, branch_count: 1,
});
const PROJECTS = [
  { id: P1, slug: "billing", name: "Billing app", status: "active", databases: [ready(D1, "billing-prod")] },
  { id: P2, slug: "warehouse", name: "Data warehouse", status: "active", databases: [ready(D2, "warehouse")] },
  { id: P3, slug: "ops", name: "Ops console", status: "active", databases: [ready(D3, "ops")] },
];

export function meFor(caller: Caller): MeResponse {
  const base = { sub: "u", instance_admin: false, tenant_admins: [], tenant_id: "t1", user: { sub: "u" } };
  const tenant = (ids: string[]) => ({ id: "t1", slug: "demo", display_name: null, projects: PROJECTS.filter((p) => ids.includes(p.id)) });
  const mp = (id: string, platform_role: string, roles: string[] | "all") => ({
    project_id: id,
    platform_role,
    roles,
    databases: PROJECTS.find((p) => p.id === id)!.databases.map((d) => ({ database_id: d.database_id, platform_role, roles })),
  });
  switch (caller) {
    case "tenant_admin":
      return {
        ...base, email: "mara@northwind.dev", name: "Mara Okafor", member: { id: MARA, status: "active", tenant_admin: true },
        projects: [mp(P1, "admin", "all"), mp(P2, "admin", "all"), mp(P3, "admin", "all")], tenant: tenant([P1, P2, P3]),
      } as unknown as MeResponse;
    case "project_admin":
      return {
        ...base, email: "pri@northwind.dev", name: "Priya Raman", member: { id: PRI, status: "active", tenant_admin: false },
        projects: [mp(P1, "admin", "all")], tenant: tenant([P1]),
      } as unknown as MeResponse;
    case "developer":
      return {
        ...base, email: "dev@northwind.dev", name: "Dev Dunn", member: { id: DEV, status: "active", tenant_admin: false },
        projects: [mp(P1, "developer", ["billing_reader"])], tenant: tenant([P1]),
      } as unknown as MeResponse;
    case "self":
      return {
        ...base, email: "sofia@northwind.dev", name: "Sofia Marchetti", member: { id: SOFIA, status: "active", tenant_admin: false },
        projects: [mp(P1, "developer", ["billing_editor", "billing_reader"]), mp(P2, "developer", ["reports_reader", "etl_writer"])],
        tenant: tenant([P1, P2]),
      } as unknown as MeResponse;
  }
}

export const SELF_GRANTS: Record<string, ProjectGrant> = {
  [P1]: { member_id: SOFIA, platform_role: "developer", pg_roles: ["billing_editor"], overrides: [] } as ProjectGrant,
  [P2]: { member_id: SOFIA, platform_role: "developer", pg_roles: ["reports_reader"], overrides: [] } as ProjectGrant,
};

function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false, guard_exempt: false,
    ...extra,
  } as BranchRole;
}
const owner = (name: string) => role(name, { origin: "owner", level: "owner_level", login: true });
const ROLES: Record<string, BranchRole[]> = {
  [D1]: [owner("billing_owner"), role("billing_editor"), role("billing_reader")],
  [D2]: [owner("wh_owner"), role("reports_reader"), role("etl_writer"), role("a&b")],
  [D3]: [owner("ops_owner")],
};
const BRANCH_OF: Record<string, string> = { [D1]: B1, [D2]: B2, [D3]: B3 };

/** The member the page is about, as the roster API shows them to the caller (R1: visible projects only). */
export function sofiaFor(caller: Caller, extra: Partial<Member> = {}): Member {
  const projects: [string, "developer" | "viewer"][] =
    caller === "tenant_admin" || caller === "self" ? [[P1, "developer"], [P2, "developer"]] : [[P1, "developer"]];
  return {
    ...person(SOFIA, { name: "Sofia Marchetti", level: "developer", projects }),
    email: "sofia@northwind.dev",
    created_at: "2026-05-09T00:00:00Z",
    ...extra,
  };
}

export function login(extra: Partial<PersonalLoginView> & { role_name: string; database_id: string }): PersonalLoginView {
  const project_id = extra.database_id === D1 ? P1 : extra.database_id === D2 ? P2 : P3;
  const database_name = extra.database_id === D1 ? "billing-prod" : extra.database_id === D2 ? "warehouse" : "ops";
  return {
    member_id: SOFIA, database_name, project_id, valid_until: inDays(71), created_at: "2026-08-03T00:00:00Z", status: "active",
    revoked_at: null, revoked_reason: null, ...extra,
  } as PersonalLoginView;
}

export const LOGIN_SOON = login({ role_name: "sofia", database_id: D1, valid_until: inDays(9) });
export const LOGIN_LATER = login({ role_name: "sofia_m", database_id: D2 });
export const LOGIN_REVOKED = login({
  role_name: "sofia", database_id: D3, status: "revoked", revoked_at: daysAgo(11), revoked_reason: "access_lost", valid_until: inDays(40),
});

export const PASSWORD = "q7Hc2vR9xW4mTzK8pLs3NbE6yUa1";
export const CONNECTION = `postgresql://sofia_m:${PASSWORD}@ep-warehouse-main.db.northwind.example:5432/warehouse?sslmode=require`;
export function secret(l: PersonalLoginView, over: { password?: string; connection_string?: string } = {}) {
  return {
    login: l,
    password: over.password ?? PASSWORD,
    connection_string: over.connection_string ?? `postgresql://${l.role_name}:${over.password ?? PASSWORD}@ep-main.db.northwind.example:5432/${l.database_name}?sslmode=require`,
    ready_on_main: true,
  };
}

export interface Opts {
  caller?: Caller;
  /** The page's member (default Sofia, or the caller's own page when `caller` is "self"). */
  memberId?: string;
  /** The roster; default Sofia (active) plus the caller. Called per request so tests can change it. */
  members?: () => Member[];
  grants?: Record<string, ProjectGrant>;
  logins?: () => PersonalLoginView[];
  events?: AccessEvent[];
  routes?: Record<string, FakeHandler>;
  initial?: string;
  /** Wraps the router in the crumb labels provider with the header trail (default off). */
  crumbs?: boolean;
}

export const ok = (body: unknown, status = 200): FakeHandler => () => jsonResponse(body, status);
export const apiError = (status: number, code: string, message: string): FakeHandler => () => jsonResponse({ error: { code, message } }, status);

/** The member page in a memory router that also knows `/app/members` and `/other`. */
export function mount(opts: Opts = {}) {
  const caller = opts.caller ?? "tenant_admin";
  const memberId = opts.memberId ?? SOFIA;
  const peopleOf = (): Member[] =>
    opts.members?.() ?? [
      sofiaFor(caller),
      person(MARA, { name: "Mara Okafor", level: "tenant_admin", projects: [[P1, "admin"], [P2, "admin"]] }),
      person(ANA, { name: "Ana Ng", level: "tenant_admin", projects: [[P1, "admin"], [P2, "admin"], [P3, "admin"]] }),
      person(PRI, { name: "Priya Raman", level: "admin", projects: [[P1, "admin"]] }),
      person(DEV, { name: "Dev Dunn", level: "developer", projects: [[P1, "developer"]] }),
    ];
  const grants = opts.grants ?? SELF_GRANTS;
  const routes: Record<string, FakeHandler> = {
    "GET /v1/me": ok(meFor(caller)),
    "GET /v1/members": () => jsonResponse(peopleOf()),
    [`GET /v1/members/${memberId}/personal-logins`]: () => jsonResponse({ logins: opts.logins?.() ?? [] }),
    "GET /v1/access-events": ok({ events: opts.events ?? [], next_before: null }),
    ...opts.routes,
  };
  for (const p of PROJECTS) {
    routes[`GET /v1/projects/${p.id}/grants`] ??= () => jsonResponse(grants[p.id] ? [grants[p.id]] : []);
    routes[`GET /v1/projects/${p.id}/access-events`] ??= ok({ events: opts.events ?? [], next_before: null });
    const d = p.databases[0].database_id;
    routes[`GET /v1/databases/${d}/branches`] ??= ok([{ id: BRANCH_OF[d], parent_branch_id: null, name: "main", status: "ready", kind: "main" }]);
    routes[`GET /v1/databases/${d}/branches/${BRANCH_OF[d]}/roles`] ??= ok(ROLES[d]);
  }
  const f = fakeFetch(routes);
  const root = createRootRoute(opts.crumbs ? { component: CrumbsRoot } : undefined);
  const page = createRoute({
    getParentRoute: () => root,
    path: "/app/members/$memberId",
    component: function Page() {
      return <MemberPage memberId={(page.useParams() as unknown as { memberId: string }).memberId} />;
    },
  });
  const list = createRoute({ getParentRoute: () => root, path: "/app/members", component: () => <p>members list route</p> });
  const team = createRoute({ getParentRoute: () => root, path: "/app/members/teams/$teamId", component: function Team() {
      return <TeamPage teamId={(team.useParams() as unknown as { teamId: string }).teamId} />;
    },
  });
  const other = createRoute({ getParentRoute: () => root, path: "/other", component: () => <p>elsewhere</p> });
  const router = createRouter({
    routeTree: root.addChildren([page, list, team, other]),
    history: createMemoryHistory({ initialEntries: [opts.initial ?? `/app/members/${memberId}`] }),
  });
  const view = renderWithQuery(<RouterProvider router={router} />, { fetch: f });
  return { ...view, f, router, user: userEvent.setup() };
}

export const calls = (f: { calls: Request[] }, method: string, pathPrefix: string) =>
  f.calls.filter((r) => r.method === method && new URL(r.url).pathname.startsWith(pathPrefix));
