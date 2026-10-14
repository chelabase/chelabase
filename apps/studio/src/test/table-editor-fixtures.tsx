import { vi } from "vitest";
import userEvent from "@testing-library/user-event";
import type { Table } from "@chelabase/management-client";
import { TableEditor } from "@/components/table-editor/table-editor";
import type { EditorParams } from "@/lib/table-editor/context";
import { invoices } from "@/lib/table-editor/test-tables";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "./fake-fetch";
import { renderWithQuery } from "./render";

export { invoices };
export const D = "/v1/databases/d1";
export const B = `${D}/branches/b1`;
export const T = `${B}/tables/public/invoices`;
export const FULL: EditorParams = { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public", table: "invoices" };

export type Level = "viewer" | "developer" | "admin";
export function meAs(level: Level, roles: string[] = []) {
  return {
    sub: "u2", email: "bo@example.test", name: "Bo", tenant_id: "t1",
    member: { id: "m-self", status: "active", tenant_admin: false },
    instance_admin: false, tenant_admins: [],
    // R17: a project Admin's grant is every role ("all") unless the test names roles.
    projects: ["p1", "p2"].map((project_id) => ({ project_id, platform_role: level, roles: level === "admin" && roles.length === 0 ? ("all" as const) : roles, databases: [] as unknown[] })),
    user: { sub: "u2", org_id: "org-1" },
    tenant: { id: "t1", slug: "demo", display_name: null, projects: PROJECTS },
  };
}
const dbSummary = (database_id: string, name: string) => ({ database_id, name, status: "ready", branch_count: 2, host: null, port: null, postgres_database: "app" });
export const PROJECTS = [
  { id: "p1", name: "Acme shop", slug: "acme-shop", status: "active", databases: [dbSummary("d1", "orders-db"), dbSummary("d2", "analytics")] },
  { id: "p2", name: "Demo", slug: "demo", status: "active", databases: [dbSummary("d9", "demo-db")] },
];
export const branch = (id: string, name: string, parent: string | null, over: Record<string, unknown> = {}) => ({
  id, name, parent_branch_id: parent, kind: "normal", status: "ready", protected: false, created_at: "2026-10-01T00:00:00Z",
  branched_at_lsn: null, branched_at_time: null, error: null, guard_logins: false, history_start: null, host: null, port: null,
  main_database_missing: false, review_acknowledged_hash: null, ...over,
});
export const BRANCHES = [
  branch("b1", "main", null, { protected: true }), branch("b2", "staging", "b1"), branch("b3", "feat-discounts", "b2"),
  branch("b4", "backup-1", "b1", { kind: "backup" }), branch("b5", "broken", "b1", { status: "failed" }),
];
export const pgDb = (name: string, main: boolean, data_api = "on") => ({ name, main, data_api, data_api_error: null, default_schema: "public", origin: main ? "main" : "owner", owner: "app_owner" });
export const PG_DATABASES = [pgDb("app", true), pgDb("reporting", false, "off")];
export const role = (name: string, origin = "sql") => ({ name, level: "plain", bypass_rls: false, login: false, origin, password: "none", pending: false, blocked_keys: [], created_at: "2026-10-01T00:00:00Z" });
export const ROLES = [role("app_owner", "owner"), role("billing_admin"), role("reporter")];
export const obj = (schema: string, name: string, over: Record<string, unknown> = {}) => ({
  schema, name, kind: "table", oid: 1, owner: "app_owner", comment: null, rls_enabled: true, rls_forced: false, data_api_exposed: true,
  row_estimate: 100, total_bytes: 8192, editable: true, ...over,
});
export const OBJECTS = [
  obj("public", "invoices", { row_estimate: 48210, total_bytes: 12582912 }), obj("public", "customers", { row_estimate: 3100 }),
  obj("public", "v_open", { kind: "view", editable: false }), obj("billing", "ledger"),
];
export const TYPES = { types: [
  { oid: 20, array_oid: 1016, schema: "pg_catalog", name: "int8", kind: "base", category: "N", preferred: false, takes_modifiers: false },
  { oid: 25, array_oid: 1009, schema: "pg_catalog", name: "text", kind: "base", category: "S", preferred: true, takes_modifiers: false },
  { oid: 16, array_oid: 1000, schema: "pg_catalog", name: "bool", kind: "base", category: "B", preferred: true, takes_modifiers: false },
  { oid: 2950, array_oid: 2951, schema: "pg_catalog", name: "uuid", kind: "base", category: "U", preferred: false, takes_modifiers: false },
  { oid: 1184, array_oid: 1185, schema: "pg_catalog", name: "timestamptz", kind: "base", category: "D", preferred: true, takes_modifiers: true },
  { oid: 16500, array_oid: 16499, schema: "public", name: "invoice_status", kind: "enum", category: "E", preferred: false, takes_modifiers: false, labels: ["open", "paid", "void"] },
] };
export const ROWS = [
  { id: "3f9a-1", customer_id: "c-1", status: "paid", amount_cents: 12900, memo: null },
  { id: "81c4-2", customer_id: "c-2", status: "open", amount_cents: 45000, memo: "Q3 plan" },
  { id: "c2d1-3", customer_id: "c-1", status: "void", amount_cents: 8900, memo: "dup" },
];
/** A dry run echoing one statement per operation; a real run that commits. */
export const schemaChanges: FakeHandler = async (req) => {
  const body = (await req.clone().json()) as { dry_run?: boolean; operations?: unknown[] };
  const statements = (body.operations ?? []).map((_, i) => ({ sql: `SQL ${i}`, op_index: i, executed: !!body.dry_run, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null }));
  return jsonResponse({ dry_run: !!body.dry_run, committed: !body.dry_run, would_succeed: body.dry_run ? true : null, statements });
};

export interface EditorSetup {
  me?: ReturnType<typeof meAs>;
  params?: EditorParams;
  table?: Table;
  rows?: Record<string, unknown>[];
  objects?: unknown[];
  routes?: Record<string, FakeHandler>;
}
export function editorRoutes(o: EditorSetup = {}): Record<string, FakeHandler> {
  return {
    "GET /v1/me": () => jsonResponse(o.me ?? meAs("admin")),
    // The real list carries `ProjectInfo` (no databases); databases come from `/v1/me` (`tenant.projects`).
    "GET /v1/projects": () => jsonResponse(PROJECTS.map(({ databases: _, ...p }) => ({ ...p, app_users: null, error: null, created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z" }))),
    [`GET ${D}`]: () => jsonResponse({ database_id: "d1", name: "orders-db", branch_count: 2, history_window: 604800, max_branches: 5, max_rows: 1000, statement_timeout_ms: 8000, max_connections: 5 }),
    [`GET ${D}/branches`]: () => jsonResponse(BRANCHES),
    [`GET ${B}/databases`]: () => jsonResponse(PG_DATABASES),
    [`GET ${B}/roles`]: () => jsonResponse(ROLES),
    [`GET ${B}/objects`]: () => jsonResponse({ objects: o.objects ?? OBJECTS }),
    [`GET ${B}/types`]: () => jsonResponse(TYPES),
    [`GET ${T}`]: () => jsonResponse(o.table ?? invoices()),
    [`GET ${B}/tables/public/customers`]: () => jsonResponse(invoices({ name: "customers" } as Partial<Table>)),
    [`POST ${B}/console/query`]: () => jsonResponse(o.rows ?? ROWS),
    [`POST ${B}/schema-changes`]: schemaChanges,
    ...o.routes,
  };
}
export function renderEditor(o: EditorSetup = {}) {
  const navigate = vi.fn();
  const f: FakeFetch = fakeFetch(editorRoutes(o));
  const result = renderWithQuery(<TableEditor params={o.params ?? FULL} navigate={navigate} />, { fetch: f });
  return { ...result, f, navigate, user: userEvent.setup() };
}
/** The JSON bodies of the requests to `path` with `method`, in order. */
export async function bodies(f: FakeFetch, method: string, path: string): Promise<any[]> {
  const hits = f.calls.filter((r) => r.method === method && new URL(r.url).pathname === path);
  return Promise.all(hits.map((r) => r.clone().json()));
}
/** jsdom has no layout: give elements a size so the virtualizer renders rows. */
export function stubLayout() {
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({ x: 0, y: 0, top: 0, left: 0, right: 1200, bottom: 800, width: 1200, height: 800, toJSON: () => ({}) } as DOMRect);
  vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockReturnValue(800);
  vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockReturnValue(1200);
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
}
