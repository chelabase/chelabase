import { type AnyRoute, RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import userEvent from "@testing-library/user-event";
import type {
  Branch,
  BranchDatabaseView,
  BranchRole,
  Grant,
  MeResponse,
  Member,
  ObjectPrivileges,
  PersonalLoginView,
  PrivilegesPage,
  ProjectGrant,
} from "@chelabase/management-client";
import { Route as MembersRoute } from "@/routes/app.members";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { P1, P2, person } from "../hub/fixtures";
import { RolePage } from "./role-page";
import { RolesTab } from "./roles-tab";

/** Test support for the Roles tab and role page RED tests (task S3): fixtures, the fake API and the router harness. */

export { P1, P2 };
export const D1 = "d0000000-0000-4000-8000-000000000001"; // billing-prod, in Billing app
export const D1B = "d0000000-0000-4000-8000-0000000000b1"; // billing-staging, in Billing app
export const D2 = "d0000000-0000-4000-8000-000000000002"; // warehouse, in Data warehouse
export const B1 = "b0000000-0000-4000-8000-000000000001"; // D1 main (protected)
export const B1DEV = "b0000000-0000-4000-8000-0000000000d1"; // D1 dev (child, not protected)
export const B1B = "b0000000-0000-4000-8000-0000000000b1"; // D1B main
export const B2 = "b0000000-0000-4000-8000-000000000002"; // D2 main
export const SOFIA = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
export const MARA = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
export const PRI = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
export const DEV = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
export const ANA = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";

export const PG = "billing";
export const ROLE = "billing_editor";
/** A role name with a right-to-left override in it (MH-38d): shown escaped, never raw. */
export const BIDI = "refunds‮review";
export const BIDI_ESCAPED = "refunds\\u202ereview";

export type Caller = "tenant_admin" | "project_admin" | "developer" | "viewer";

// ---- who is asking ---------------------------------------------------------------------------

const ready = (database_id: string, name: string) => ({
  database_id, name, status: "ready", host: "h", port: 5432, postgres_database: name, branch_count: 1,
});
const PROJECTS = [
  { id: P1, slug: "billing", name: "Billing app", status: "active", databases: [ready(D1, "billing-prod"), ready(D1B, "billing-staging")] },
  { id: P2, slug: "warehouse", name: "Data warehouse", status: "active", databases: [ready(D2, "warehouse")] },
];

export function meFor(caller: Caller): MeResponse {
  const base = { sub: "u", instance_admin: false, tenant_admins: [], tenant_id: "t1", user: { sub: "u", org_id: "o" } };
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
        projects: [mp(P1, "admin", "all"), mp(P2, "admin", "all")], tenant: tenant([P1, P2]),
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
    case "viewer":
      return {
        ...base, email: "vera@northwind.dev", name: "Vera", member: { id: DEV, status: "active", tenant_admin: false },
        projects: [mp(P1, "viewer", [])], tenant: tenant([P1]),
      } as unknown as MeResponse;
  }
}

// ---- the roster, grants and personal logins ---------------------------------------------------

export const MEMBERS: Member[] = [
  person(MARA, { name: "Mara Okafor", level: "tenant_admin", projects: [[P1, "admin"], [P2, "admin"]] }),
  person(PRI, { name: "Priya Raman", level: "admin", projects: [[P1, "admin"]] }),
  person(SOFIA, { name: "Sofia Marchetti", level: "developer", projects: [[P1, "developer"]] }),
  person(DEV, { name: "Dev Dunn", level: "developer", projects: [[P1, "developer"]] }),
  person(ANA, { name: "Ana Ng", level: "developer", projects: [[P1, "developer"]] }),
];

const pg = (member_id: string, pg_roles: string[], platform_role = "developer"): ProjectGrant =>
  ({ member_id, platform_role, pg_roles, overrides: [] }) as unknown as ProjectGrant;
/**
 * Holders: Sofia has billing_editor and billing_reader; Dev has billing_reader; Ana has billing_admin,
 * which is a member of billing_editor (so Ana holds billing_editor through the chain).
 * Counts: billing_editor 2, billing_reader 2, billing_admin 1, everything else none.
 */
export const GRANTS_P1: ProjectGrant[] = [pg(SOFIA, ["billing_editor", "billing_reader"]), pg(DEV, ["billing_reader"]), pg(ANA, ["billing_admin"])];

const login = (member_id: string, role_name: string, extra: Partial<PersonalLoginView> = {}): PersonalLoginView =>
  ({
    member_id, database_id: D1, database_name: "billing-prod", project_id: P1, role_name, valid_until: new Date(Date.now() + 9 * 86_400_000 + 3_600_000).toISOString(),
    created_at: "2026-08-03T00:00:00Z", status: "active", revoked_at: null, revoked_reason: null, ...extra,
  }) as PersonalLoginView;
/** Three active (Sofia, Priya, Ana) and one revoked (Dev). */
export const LOGINS_D1: PersonalLoginView[] = [
  login(SOFIA, "sofia"),
  login(PRI, "priya", { valid_until: new Date(Date.now() + 64 * 86_400_000 + 3_600_000).toISOString() }),
  login(ANA, "ana", { valid_until: new Date(Date.now() + 88 * 86_400_000 + 3_600_000).toISOString() }),
  login(DEV, "devd", { status: "revoked", revoked_at: "2026-10-01T00:00:00Z", revoked_reason: "access_lost" }),
];

// ---- roles --------------------------------------------------------------------------------------

export function role(name: string, extra: Partial<BranchRole> = {}): BranchRole {
  return {
    name, origin: "api", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
    blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false, guard_exempt: false,
    valid_until: null, ...extra,
  } as BranchRole;
}
/** Visible on the Roles tab: billing_editor, billing_reader, support_agent, webhook_worker, billing_admin, migrator, BIDI. */
export const ROLES_D1: BranchRole[] = [
  role("chelabase_owner", { origin: "owner", level: "owner_level", login: true }),
  role("billing_editor"),
  role("billing_reader"),
  role("support_agent"),
  role("webhook_worker"),
  role("billing_admin", { level: "owner_level", member_of: ["billing_editor"] }),
  role("migrator", { login: true, password: "generated" }),
  role(BIDI),
  // Never on the list (owner above; reserved; personal logins):
  role("authenticator", { origin: "sql", login: true }),
  role("chelabase_schema_locked", { origin: "sql" }),
  role("sofia", { origin: "personal", login: true }),
  role("priya", { origin: "personal", login: true }),
  role("ana", { origin: "personal", login: true }),
];
export const VISIBLE_ROLES = ["billing_editor", "billing_reader", "support_agent", "webhook_worker", "billing_admin", "migrator"];
export const HIDDEN_ROLES = ["chelabase_owner", "authenticator", "chelabase_schema_locked", "sofia", "priya", "ana"];

// ---- the privileges read ------------------------------------------------------------------------

const GRANTEE = { role: { name: ROLE, oid: 100 } };
const named = (name: string) => ({ name, oid: name.length + 1000 });
/** One privilege of `ROLE`, as `aclexplode` gives it (upper case). */
export const gr = (privilege: string, grantor = "billing_owner", grantable = false): Grant =>
  ({ grantor: named(grantor), grantee: GRANTEE, privilege, grantable }) as unknown as Grant;

const COLS = (table: string, grants: Record<string, Grant[]> = {}) =>
  ({
    invoices: ["id", "amount", "note"],
    invoice_lines: ["id", "invoice_id"],
    customers: ["id", "email", "display_name", "tax_id"],
    payments: ["id"],
    plans: ["id"],
    refunds: ["id"],
    open_balances: ["id"],
  })[table as "invoices"].map((name) => ({ name, grants: grants[name] ?? [] }));

function obj(kind: ObjectPrivileges["kind"], name: string, extra: Partial<ObjectPrivileges> = {}): ObjectPrivileges {
  return {
    kind, schema: PG, name, oid: 20000 + name.length, identity: null, owner: "billing_owner", owner_settable: true, rls_enabled: false,
    grants: [], columns: [], all_rows: null, acl_hash: `h-${name}`, ...extra,
  };
}

/** The `billing` schema's objects as the read returns them for `billing_editor`. */
export function billingObjects(): ObjectPrivileges[] {
  return [
    obj("table", "invoices", { grants: [gr("SELECT", "billing_owner", true), gr("INSERT")], columns: COLS("invoices") }),
    obj("table", "invoice_lines", { columns: COLS("invoice_lines") }),
    obj("table", "customers", {
      grants: [gr("SELECT")],
      columns: COLS("customers", { display_name: [gr("UPDATE")] }),
      rls_enabled: true,
      all_rows: { policy: "billing_editor_all_rows" },
    }),
    obj("table", "payments", { owner: "ledger_svc", owner_settable: false, grants: [gr("SELECT", "ledger_svc")], columns: COLS("payments") }),
    obj("table", "plans", { grants: [gr("SELECT", "neon_superuser")], columns: COLS("plans") }),
    obj("table", "refunds", { rls_enabled: true, columns: COLS("refunds") }),
    obj("view", "open_balances", { columns: COLS("open_balances") }),
    obj("sequence", "invoices_id_seq", { grants: [gr("USAGE")] }),
    obj("function", "issue_refund", { oid: 5001, identity: "billing.issue_refund(uuid, numeric)", grants: [gr("EXECUTE")] }),
    obj("function", "recalc_totals", { oid: 5002, identity: "billing.recalc_totals(uuid)" }),
    obj("function", "close_period", { oid: 5003, identity: "billing.close_period(date)", owner: "ledger_svc", owner_settable: false }),
  ];
}

export interface PrivData {
  server_version_num: number;
  offered: PrivilegesPage["offered"];
  schemas: PrivilegesPage["schemas"];
  default_privileges: PrivilegesPage["default_privileges"];
  objects: Record<string, ObjectPrivileges[]>;
}

export function privData(): PrivData {
  return {
    server_version_num: 170011,
    offered: {
      table: ["select", "insert", "update", "delete", "truncate", "references", "trigger"],
      sequence: ["usage", "select", "update"],
      schema: ["usage", "create"],
      function: ["execute"],
    },
    schemas: [
      { name: PG, owner: "billing_owner", owner_settable: true, grants: [gr("USAGE", "postgres")], acl_hash: "h-schema-billing" },
      { name: "public", owner: "postgres", owner_settable: true, grants: [], acl_hash: "h-schema-public" },
    ],
    default_privileges: [
      { for_role: "postgres", schema: PG, object: "tables", grants: [gr("SELECT", "postgres"), gr("INSERT", "postgres")], acl_hash: "h-def-tables" },
    ] as PrivilegesPage["default_privileges"],
    objects: {
      [PG]: billingObjects(),
      public: [obj("table", "notes", { schema: "public", owner: "postgres", columns: [{ name: "id", grants: [] }] })],
    },
  };
}

function privPage(data: PrivData, url: URL): PrivilegesPage {
  const schema = url.searchParams.get("schema");
  const after = url.searchParams.get("after");
  const page: PrivilegesPage = {
    role: url.searchParams.get("role") ?? ROLE,
    server_version_num: data.server_version_num,
    offered: data.offered,
    schemas: data.schemas,
    default_privileges: data.default_privileges,
    objects: [],
    next_after: null,
  };
  if (schema === null) return page;
  const all = data.objects[schema] ?? [];
  // A schema bigger than `limit` pages: `limit` is taken from the query; "after" is the last name seen.
  const limit = Number(url.searchParams.get("limit") ?? "200");
  const start = after === null ? 0 : all.findIndex((o) => o.name === after) + 1;
  const slice = all.slice(start, start + limit);
  const more = start + limit < all.length;
  return { ...page, objects: slice, next_after: more ? slice[slice.length - 1].name : null };
}

// ---- the fake API ---------------------------------------------------------------------------------

export const ok = (body: unknown, status = 200): FakeHandler => () => jsonResponse(body, status);
export const apiError = (status: number, code: string, message: string, extra: Record<string, unknown> = {}): FakeHandler => () =>
  jsonResponse({ error: { code, message, ...extra } }, status);

const BRANCHES: Record<string, Branch[]> = {
  [D1]: [
    { id: B1, name: "main", kind: "main", status: "ready", protected: true, parent_branch_id: null },
    { id: B1DEV, name: "dev", kind: "child", status: "ready", protected: false, parent_branch_id: B1 },
  ] as unknown as Branch[],
  [D1B]: [{ id: B1B, name: "main", kind: "main", status: "ready", protected: false, parent_branch_id: null }] as unknown as Branch[],
  [D2]: [{ id: B2, name: "main", kind: "main", status: "ready", protected: false, parent_branch_id: null }] as unknown as Branch[],
};

/** `GET …/databases`: the Postgres databases of a branch; `billing` is the main one. */
export const PG_DATABASES: BranchDatabaseView[] = [
  { name: PG, main: true, owner: "postgres", default_schema: "public", origin: "owner", data_api: "off", data_api_error: null },
  { name: "analytics", main: false, owner: "postgres", default_schema: "public", origin: "api", data_api: "off", data_api_error: null },
] as unknown as BranchDatabaseView[];

export interface Opts {
  caller?: Caller;
  roles?: BranchRole[];
  members?: Member[];
  grants?: ProjectGrant[];
  logins?: PersonalLoginView[];
  data?: PrivData;
  /** Overrides, keyed like `fakeFetch`'s routes. */
  routes?: Record<string, FakeHandler>;
  initial?: string;
}

export const PRIV = `/v1/databases/${D1}/branches/${B1}/pg-databases/${PG}/privileges`;
export const CHANGES = `/v1/databases/${D1}/branches/${B1}/schema-changes`;
export const ROLES_URL = `/v1/databases/${D1}/branches/${B1}/roles`;

function routesFor(opts: Opts, data: PrivData): Record<string, FakeHandler> {
  const caller = opts.caller ?? "tenant_admin";
  const routes: Record<string, FakeHandler> = {
    "GET /v1/me": ok(meFor(caller)),
    "GET /v1/members": () => jsonResponse(opts.members ?? MEMBERS),
    "GET /v1/projects": ok(PROJECTS),
    [`GET /v1/projects/${P1}/grants`]: () => jsonResponse(opts.grants ?? GRANTS_P1),
    [`GET /v1/projects/${P2}/grants`]: ok([]),
    [`GET /v1/databases/${D1}/personal-logins`]: () => jsonResponse({ logins: opts.logins ?? LOGINS_D1 }),
    [`GET /v1/databases/${D1B}/personal-logins`]: ok({ logins: [] }),
    [`GET /v1/databases/${D2}/personal-logins`]: ok({ logins: [] }),
    [`GET ${ROLES_URL}`]: () => jsonResponse(opts.roles ?? ROLES_D1),
    [`GET /v1/databases/${D1}/branches/${B1DEV}/roles`]: () => jsonResponse([role("chelabase_owner", { origin: "owner", level: "owner_level" }), role("dev_only")]),
    [`GET /v1/databases/${D1B}/branches/${B1B}/roles`]: () => jsonResponse([role("chelabase_owner", { origin: "owner", level: "owner_level" })]),
    [`GET /v1/databases/${D2}/branches/${B2}/roles`]: () => jsonResponse([role("chelabase_owner", { origin: "owner", level: "owner_level" })]),
    [`GET ${PRIV}`]: (req) => jsonResponse(privPage(data, new URL(req.url))),
    [`POST ${CHANGES}`]: async (req) => {
      const body = await req.clone().json();
      if (body.dry_run) {
        return jsonResponse({
          committed: false,
          dry_run: true,
          would_succeed: true,
          data_api_exposure: [],
          statements: (body.operations ?? []).map((_: unknown, i: number) => ({ op_index: i, sql: `-- dry run, operation ${i}`, executed: false, lock: "none" })),
        });
      }
      return jsonResponse({ committed: true, dry_run: false, data_api_exposure: [], statements: [] });
    },
    ...opts.routes,
  };
  for (const [d, bs] of Object.entries(BRANCHES)) {
    routes[`GET /v1/databases/${d}/branches`] ??= ok(bs);
    for (const b of bs) routes[`GET /v1/databases/${d}/branches/${b.id}/databases`] ??= ok(PG_DATABASES);
  }
  return routes;
}

function harness(routes: Record<string, FakeHandler>, root: AnyRoute, children: AnyRoute[], initial: string) {
  const f = fakeFetch(routes);
  const router = createRouter({ routeTree: root.addChildren(children), history: createMemoryHistory({ initialEntries: [initial] }) });
  const view = renderWithQuery(<RouterProvider router={router} />, { fetch: f });
  return { ...view, f, router, user: userEvent.setup() };
}

/** Routes the harnesses share: where links and redirects land. */
function landing(root: AnyRoute) {
  const memberPage = createRoute({ getParentRoute: () => root, path: "/app/members/$memberId", component: () => <p>member page</p> });
  const rolePage = (component: () => React.ReactNode) =>
    createRoute({
      getParentRoute: () => root,
      path: "/app/members/roles/$databaseId/$branchId/$role",
      validateSearch: (s: Record<string, unknown>): { pg_database?: string } => (typeof s.pg_database === "string" ? { pg_database: s.pg_database } : {}),
      component,
    });
  return { memberPage, rolePage };
}

/** The Roles tab at `/app/members?tab=roles…`, in a memory router that also knows the role page and member pages. */
export function mountTab(opts: Opts = {}, initial = "/app/members?tab=roles") {
  const root = createRootRoute();
  const { memberPage, rolePage } = landing(root);
  const members = createRoute({
    getParentRoute: () => root,
    path: "/app/members",
    validateSearch: (s: Record<string, unknown>) => (MembersRoute.options.validateSearch as (x: Record<string, unknown>) => Record<string, unknown>)(s),
    component: () => <RolesTab />,
  });
  const landed = rolePage(function Landed() {
    const p = landed.useParams() as unknown as { databaseId: string; branchId: string; role: string };
    return <p>role page route: {p.role} on {p.databaseId}/{p.branchId}</p>;
  });
  return harness(routesFor(opts, opts.data ?? privData()), root, [members, memberPage, landed], opts.initial ?? initial);
}

/** The role page for `role` (default billing_editor) on D1/B1, `?pg_database=billing`. */
export function mountPage(opts: Opts & { role?: string; pgDatabase?: string | null } = {}) {
  const data = opts.data ?? privData();
  const name = opts.role ?? ROLE;
  const root = createRootRoute();
  const { memberPage, rolePage } = landing(root);
  const page = rolePage(function Page() {
    const p = page.useParams() as unknown as { databaseId: string; branchId: string; role: string };
    const s = page.useSearch() as { pg_database?: string };
    return <RolePage databaseId={p.databaseId} branchId={p.branchId} role={p.role} pgDatabase={s.pg_database} />;
  });
  const members = createRoute({
    getParentRoute: () => root,
    path: "/app/members",
    validateSearch: (s: Record<string, unknown>) => (MembersRoute.options.validateSearch as (x: Record<string, unknown>) => Record<string, unknown>)(s),
    component: () => <p>members hub route</p>,
  });
  const search = opts.pgDatabase === null ? "" : `?pg_database=${opts.pgDatabase ?? PG}`;
  const initial = opts.initial ?? `/app/members/roles/${D1}/${B1}/${encodeURIComponent(name)}${search}`;
  return { ...harness(routesFor(opts, data), root, [page, members, memberPage], initial), data };
}

// ---- reading what the page sent ---------------------------------------------------------------------

export const calls = (f: FakeFetch, method: string, path: string) =>
  f.calls.filter((r) => r.method === method && new URL(r.url).pathname === path);
export const bodies = (f: FakeFetch, method: string, path: string) => Promise.all(calls(f, method, path).map((r) => r.clone().json()));
export const query = (r: Request) => new URL(r.url).searchParams;
export const lastQuery = (f: FakeFetch, method: string, path: string) => {
  const all = calls(f, method, path);
  return all.length ? query(all[all.length - 1]) : null;
};

export interface ChangeBody {
  dry_run?: boolean;
  database?: string | null;
  role?: string | null;
  operations: Record<string, Record<string, unknown>>[];
  expect_acl: { object: Record<string, unknown>; acl_hash: string }[];
}
export const changeBodies = (f: FakeFetch) => bodies(f, "POST", CHANGES) as Promise<ChangeBody[]>;

/**
 * Operations as flat facts, so a test doesn't depend on how the page groups privileges into ops:
 * "grant select billing.customers", "grant update(display_name) billing.customers",
 * "revoke select billing.invoices", "revoke-option select billing.invoices",
 * "grant-schema usage billing", "grant-sequence usage billing.invoices_id_seq", "grant-function 5002",
 * "default grant select tables billing for postgres", "all-rows on billing.refunds".
 */
export function facts(ops: ChangeBody["operations"]): string[] {
  const out: string[] = [];
  for (const op of ops) {
    const [kind, v] = Object.entries(op)[0] as [string, Record<string, any>];
    const t = v.table ? `${v.table.schema}.${v.table.name}` : "";
    switch (kind) {
      case "grant":
      case "revoke":
        for (const p of v.grant.privileges) {
          const tag = kind === "revoke" && v.grant.with_grant_option ? "revoke-option" : kind;
          if (kind === "grant" && v.grant.with_grant_option) out.push(`WITH-OPTION ${p} ${t}`);
          for (const c of v.grant.columns?.length ? v.grant.columns : [null]) out.push(`${tag} ${p}${c ? `(${c})` : ""} ${t}`);
        }
        break;
      case "grant_schema":
      case "revoke_schema":
        for (const p of v.privileges) out.push(`${kind.replace("_schema", "")}-schema ${p} ${v.schema}${v.grant_option_only ? " option" : ""}`);
        break;
      case "grant_sequence":
      case "revoke_sequence":
        for (const p of v.privileges) out.push(`${kind.replace("_sequence", "")}-sequence ${p} ${v.sequence.schema}.${v.sequence.name}${v.grant_option_only ? " option" : ""}`);
        break;
      case "grant_function":
      case "revoke_function":
        out.push(`${kind.replace("_function", "")}-function ${v.function.oid}${v.grant_option_only ? " option" : ""}`);
        break;
      case "alter_default_privileges":
        for (const p of v.privileges) out.push(`default ${v.grant ? "grant" : "revoke"} ${p} ${v.object} ${v.schema} for ${v.for_role}`);
        break;
      case "set_all_rows":
        out.push(`all-rows ${v.enabled ? "on" : "off"} ${t}`);
        break;
      default:
        out.push(`UNEXPECTED ${kind}`);
    }
  }
  return out.sort();
}

/** A privilege op's grantee must be the page's role, as serde writes `Grantee::Role`. */
export function grantees(ops: ChangeBody["operations"]): unknown[] {
  return ops.map((op) => {
    const v = Object.values(op)[0] as Record<string, unknown>;
    return v.grantee ?? (v.grant as Record<string, unknown> | undefined)?.grantee ?? v.role;
  });
}
