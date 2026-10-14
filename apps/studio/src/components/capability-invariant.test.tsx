/**
 * The capability invariant: every route-backed control in Studio is shown if and only if the
 * caller's capabilities pass that route's `Access`.
 *
 * Each row maps one control to the operationId it calls. The route's access is read from the
 * generated access table (`apps/docs/.../reference/access.mdx`, checked against the route table
 * by `chelabase-routes`'s `access_table_is_current`), through the OpenAPI document's
 * operationId → method and path. Each control is rendered for a Viewer, a Developer, a project
 * Admin and a tenant admin, and its visibility must match what that access allows.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { screen, waitFor } from "@testing-library/react";
import { useState, type ReactElement } from "react";
import { describe, expect, it } from "vitest";
import type { BranchDatabaseView, BranchRole, DataKey, MeResponse as Me, Member, PlatformRole, ProjectInfo } from "@chelabase/management-client";
import { ProjectCapsContext, projectCaps } from "@/lib/capabilities";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { BranchActions } from "./branch-actions";
import { BranchRoles } from "./branch-roles";
import { Dashboard } from "./dashboard";
import { DataApiDatabases } from "./data-api-databases";
import { DataApiKeys } from "./data-api-keys";
import { DatabaseBranches } from "./database-branches";
import { DatabaseSettings } from "./database-settings";
import { MembersPage, type MembersTab } from "./members/members-page";
import { AccessTab } from "./project-access/access-tab";
import { ProjectPage } from "./project-page";
import { ProjectSettings } from "./project-settings";
import { MergePage } from "./merges/merge-page";
import { branchOf, detailOf, devBranch, mergeOf } from "@/test/merge-fixtures";

// --- The route table, as generated for the docs, keyed by operationId. ---------------------

const root = (rel: string) => fileURLToPath(new URL(rel, import.meta.url));
const accessTable = readFileSync(root("../../../docs/content/docs/reference/access.mdx"), "utf8");
const openapi = JSON.parse(readFileSync(root("../../../../openapi/management.json"), "utf8")) as {
  paths: Record<string, Record<string, { operationId?: string }>>;
};

const accessByRoute = new Map<string, string>();
for (const line of accessTable.split("\n")) {
  const m = /^\| (GET|POST|PUT|PATCH|DELETE) \| `([^`]+)` \| [^|]+ \| ([^|]+) \|$/.exec(line.trim());
  if (m) accessByRoute.set(`${m[1]} ${m[2].replace(/\\/g, "")}`, m[3].trim());
}
function accessOf(operationId: string): string {
  for (const [path, ops] of Object.entries(openapi.paths)) {
    for (const [method, op] of Object.entries(ops)) {
      if (op.operationId === operationId) {
        const access = accessByRoute.get(`${method.toUpperCase()} ${path}`);
        if (!access) throw new Error(`no access row for ${method} ${path}`);
        return access;
      }
    }
  }
  throw new Error(`unknown operationId ${operationId}`);
}

// --- The callers. ----------------------------------------------------------------------------

// I3: the last two are narrowed by an override to Viewer on database d1 (`/v1/me` lists it in
// `projects[].databases`): what they may do on that database is a Viewer's, whatever their grant.
type Persona =
  | "viewer"
  | "developer"
  | "admin"
  | "tenant admin"
  | "developer narrowed to viewer on web1"
  | "admin narrowed to viewer on web1";
const PERSONAS: Persona[] = [
  "viewer",
  "developer",
  "admin",
  "tenant admin",
  "developer narrowed to viewer on web1",
  "admin narrowed to viewer on web1",
];
const narrowedPersona = (p: Persona) => p.includes("narrowed");
const grantRole = (p: Persona): PlatformRole => (p === "tenant admin" ? "admin" : p.startsWith("developer") ? "developer" : p.startsWith("admin") ? "admin" : (p as PlatformRole));

/** Controls that belong to the project as a whole (its settings, the people, its access); the rest are on a database. */
const PROJECT_SCOPED = new Set([
  "Create database", "Rename project", "Delete project", "New project", "Invite people", "A member's actions",
  "Approve a sign-up", "Decline a sign-up", "Rename the tenant", "Activity", "Edit a grant", "Take a grant away",
  "Narrow a grant on a database", "Stop narrowing a grant", "Add people to a project", "A project's access log",
  "Instance settings",
]);
const scopeOf = (c: { name: string }): "project" | "database" => (PROJECT_SCOPED.has(c.name) ? "project" : "database");

const database = { database_id: "d1", name: "web1", postgres_database: "web1", status: "ready", host: "h", port: 1, branch_count: 1 } as const;
function meFor(p: Persona): Me {
  const tenantAdmin = p === "tenant admin";
  const role = grantRole(p);
  const databases = narrowedPersona(p) ? [{ database_id: "d1", platform_role: "viewer" as const, roles: ["reporting"] }] : [];
  return {
    sub: "u", email: "u@example.test", name: "U", tenant_id: "t1", instance_admin: tenantAdmin, tenant_admins: [],
    member: { id: "u", status: "active", tenant_admin: tenantAdmin },
    projects: [{ project_id: "p1", platform_role: role, roles: tenantAdmin ? "all" : ["reporting"], databases }],
    user: { sub: "u", org_id: "o" },
    tenant: { id: "t1", slug: "demo", display_name: null, projects: [{ id: "p1", slug: "default", name: "Default", status: "active", databases: [database] }] },
  };
}

/** What a route's access allows the caller (the Studio side of `decide`). */
function allows(access: string, p: Persona, branchProtected: boolean, scope: "project" | "database"): boolean {
  const me = meFor(p);
  const caps = projectCaps(me, "p1", scope === "database" ? "d1" : undefined);
  let m: RegExpExecArray | null;
  if ((m = /^Project (Viewer|Developer|Admin)$/.exec(access))) return caps.atLeast(m[1].toLowerCase() as PlatformRole);
  if ((m = /^Act as: (Viewer|Developer|Admin) and the role in your set$/.exec(access))) return caps.atLeast(m[1].toLowerCase() as PlatformRole);
  if (access === "Branch: Developer unprotected, Admin protected") return caps.atLeast(branchProtected ? "admin" : "developer");
  if (access === "Tenant admin") return me.member?.tenant_admin === true;
  if (access === "Instance admin") return me.instance_admin;
  if (access === "Tenant member") return true;
  throw new Error(`no rule for access "${access}"`);
}

// --- Fixtures. -------------------------------------------------------------------------------

const ok = (body: unknown): FakeHandler => () => jsonResponse(body);
const branch = (over: Record<string, unknown>) => ({
  parent_branch_id: null, kind: "normal", status: "ready", host: "h", port: 1, branched_at_time: null, branched_at_lsn: null,
  created_at: "2026-09-30T10:00:00Z", history_start: null, error: null, protected: false, main_database_missing: false, ...over,
});
const mainProtected = branch({ id: "m", name: "main", protected: true });
const devOpen = branch({ id: "dv", name: "dev", parent_branch_id: "m" });
const mainOpen = branch({ id: "m", name: "main" });
const roleRow = (name: string, over: Partial<BranchRole> = {}): BranchRole =>
  ({ name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "generated", pending: false, ...over }) as BranchRole;
const dbView = (name: string, main: boolean, data_api: string): BranchDatabaseView =>
  ({ name, main, data_api, data_api_error: null, default_schema: "public", origin: main ? "main" : "sql", owner: "o" }) as BranchDatabaseView;
const dataKey = { id: "k1", name: "k1", kind: "secret", role: "authenticated", databases: null, prefix: "chk_sec_ab12", created_at: "2026-10-01T00:00:00Z", last_used_at: null, role_blocked: false } as unknown as DataKey;
const info: ProjectInfo = { id: "p1", slug: "default", name: "Default", status: "active", error: null, app_users: null, created_at: "2026-10-03T10:00:00Z", updated_at: "2026-10-03T10:00:00Z" };
const settings = { history_window: 604800, max_branches: 5, max_rows: 1000, statement_timeout_ms: 8000, max_connections: 5 };
const member = (id: string, extra: Partial<Member> = {}): Member => ({
  id, email: `${id}@example.test`, display_name: id, status: "active", tenant_admin: false, has_access: true, invite: null,
  accepted_at: null, created_at: "2026-10-01T00:00:00Z", ...extra,
});
const consoleSchema = {
  default_schema: "public", version: "v",
  schemas: { public: { enums: {}, functions: {}, tables: { items: { kind: "table", rls: true, security_invoker: false, primary_key: ["id"], columns: { id: { type: "integer", nullable: false, has_default: true, generated: false, array: false, enum: null } } } } } },
};

const routes: Record<string, FakeHandler> = {
  "GET /v1/projects/p1": ok(info),
  "GET /v1/databases/d1": ok({ ...database, ...settings, tenant_id: "t1", project_id: "p1", error: null, default_schema: "public" }),
  "GET /v1/databases/d1/branches": ok([mainOpen]),
  "GET /v1/databases/d1/merges": ok([]),
  "GET /v1/databases/d1/branches/m/roles": ok([roleRow("o", { origin: "owner", level: "owner_level", login: true }), roleRow("reporting")]),
  "GET /v1/databases/d1/branches/m/databases": ok([dbView("web1", true, "on")]),
  "GET /v1/databases/d1/branches/m/console/schema": ok(consoleSchema),
  "POST /v1/databases/d1/branches/m/console/query": ok({ rows: [], truncated: false }),
  "GET /v1/members": ok([member("u", { tenant_admin: true }), member("bo"), member("ed", { status: "pending" })]),
  "GET /v1/instance/settings": ok({ open_signup: false, open_signup_switchable: true }),
  "GET /v1/invites/delivery": ok({ email_available: true, reason: null }),
  "GET /v1/access-events": ok({ events: [], next_before: null }),
  "GET /v1/projects/p1/grants": ok([
    { member_id: "bo", platform_role: "developer", pg_roles: ["reporting"], overrides: [{ database_id: "d1", platform_role: "viewer", pg_roles: null }] },
  ]),
  "GET /v1/projects/p1/access-events": ok({ events: [], next_before: null }),
};

/** Renders `ui` as `p` sees it: `/v1/me` answers for `p`, and the project's caps are provided. */
function renderAs(p: Persona, ui: ReactElement, extra: Record<string, FakeHandler> = {}, scope: "project" | "database" = "project") {
  const me = meFor(p);
  const f = fakeFetch({ ...routes, ...extra, "GET /v1/me": ok(me) });
  // A surface on a database is given that database's caps, as the project page gives its rows;
  // the project page itself (the rows through `projectPage`) provides its own.
  const caps = projectCaps(me, "p1", scope === "database" ? "d1" : undefined);
  return renderWithRouter(<ProjectCapsContext.Provider value={caps}>{ui}</ProjectCapsContext.Provider>, { fetch: f });
}

function MembersHarness() {
  const [tab, setTab] = useState<MembersTab>("people");
  return <MembersPage tab={tab} onTabChange={setTab} />;
}

interface Control {
  name: string;
  operationId: string;
  /** Overrides the route's access where the control does more than the route's minimum. */
  access?: string;
  protectedBranch?: boolean;
  /** Routes this control's surface needs beyond the shared ones. */
  routes?: Record<string, FakeHandler>;
  ui: () => ReactElement;
  /** Waits until the surface around the control has rendered. */
  ready: () => Promise<unknown>;
  query: () => HTMLElement | null;
}

const button = (name: string | RegExp) => () => screen.queryByRole("button", { name });
const projectPage = () => <ProjectPage projectId="p1" apiUrl="http://gw" tab="databases" onTabChange={() => {}} onGone={() => {}} />;
const projectReady = () => screen.findByRole("cell", { name: "web1" });
const branchActions = (b: ReturnType<typeof branch>, all = [b]) => () => (
  <BranchActions projectId="p1" databaseId="d1" databaseName="web1" branch={b as never} branches={all as never} historyWindow={604800} />
);
const branchReady = () => screen.findByRole("button", { name: "Roles" });
const rolesReady = () => screen.findByText("reporting");
/** A draft merge of dev into main (protected or not). */
const mergeRoutes = (protectedTarget: boolean): Record<string, FakeHandler> => ({
  "GET /v1/databases/d1/merges/m1": ok(detailOf(mergeOf({ status: "draft" }))),
  "GET /v1/databases/d1/branches": ok([branchOf({ id: "b-main", name: "main", protected: protectedTarget }), devBranch]),
});
const mergePage = () => <MergePage projectId="p1" databaseId="d1" mergeId="m1" createClient={() => Promise.reject(new Error("no worker"))} />;
const mergeReady = () => screen.findByRole("heading", { name: /dev/ });

/** The Access tab: "Your access" for everyone, and for Admins the table, whose first row's editor is opened. */
const accessTab = () => <AccessTab project={meFor("admin").tenant!.projects[0]} />;
const accessReady = async () => {
  await screen.findByRole("heading", { name: "Your access" });
  await waitFor(() =>
    expect(screen.queryByRole("button", { name: "Edit access for bo" }) ?? screen.queryByText(/decide who has access/)).not.toBeNull(),
  );
  const edit = screen.queryByRole("button", { name: "Edit access for bo" });
  if (edit) {
    edit.click();
    await screen.findByRole("group", { name: "Access for bo" });
  }
};

const CONTROLS: Control[] = [
  { name: "Create database", operationId: "createDatabase", ui: projectPage, ready: projectReady, query: button("Create database") },
  { name: "SQL editor", operationId: "runSql", ui: projectPage, ready: projectReady, query: button("SQL") },
  { name: "Connection details", operationId: "getDatabaseConnection", ui: projectPage, ready: projectReady, query: button("Connection details") },
  { name: "Reset database password", operationId: "resetDatabasePassword", ui: projectPage, ready: projectReady, query: button("Reset password") },
  { name: "Delete database", operationId: "deleteDatabase", ui: projectPage, ready: projectReady, query: button("Delete") },
  {
    name: "Rename project", operationId: "updateProject",
    ui: () => <ProjectSettings project={info} databases={[]} onDeleted={() => {}} />,
    ready: () => screen.findAllByText(/app users/i), query: () => screen.queryByLabelText(/Project name/i),
  },
  {
    name: "Delete project", operationId: "deleteProject",
    ui: () => <ProjectSettings project={info} databases={[]} onDeleted={() => {}} />,
    ready: () => screen.findAllByText(/app users/i), query: button("Delete project"),
  },
  { name: "New project", operationId: "createProject", ui: () => <Dashboard />, ready: () => screen.findByRole("heading", { name: "demo" }), query: button("New project") },
  { name: "Create role", operationId: "createRole", ui: () => <BranchRoles databaseId="d1" branchId="m" />, ready: rolesReady, query: button("Create role") },
  { name: "Delete role", operationId: "deleteRole", ui: () => <BranchRoles databaseId="d1" branchId="m" />, ready: rolesReady, query: button("Delete reporting") },
  { name: "Reset role password", operationId: "resetRolePassword", ui: () => <BranchRoles databaseId="d1" branchId="m" />, ready: rolesReady, query: button("Reset password for reporting") },
  { name: "Reveal a role", operationId: "getBranchConnection", ui: () => <BranchRoles databaseId="d1" branchId="m" />, ready: rolesReady, query: button("Reveal reporting") },
  {
    name: "Save database settings", operationId: "updateDatabase",
    ui: () => <DatabaseSettings databaseId="d1" settings={settings as never} />,
    ready: () => screen.findByText(/History window/), query: button("Save settings"),
  },
  {
    name: "Enable data API", operationId: "enableDataApi",
    ui: () => <DataApiDatabases databaseId="d1" branchId="m" databases={[dbView("web1", true, "on"), dbView("extra", false, "off")]} picked="extra" onPick={() => {}} />,
    ready: () => screen.findAllByText(/extra/), query: button("Enable data API"),
  },
  {
    name: "Disable data API", operationId: "disableDataApi",
    ui: () => <DataApiDatabases databaseId="d1" branchId="m" databases={[dbView("web1", true, "on"), dbView("extra", false, "on")]} picked="extra" onPick={() => {}} />,
    ready: () => screen.findAllByText(/extra/), query: button("Disable data API"),
  },
  {
    name: "Save default schema", operationId: "updatePgDatabase",
    ui: () => <DataApiDatabases databaseId="d1" branchId="m" databases={[dbView("web1", true, "on"), dbView("extra", false, "on")]} picked="extra" onPick={() => {}} />,
    ready: () => screen.findAllByText(/extra/), query: button("Save default schema"),
  },
  {
    name: "Create key", operationId: "createKey",
    ui: () => <DataApiKeys databaseId="d1" branchId="m" keys={[dataKey]} databases={[dbView("web1", true, "on")]} />,
    ready: () => screen.findAllByText("k1"), query: button("Create key"),
  },
  {
    name: "Revoke key", operationId: "revokeKey",
    ui: () => <DataApiKeys databaseId="d1" branchId="m" keys={[dataKey]} databases={[dbView("web1", true, "on")]} />,
    ready: () => screen.findAllByText("k1"), query: button("Revoke k1"),
  },
  {
    name: "Rotate key", operationId: "rotateKey",
    ui: () => <DataApiKeys databaseId="d1" branchId="m" keys={[dataKey]} databases={[dbView("web1", true, "on")]} />,
    ready: () => screen.findAllByText("k1"), query: button("Rotate k1"),
  },
  {
    name: "Edit a key's databases", operationId: "updateKey",
    ui: () => <DataApiKeys databaseId="d1" branchId="m" keys={[dataKey]} databases={[dbView("web1", true, "on")]} />,
    ready: () => screen.findAllByText("k1"), query: button("Edit databases"),
  },
  { name: "Protect a branch", operationId: "updateBranch", ui: branchActions(mainOpen), ready: branchReady, query: button("Protect") },
  { name: "Unprotect a branch", operationId: "updateBranch", ui: branchActions(mainProtected), ready: branchReady, query: button("Unprotect") },
  { name: "Delete an open branch", operationId: "deleteBranch", ui: branchActions(devOpen, [mainProtected, devOpen]), ready: branchReady, query: button("Delete branch") },
  { name: "Delete a protected branch", operationId: "deleteBranch", protectedBranch: true, ui: branchActions(mainProtected), ready: branchReady, query: button("Delete branch") },
  { name: "Restore an open branch", operationId: "restoreBranch", ui: branchActions(devOpen, [mainProtected, devOpen]), ready: branchReady, query: button("Restore") },
  { name: "Restore a protected branch", operationId: "restoreBranch", protectedBranch: true, ui: branchActions(mainProtected), ready: branchReady, query: button("Restore") },
  { name: "Reset an open branch", operationId: "resetBranchToParent", ui: branchActions(devOpen, [mainProtected, devOpen]), ready: branchReady, query: button("Reset to parent") },
  { name: "Update an open branch from its parent", operationId: "createMerge", ui: branchActions(devOpen, [mainProtected, devOpen]), ready: branchReady, query: button("Update from main") },
  { name: "Merge into a protected parent", operationId: "createMerge", protectedBranch: true, ui: branchActions(devOpen, [mainProtected, devOpen]), ready: branchReady, query: button("Merge into main") },
  {
    name: "Create branch", operationId: "createBranch",
    ui: () => <DatabaseBranches projectId="p1" database={database as never} />,
    ready: () => screen.findByText(/branches/), query: button("Create branch"),
  },
  {
    name: "Recompute a merge into an open branch", operationId: "recomputeMerge",
    routes: mergeRoutes(false), ui: mergePage, ready: mergeReady, query: button("Recompute"),
  },
  {
    name: "Recompute a merge into a protected branch", operationId: "recomputeMerge", protectedBranch: true,
    routes: mergeRoutes(true), ui: mergePage, ready: mergeReady, query: button("Recompute"),
  },
  {
    name: "Rehearse a merge into a protected branch", operationId: "rehearseMerge", protectedBranch: true,
    routes: mergeRoutes(true), ui: mergePage, ready: mergeReady, query: () => screen.queryByRole("region", { name: "Rehearsal" }),
  },
  { name: "Invite people", operationId: "createInvite", ui: () => <MembersHarness />, ready: () => screen.findAllByText(/\d people$/), query: button("Invite people") },
  { name: "A member's actions", operationId: "updateMember", ui: () => <MembersHarness />, ready: () => screen.findAllByText(/\d people$/), query: button("More actions for bo") },
  { name: "Approve a sign-up", operationId: "approveMember", ui: () => <MembersHarness />, ready: () => screen.findAllByText(/\d people$/), query: button("Approve ed") },
  { name: "Decline a sign-up", operationId: "declineMember", ui: () => <MembersHarness />, ready: () => screen.findAllByText(/\d people$/), query: button("Decline ed") },
  { name: "Rename the tenant", operationId: "updateTenant", ui: () => <MembersHarness />, ready: () => screen.findAllByText(/\d people$/), query: button("Rename") },
  { name: "Activity", operationId: "listAccessEvents", ui: () => <MembersHarness />, ready: () => screen.findAllByText(/\d people$/), query: () => screen.queryByRole("tab", { name: "Activity" }) },
  { name: "Edit a grant", operationId: "putProjectGrant", ui: accessTab, ready: accessReady, query: button("Edit access for bo") },
  { name: "Take a grant away", operationId: "deleteProjectGrant", ui: accessTab, ready: accessReady, query: button("Take access away") },
  { name: "Narrow a grant on a database", operationId: "putGrantOverride", ui: accessTab, ready: accessReady, query: button("Change narrowing on web1") },
  { name: "Stop narrowing a grant", operationId: "deleteGrantOverride", ui: accessTab, ready: accessReady, query: button("Stop narrowing on web1") },
  { name: "Add people to a project", operationId: "createProjectInvite", ui: accessTab, ready: accessReady, query: button("Add people") },
  {
    name: "A project's access log", operationId: "listProjectAccessEvents", ui: accessTab, ready: accessReady,
    query: () => screen.queryByRole("heading", { name: "Access changes" }),
  },
  {
    name: "Instance settings", operationId: "updateInstanceSettings", ui: () => <MembersHarness />,
    ready: async () => {
      await screen.findAllByText(/\d people$/);
      // The tab waits for the settings read (instance admins only).
      await new Promise((r) => setTimeout(r, 30));
    },
    query: () => screen.queryByRole("tab", { name: "Instance" }),
  },
];

describe("capability invariant", () => {
  it("every operationId in the table has a documented access level", () => {
    for (const c of CONTROLS) expect(() => accessOf(c.operationId), c.name).not.toThrow();
  });

  for (const c of CONTROLS) {
    for (const p of PERSONAS) {
      const access = c.access ?? accessOf(c.operationId);
      const expected = allows(access, p, c.protectedBranch ?? false, scopeOf(c));
      it(`${c.name} (${c.operationId}, ${access}) is ${expected ? "shown" : "hidden"} for ${p}`, async () => {
        renderAs(p, c.ui(), c.routes, scopeOf(c));
        await c.ready();
        if (expected) await waitFor(() => expect(c.query()).not.toBeNull());
        else expect(c.query()).toBeNull();
      });
    }
  }
});
