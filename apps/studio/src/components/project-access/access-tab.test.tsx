import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { AccessEvent, BranchRole, MeResponse as Me, Member, Project, ProjectGrant } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { AccessTab } from "./access-tab";

const orders = { database_id: "d1", name: "orders", postgres_database: "orders", status: "ready", host: "h", port: 1, branch_count: 1 } as const;
const events = { database_id: "d2", name: "events", postgres_database: "events", status: "ready", host: "h", port: 1, branch_count: 1 } as const;
const project: Project = { id: "p1", slug: "shop", name: "Shop", status: "active", databases: [orders, events] };

function meFor(over: { id: string; tenantAdmin?: boolean; role: "viewer" | "developer" | "admin"; roles?: "all" | string[] }): Me {
  return {
    sub: over.id, email: `${over.id}@example.test`, name: over.id, tenant_id: "t1", instance_admin: false, tenant_admins: [],
    member: { id: over.id, status: "active", tenant_admin: over.tenantAdmin === true },
    projects: [{ project_id: "p1", platform_role: over.role, roles: over.roles ?? [], databases: [] }],
    user: { sub: over.id, org_id: "o" },
    tenant: { id: "t1", slug: "demo", display_name: null, projects: [project] },
  };
}
const adminMe = meFor({ id: "ana", role: "admin", roles: "all" });

const member = (id: string, extra: Partial<Member> = {}): Member => ({
  id, email: `${id}@example.test`, display_name: id[0].toUpperCase() + id.slice(1), status: "active", tenant_admin: false, has_access: true,
  invite: null, accepted_at: null, suspended_at: null, suspended_by: null, created_at: "2026-10-01T00:00:00Z", access: { level: "none", projects: [] }, ...extra,
});
const people: Member[] = [
  member("ana"),
  member("tia", { tenant_admin: true }),
  member("bo"),
  member("cy", { status: "invited", invite: { id: "i1", expires_at: "2099-01-01T00:00:00Z" } }),
  member("eve"),
  member("dee", { has_access: false }),
  member("gus"),
  member("fay"),
  member("nul", { email: null, has_access: false }),
];

const grants: ProjectGrant[] = [
  { member_id: "ana", platform_role: "admin", pg_roles: [], overrides: [] },
  {
    member_id: "bo", platform_role: "developer", pg_roles: ["billing_admin"],
    overrides: [{ database_id: "d1", platform_role: "viewer", pg_roles: null }],
  },
  { member_id: "cy", platform_role: "viewer", pg_roles: ["reporting"], overrides: [] },
  // R17: an Admin grant is every role.
  { member_id: "eve", platform_role: "admin", pg_roles: "all", overrides: [] },
  { member_id: "gus", platform_role: "developer", pg_roles: ["billing_admin", "reporting"], overrides: [] },
  { member_id: "fay", platform_role: "admin", pg_roles: ["app_login"], overrides: [] },
];

const role = (name: string, extra: Partial<BranchRole> = {}): BranchRole =>
  ({ name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "generated", pending: false, ...extra }) as BranchRole;
const ordersRoles = [
  role("o", { origin: "owner", level: "owner_level", login: true }),
  role("billing"),
  role("billing_admin", { member_of: ["billing"] }),
  role("app_login", { login: true }),
  role("reporting"),
];
const eventsRoles = [role("o", { origin: "owner", level: "owner_level", login: true }), role("reporting"), role("audit")];

const branch = (id: string) => ({
  id, name: "main", parent_branch_id: null, kind: "normal", status: "ready", host: "h", port: 1, branched_at_time: null,
  branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null, protected: false, main_database_missing: false,
});

const ok = (body: unknown, status = 200): FakeHandler => () => jsonResponse(body, status);
const refusal = (extra: Record<string, unknown>): FakeHandler => () =>
  jsonResponse({ error: { code: "invalid_request", message: "`app_login` can't be held below Admin: it is a LOGIN role", ...extra } }, 422);

function setup(opts: { me?: Me; routes?: Record<string, FakeHandler> } = {}) {
  const f = fakeFetch({
    "GET /v1/me": ok(opts.me ?? adminMe),
    "GET /v1/members": ok(people),
    "GET /v1/projects/p1/grants": ok(grants),
    "GET /v1/projects/p1/access-events": ok({ events: [], next_before: null }),
    "GET /v1/databases/d1/branches": ok([branch("m1")]),
    "GET /v1/databases/d2/branches": ok([branch("m2")]),
    "GET /v1/databases/d1/branches/m1/roles": ok(ordersRoles),
    "GET /v1/databases/d2/branches/m2/roles": ok(eventsRoles),
    "GET /v1/invites/delivery": ok({ email_available: true, reason: null }),
    ...opts.routes,
  });
  return { ...renderWithRouter(<AccessTab project={project} />, { fetch: f }), f };
}

const call = (f: { calls: Request[] }, method: string, path: string) =>
  f.calls.find((r) => r.method === method && new URL(r.url).pathname === path);

async function row(name: string | RegExp) {
  const table = await screen.findByRole("table", { name: "People with access" });
  return within(table).getByRole("row", { name });
}
async function openEditor(name: string) {
  await userEvent.click(await screen.findByRole("button", { name: `Edit access for ${name}` }));
  return screen.findByRole("group", { name: `Access for ${name}` });
}

describe("AccessTab", () => {
  it("admin_is_labelled_as_owner_and_no_owner_chip", async () => {
    setup();
    const eve = await screen.findByText("eve@example.test");
    const eveRow = eve.closest("tr")!;
    expect(within(eveRow).getByText("includes the database owner")).toBeInTheDocument();
    // R17: an Admin holds every role: no chips, the owner ("o") never one.
    expect(within(eveRow).getByText("All roles")).toBeInTheDocument();
    expect(within(eveRow).queryByText("o")).toBeNull();
    expect(within(eveRow).queryByText(/owner chip/i)).toBeNull();

    // The editor (the shared grant card) says "All roles" instead of a role list; narrowing stays per database.
    const editor = await openEditor("Eve");
    expect(within(editor).getByText("All roles")).toBeInTheDocument();
    expect(within(editor).queryByRole("checkbox", { name: "billing_admin" })).toBeNull();
    expect(within(editor).queryByRole("combobox", { name: "Postgres roles" })).toBeNull();
  });

  it("narrow_table_keeps_roles_and_narrowing_on_separate_lines", async () => {
    // Below 40rem "Per database" folds into the roles cell: inline spans ran into "No rolesSame everywhere".
    const original = window.matchMedia;
    window.matchMedia = ((query: string) =>
      ({ matches: true, media: query, onchange: null, addEventListener: () => {}, removeEventListener: () => {}, addListener: () => {}, removeListener: () => {}, dispatchEvent: () => false }) as MediaQueryList);
    try {
      setup();
      const anaRow = (await screen.findByText("ana@example.test")).closest("tr")!;
      const none = within(anaRow).getByText("No roles");
      const same = within(anaRow).getByText("Same everywhere");
      expect(none.closest("td")).toBe(same.closest("td"));
      for (const el of [none, same]) expect(["P", "DIV"]).toContain(el.tagName);
      // At 375px the table can't scroll sideways: one column holds the person, their role, roles
      // and narrowings; the other the editor's toggle.
      const table = screen.getByRole("table", { name: "People with access" });
      expect(within(table).getAllByRole("columnheader").map((h) => h.textContent)).toEqual(["Person", "Edit"]);
      expect(within(anaRow).getAllByRole("cell")[0]).toHaveTextContent("Admin");
      expect(within(anaRow).getAllByRole("cell")[0]).toHaveTextContent("No roles");
    } finally {
      window.matchMedia = original;
    }
  });

  it("tenant_admins_are_read_only_rows", async () => {
    setup();
    const tia = (await screen.findByText("tia@example.test")).closest("tr")!;
    expect(within(tia).getByText("Tenant admin")).toBeInTheDocument();
    expect(within(tia).getByText("Every role, including the database owner")).toBeInTheDocument();
    expect(within(tia).queryByRole("button")).toBeNull();
    expect(screen.getByText(/Tenant admins are Admin on every project/)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Members page" })).toBeInTheDocument();
  });

  it("shows_the_grant_as_the_rule_postgres_applies", async () => {
    setup();
    const bo = (await screen.findByText("bo@example.test")).closest("tr")!;
    expect(within(bo).getByText("Developer")).toBeInTheDocument();
    expect(within(bo).getByText("billing_admin")).toBeInTheDocument();
    expect(await within(bo).findByText("+ billing")).toBeInTheDocument();
    expect(within(bo).getByText("orders: Viewer")).toBeInTheDocument();
    const cy = (await screen.findByText("cy@example.test")).closest("tr")!;
    expect(within(cy).getByText("Invited")).toBeInTheDocument();
    expect(within(cy).getByText("Same everywhere")).toBeInTheDocument();
  });

  it("override_offers_only_narrower_choices", async () => {
    const { f } = setup({
      routes: {
        "PUT /v1/projects/p1/grants/bo/overrides": ok({ database_id: "d2", platform_role: "viewer", pg_roles: null }),
      },
    });
    const editor = await openEditor("Bo");
    const line = within(editor).getByRole("group", { name: "events" });
    expect(within(line).getByText("Same as the project")).toBeInTheDocument();
    await userEvent.click(within(line).getByRole("button", { name: "Narrow access on events" }));
    const form = await within(editor).findByRole("form", { name: "Narrow on events" });
    const save = within(form).getByRole("button", { name: "Save narrowing" });
    // Nothing chosen yet: nothing is narrower.
    expect(save).toBeDisabled();
    await userEvent.click(within(form).getByRole("checkbox", { name: "Same as the project" }));
    // The grant is Developer: Viewer and Developer are offered, Admin is absent, not disabled.
    expect(within(form).getByRole("radio", { name: "Viewer" })).toBeInTheDocument();
    expect(within(form).getByRole("radio", { name: "Developer" })).toBeInTheDocument();
    expect(within(form).queryByRole("radio", { name: "Admin" })).toBeNull();
    // Developer is the grant's own role: still not narrower.
    expect(save).toBeDisabled();
    await userEvent.click(within(form).getByRole("radio", { name: "Viewer" }));
    expect(save).toBeEnabled();
    await userEvent.click(save);
    await waitFor(() => expect(call(f, "PUT", "/v1/projects/p1/grants/bo/overrides")).toBeDefined());
    expect(await call(f, "PUT", "/v1/projects/p1/grants/bo/overrides")!.clone().json()).toEqual({
      database_id: "d2",
      platform_role: "viewer",
      pg_roles: null,
    });
    expect(await screen.findByRole("status")).toHaveTextContent("Narrowed Bo on events.");
  });

  it("override_roles_are_a_subset_and_stop_narrowing_deletes", async () => {
    const { f } = setup({
      routes: { "DELETE /v1/projects/p1/grants/bo/overrides": () => new Response(null, { status: 204 }) },
    });
    const editor = await openEditor("Bo");
    const line = within(editor).getByRole("group", { name: "orders" });
    expect(within(line).getByText("Viewer")).toBeInTheDocument();
    await userEvent.click(within(line).getByRole("button", { name: "Stop narrowing on orders" }));
    await waitFor(() => expect(call(f, "DELETE", "/v1/projects/p1/grants/bo/overrides")).toBeDefined());
    expect(new URL(call(f, "DELETE", "/v1/projects/p1/grants/bo/overrides")!.url).searchParams.get("database_id")).toBe("d1");
  });

  it("invite_into_project", async () => {
    const { f } = setup({
      routes: {
        "POST /v1/projects/p1/invites": ok({ outcome: "invited", member: member("new", { status: "invited" }), invite_url: null }),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Add people" }));
    const dialog = await screen.findByRole("dialog", { name: "Add people to Shop" });
    await userEvent.type(within(dialog).getByLabelText("Email"), "New@Example.test");
    await userEvent.click(await within(dialog).findByRole("radio", { name: "Viewer" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    await waitFor(() => expect(call(f, "POST", "/v1/projects/p1/invites")).toBeDefined());
    expect(await call(f, "POST", "/v1/projects/p1/invites")!.clone().json()).toEqual({
      email: "new@example.test",
      platform_role: "viewer",
      pg_roles: [],
      delivery: "email",
    });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(screen.getByRole("status")).toHaveTextContent(
      "Invited new@example.test with Viewer access. The link in their email works for 72 hours; access counts once they accept.",
    );
  });

  it("invite_link_never_reaches_the_cache", async () => {
    const url = "http://studio.test/accept-invite#abc";
    const { queryClient } = setup({
      routes: {
        "GET /v1/invites/delivery": ok({ email_available: false, reason: "email_not_configured" }),
        "POST /v1/projects/p1/invites": ok({ outcome: "invited", member: member("new", { status: "invited" }), invite_url: url }),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Add people" }));
    const dialog = await screen.findByRole("dialog", { name: "Add people to Shop" });
    await userEvent.type(within(dialog).getByLabelText("Email"), "new@example.test");
    await userEvent.click(await within(dialog).findByRole("button", { name: "Create link" }));
    const panel = await screen.findByRole("dialog", { name: "Invite link for new@example.test" });
    expect(within(panel).getByDisplayValue(url)).toBeInTheDocument();
    const cached = JSON.stringify([
      queryClient.getQueryCache().getAll().map((q) => q.state.data),
      queryClient.getMutationCache().getAll().map((m) => m.state.data),
    ]);
    expect(cached).not.toContain("accept-invite");
  });

  it("activity_lists_events", async () => {
    const event: AccessEvent = {
      id: 7, at: new Date().toISOString(), action: "grant_put", actor_member_id: "ana", actor_email: "ana@example.test",
      target_member_id: "bo", target_email: "bo@example.test", project_id: "p1", project_name: "Shop", database_id: null, branch_id: null,
      before: null, after: { platform_role: "developer", pg_roles: [] },
    } as AccessEvent;
    setup({ routes: { "GET /v1/projects/p1/access-events": ok({ events: [event], next_before: null }) } });
    expect(await screen.findByRole("heading", { name: "Access changes" })).toBeInTheDocument();
    expect(await screen.findByText("Ana gave Bo Developer on Shop")).toBeInTheDocument();
  });

  it("activity_empty", async () => {
    setup();
    expect(await screen.findByText("No access changes on this project yet.")).toBeInTheDocument();
  });

  it("non_admin_sees_only_your_access", async () => {
    const { f } = setup({ me: meFor({ id: "dev", role: "developer", roles: ["billing_admin"] }) });
    expect(await screen.findByRole("heading", { name: "Your access" })).toBeInTheDocument();
    expect(screen.getByText("You're a Developer on Shop.")).toBeInTheDocument();
    expect(await screen.findByText("+ billing")).toBeInTheDocument();
    expect(screen.getByText(/Runs SQL and edits data/)).toBeInTheDocument();
    expect(screen.getByText("Some databases may narrow this further.")).toBeInTheDocument();
    expect(screen.getByText(/Project Admins and tenant admins decide who has access/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add people" })).toBeNull();
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.queryByRole("heading", { name: "Access changes" })).toBeNull();
    // No Admin-only route is called.
    const paths = f.calls.map((r) => `${r.method} ${new URL(r.url).pathname}`);
    expect(paths.filter((p) => /grants|invites|access-events|\/members/.test(p))).toEqual([]);
  });

  it("non_admin_without_roles_says_so", async () => {
    setup({ me: meFor({ id: "v", role: "viewer", roles: [] }) });
    expect(await screen.findByText("You can't act as any Postgres role here.")).toBeInTheDocument();
  });

  it("grant_refusal_marks_the_row", async () => {
    setup({
      routes: {
        "PUT /v1/projects/p1/grants/bo": refusal({ role: "billing_admin", chain: ["billing_admin", "app_login"], why: "login", database_id: "d1", project_id: "p1" }),
      },
    });
    const editor = await openEditor("Bo");
    await userEvent.click(await within(editor).findByRole("checkbox", { name: "reporting" }));
    await userEvent.click(within(editor).getByRole("button", { name: "Save" }));
    // On Bo's card, in Studio's words (never the server's sentence), with the chain and database
    // (the shared card is given the project's databases).
    const alert = await within(editor).findByText((_, el) => el?.tagName === "P" && el.textContent === "Only Admin can hold billing_admin on orders: it reaches app_login, a LOGIN role.");
    expect(alert).toBeInTheDocument();
    expect(within(editor).queryByText(/can't be held below Admin/)).toBeNull();
    expect(within(editor).getByText("billing_admin → app_login")).toBeInTheDocument();
    expect(within(editor).getByText("on orders")).toBeInTheDocument();
    // The orders line (the database in the error) is flagged; events is not.
    expect(within(within(editor).getByRole("group", { name: "orders" })).getByText(/why the grant was refused/)).toBeInTheDocument();
    expect(within(within(editor).getByRole("group", { name: "events" })).queryByText(/why the grant was refused/)).toBeNull();
    // Nobody else's row got an editor or an error.
    expect(screen.queryByRole("group", { name: "Access for Cy" })).toBeNull();
  });

  it("a_refusal_on_a_database_that_isn't_narrowed_has_no_narrowing_note", async () => {
    // Gus has no narrowing: the grant itself is what's refused, so nothing points at narrowing.
    setup({
      routes: {
        "PUT /v1/projects/p1/grants/gus": refusal({ role: "reporting", chain: ["reporting"], why: "login", database_id: "d1", project_id: "p1" }),
      },
    });
    const editor = await openEditor("Gus");
    await userEvent.click(await within(editor).findByRole("checkbox", { name: "billing_admin" }));
    await userEvent.click(within(editor).getByRole("button", { name: "Save" }));
    expect(await within(editor).findByText((_, el) => el?.tagName === "P" && el.textContent === "Only Admin can hold reporting on orders: it's a LOGIN role.")).toBeInTheDocument();
    expect(within(editor).queryByText(/why the grant was refused/)).toBeNull();
  });

  it("override_refusal_marks_the_line", async () => {
    setup({
      routes: {
        "PUT /v1/projects/p1/grants/bo/overrides": ok(
          { error: { code: "invalid_request", message: "That narrows nothing.", database_id: "d2", project_id: "p1" } },
          422,
        ),
      },
    });
    const editor = await openEditor("Bo");
    const events = within(editor).getByRole("group", { name: "events" });
    await userEvent.click(within(events).getByRole("button", { name: "Narrow access on events" }));
    const form = await within(editor).findByRole("form", { name: "Narrow on events" });
    await userEvent.click(within(form).getByRole("checkbox", { name: "Same as the project" }));
    await userEvent.click(within(form).getByRole("radio", { name: "Viewer" }));
    await userEvent.click(within(form).getByRole("button", { name: "Save narrowing" }));
    expect(await within(events).findByText("That narrows nothing.")).toBeInTheDocument();
    expect(within(within(editor).getByRole("group", { name: "orders" })).queryByText("That narrows nothing.")).toBeNull();
  });

  it("role_checklist_merges_databases", async () => {
    setup();
    const editor = await openEditor("Bo");
    // `reporting` is on both databases: one checkbox, no "only on" note.
    const reporting = await within(editor).findAllByRole("checkbox", { name: "reporting" });
    expect(reporting).toHaveLength(1);
    expect(reporting[0].closest("li")).not.toHaveTextContent("only on");
    // `audit` exists only on events, `billing` only on orders.
    expect((await within(editor).findByRole("checkbox", { name: "audit" })).closest("li")).toHaveTextContent("only on events");
    expect(within(editor).getByRole("checkbox", { name: "billing_admin" }).closest("li")).toHaveTextContent("only on orders");
    // The LOGIN role is Admin-only below Admin: disabled with its reason.
    const login = within(editor).getByRole("checkbox", { name: "app_login" });
    expect(login).toBeDisabled();
    expect(login.closest("li")).toHaveTextContent("Admin only: LOGIN role");
  });

  it("take_access_away_confirms", async () => {
    const { f } = setup({ routes: { "DELETE /v1/projects/p1/grants/bo": () => new Response(null, { status: 204 }) } });
    const editor = await openEditor("Bo");
    await userEvent.click(within(editor).getByRole("button", { name: "Take access away" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Take Bo's access to Shop away?" });
    expect(within(dialog).getByText(/Per-database narrowings are kept/)).toBeInTheDocument();
    expect(call(f, "DELETE", "/v1/projects/p1/grants/bo")).toBeUndefined();
    await userEvent.click(within(dialog).getByRole("button", { name: "Take access away" }));
    await waitFor(() => expect(call(f, "DELETE", "/v1/projects/p1/grants/bo")).toBeDefined());
    expect(await screen.findByRole("status")).toHaveTextContent("Took Bo's access to Shop away.");
  });

  it("save_puts_the_grant_and_reports", async () => {
    const { f } = setup({
      routes: { "PUT /v1/projects/p1/grants/bo": ok({ member_id: "bo", platform_role: "viewer", pg_roles: ["billing_admin"], overrides: [] }) },
    });
    const editor = await openEditor("Bo");
    expect(within(editor).queryByRole("button", { name: "Save" })).toBeNull();
    await userEvent.click(within(editor).getByRole("radio", { name: "Viewer" }));
    const save = within(editor).getByRole("button", { name: "Save" });
    expect(save).toBeEnabled();
    await userEvent.click(save);
    await waitFor(() => expect(call(f, "PUT", "/v1/projects/p1/grants/bo")).toBeDefined());
    expect(await call(f, "PUT", "/v1/projects/p1/grants/bo")!.clone().json()).toEqual({ platform_role: "viewer", pg_roles: ["billing_admin"] });
    expect(await screen.findByRole("status")).toHaveTextContent("Saved Bo's access.");
    expect(screen.queryByRole("group", { name: "Access for Bo" })).toBeNull();
  });

  it("only_tenant_admins_says_add_people", async () => {
    setup({ routes: { "GET /v1/projects/p1/grants": ok([]) } });
    expect(await screen.findByText(/No one else has access yet/)).toBeInTheDocument();
    expect(await row(/tia/)).toBeInTheDocument();
  });

  it("forbidden_grants_fall_back_to_your_access", async () => {
    setup({
      routes: { "GET /v1/projects/p1/grants": ok({ error: { code: "forbidden", message: "no" } }, 403) },
    });
    expect(await screen.findByText("Only project Admins can see who has access to Shop.")).toBeInTheDocument();
  });

  async function openAdd() {
    await userEvent.click(await screen.findByRole("button", { name: "Add people" }));
    return screen.findByRole("dialog", { name: "Add people to Shop" });
  }

  it("existing_grant_blocks_add", async () => {
    const { f } = setup();
    const dialog = await openAdd();
    await userEvent.type(within(dialog).getByLabelText("Email"), "bo@example.test");
    expect(within(dialog).getByText("Bo already has Developer here. Adding replaces it; edit their row instead.")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Add" })).toBeDisabled();
    // A tenant admin is Admin everywhere.
    await userEvent.clear(within(dialog).getByLabelText("Email"));
    await userEvent.type(within(dialog).getByLabelText("Email"), "tia@example.test");
    expect(within(dialog).getByText("Tia already has Admin here. Adding replaces it; edit their row instead.")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Add" })).toBeDisabled();
    expect(call(f, "POST", "/v1/projects/p1/invites")).toBeUndefined();
  });

  it("adding_an_existing_member_sets_access", async () => {
    const { f } = setup({
      routes: { "POST /v1/projects/p1/invites": ok({ outcome: "existing_member", member: member("dee"), invite_url: null }) },
    });
    const dialog = await openAdd();
    await userEvent.type(within(dialog).getByLabelText("Email"), "dee@example.test");
    // Someone already here needs no invite delivery.
    expect(within(dialog).queryByRole("radio", { name: /One-time link/ })).toBeNull();
    expect(within(dialog).getByText("They're already here; their access is set right away.")).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole("button", { name: "Add" }));
    await waitFor(() => expect(call(f, "POST", "/v1/projects/p1/invites")).toBeDefined());
    expect(await screen.findByRole("status")).toHaveTextContent("Dee now has Developer access to Shop.");
  });

  it("email_field_suggests_members_without_access", async () => {
    setup();
    const dialog = await openAdd();
    const input = within(dialog).getByLabelText("Email");
    await userEvent.type(input, "e");
    const options = await screen.findAllByRole("option");
    // Dee has no access here; Eve has a grant, and a member without an email is never offered.
    expect(options.map((o) => o.textContent?.trim())).toEqual(expect.arrayContaining([expect.stringContaining("dee@example.test")]));
    expect(options.every((o) => (o.textContent ?? "").trim() !== "")).toBe(true);
    expect(options.some((o) => (o.textContent ?? "").includes("eve@example.test"))).toBe(false);
    await userEvent.click(options.find((o) => (o.textContent ?? "").includes("dee@example.test"))!);
    expect(input).toHaveValue("dee@example.test");
    // Any other address still works.
    await userEvent.clear(input);
    await userEvent.type(input, "zed@else.test");
    expect(input).toHaveValue("zed@else.test");
  });

  it("take_access_away_moves_focus_to_add_people", async () => {
    setup({ routes: { "DELETE /v1/projects/p1/grants/bo": () => new Response(null, { status: 204 }) } });
    const editor = await openEditor("Bo");
    await userEvent.click(within(editor).getByRole("button", { name: "Take access away" }));
    const dialog = await screen.findByRole("alertdialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Take access away" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Add people" })).toHaveFocus());
  });

  it("override_subset_sends_the_chosen_roles_and_returns_focus", async () => {
    const { f } = setup({
      routes: { "PUT /v1/projects/p1/grants/gus/overrides": ok({ database_id: "d1", platform_role: null, pg_roles: ["reporting"] }) },
    });
    const editor = await openEditor("Gus");
    const line = () => within(editor).getByRole("group", { name: "orders" });
    const puts = () => f.calls.filter((r) => r.method === "PUT" && new URL(r.url).pathname.endsWith("/overrides"));

    await userEvent.click(within(line()).getByRole("button", { name: "Narrow access on orders" }));
    let form = await within(editor).findByRole("form", { name: "Narrow on orders" });
    await userEvent.click(within(form).getByRole("checkbox", { name: "All of the project's roles" }));
    await userEvent.click(within(form).getByRole("checkbox", { name: /^billing_admin/ }));
    await userEvent.click(within(form).getByRole("button", { name: "Save narrowing" }));
    await waitFor(() => expect(puts()).toHaveLength(1));
    expect(await puts()[0].clone().json()).toEqual({ database_id: "d1", platform_role: null, pg_roles: ["reporting"] });
    // Focus returns to the line's button.
    await waitFor(() => expect(within(line()).getByRole("button", { name: "Narrow access on orders" })).toHaveFocus());

    // No roles left is a narrowing too.
    await userEvent.click(within(line()).getByRole("button", { name: "Narrow access on orders" }));
    form = await within(editor).findByRole("form", { name: "Narrow on orders" });
    await userEvent.click(within(form).getByRole("checkbox", { name: "All of the project's roles" }));
    await userEvent.click(within(form).getByRole("checkbox", { name: /^billing_admin/ }));
    await userEvent.click(within(form).getByRole("checkbox", { name: /^reporting/ }));
    await userEvent.click(within(form).getByRole("button", { name: "Save narrowing" }));
    await waitFor(() => expect(puts()).toHaveLength(2));
    expect(await puts()[1].clone().json()).toEqual({ database_id: "d1", platform_role: null, pg_roles: [] });
  });

  it("stop_narrowing_returns_focus_to_the_line", async () => {
    setup({ routes: { "DELETE /v1/projects/p1/grants/bo/overrides": () => new Response(null, { status: 204 }) } });
    const editor = await openEditor("Bo");
    const line = within(editor).getByRole("group", { name: "orders" });
    await userEvent.click(within(line).getByRole("button", { name: "Stop narrowing on orders" }));
    await waitFor(() => expect(within(line).getByRole("button", { name: "Change narrowing on orders" })).toHaveFocus());
  });

  it("admin_narrowing_notes_the_owner", async () => {
    setup();
    const editor = await openEditor("Eve");
    const line = within(editor).getByRole("group", { name: "events" });
    await userEvent.click(within(line).getByRole("button", { name: "Narrow access on events" }));
    const form = await within(editor).findByRole("form", { name: "Narrow on events" });
    expect(within(form).queryByText(/can't act as the database owner/)).toBeNull();
    await userEvent.click(within(form).getByRole("checkbox", { name: "Same as the project" }));
    await userEvent.click(within(form).getByRole("radio", { name: "Developer" }));
    // R17: Eve's every role narrowed below Admin without named roles keeps none.
    expect(within(form).getByText("Below Admin, Eve can't act as the database owner on events, nor any role: pick the roles to keep.")).toBeInTheDocument();
  });

  // R17: narrowing an Admin's every role offers the database's own roles and sends the ones kept.
  it("narrowing_every_role_offers_the_databases_roles", async () => {
    const { f } = setup({
      routes: { "PUT /v1/projects/p1/grants/eve/overrides": ok({ database_id: "d1", platform_role: null, pg_roles: ["reporting"] }) },
    });
    const editor = await openEditor("Eve");
    await userEvent.click(within(within(editor).getByRole("group", { name: "orders" })).getByRole("button", { name: "Narrow access on orders" }));
    const form = await within(editor).findByRole("form", { name: "Narrow on orders" });
    await userEvent.click(within(form).getByRole("checkbox", { name: "All roles" }));
    await userEvent.click(await within(form).findByRole("checkbox", { name: /^reporting/ }));
    await userEvent.click(within(form).getByRole("button", { name: "Save narrowing" }));
    const puts = () => f.calls.filter((r) => r.method === "PUT");
    await waitFor(() => expect(puts()).toHaveLength(1));
    expect(await puts()[0].clone().json()).toEqual({ database_id: "d1", platform_role: null, pg_roles: ["reporting"] });
  });

  it("admin_only_roles_keep_admin_in_a_narrowing", async () => {
    setup();
    const editor = await openEditor("Fay");
    // app_login is a LOGIN role on orders: below Admin it is refused there.
    await userEvent.click(within(within(editor).getByRole("group", { name: "orders" })).getByRole("button", { name: "Narrow access on orders" }));
    const form = await within(editor).findByRole("form", { name: "Narrow on orders" });
    await userEvent.click(within(form).getByRole("checkbox", { name: "Same as the project" }));
    expect(within(form).getByRole("radio", { name: "Viewer" })).toHaveAttribute("aria-disabled", "true");
    expect(within(form).getByRole("radio", { name: "Developer" })).toHaveAttribute("aria-disabled", "true");
    expect(within(form).getByText(/Keep Admin while app_login is chosen/)).toBeInTheDocument();
    // Drop the role from this database's list and the lower roles open up.
    await userEvent.click(within(form).getByRole("checkbox", { name: "All of the project's roles" }));
    await userEvent.click(within(form).getByRole("checkbox", { name: /^app_login/ }));
    expect(within(form).getByRole("radio", { name: "Viewer" })).not.toHaveAttribute("aria-disabled", "true");
    // On events app_login doesn't exist, so nothing is locked there.
    await userEvent.click(within(within(editor).getByRole("group", { name: "events" })).getByRole("button", { name: "Narrow access on events" }));
    const events = await within(editor).findByRole("form", { name: "Narrow on events" });
    await userEvent.click(within(events).getByRole("checkbox", { name: "Same as the project" }));
    expect(within(events).getByRole("radio", { name: "Viewer" })).not.toHaveAttribute("aria-disabled", "true");
  });

  it("stale_override_role_is_lowered_in_the_form", async () => {
    const stale = grants.map((g) =>
      g.member_id === "bo" ? { ...g, overrides: [{ database_id: "d2", platform_role: "admin", pg_roles: null }] } : g,
    );
    setup({ routes: { "GET /v1/projects/p1/grants": ok(stale) } });
    const editor = await openEditor("Bo");
    const line = within(editor).getByRole("group", { name: "events" });
    await userEvent.click(within(line).getByRole("button", { name: "Change narrowing on events" }));
    const form = await within(editor).findByRole("form", { name: "Narrow on events" });
    expect(within(form).getByRole("radio", { name: "Developer" })).toBeChecked();
    expect(within(form).getByText(/was Admin, above the grant's Developer/)).toBeInTheDocument();
  });

  it("expander_controls_the_editor", async () => {
    setup();
    const button = await screen.findByRole("button", { name: "Edit access for Bo" });
    expect(button).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(button);
    const editor = await screen.findByRole("group", { name: "Access for Bo" });
    expect(button).toHaveAttribute("aria-expanded", "true");
    expect(button.getAttribute("aria-controls")).toBe(editor.id);
    expect(editor.id).not.toBe("");
  });

  it("opening_another_row_clears_the_status", async () => {
    setup({
      routes: { "PUT /v1/projects/p1/grants/bo": ok({ member_id: "bo", platform_role: "viewer", pg_roles: ["billing_admin"], overrides: [] }) },
    });
    const editor = await openEditor("Bo");
    await userEvent.click(within(editor).getByRole("radio", { name: "Viewer" }));
    await userEvent.click(within(editor).getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Saved Bo's access.");
    await userEvent.click(await screen.findByRole("button", { name: "Edit access for Cy" }));
    await screen.findByRole("group", { name: "Access for Cy" });
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("folds_per_database_under_roles_below_40rem", async () => {
    const original = Object.getOwnPropertyDescriptor(window, "matchMedia");
    const stub = (matches: boolean) =>
      Object.defineProperty(window, "matchMedia", {
        configurable: true,
        writable: true,
        value: (media: string) => ({ matches, media, addEventListener() {}, removeEventListener() {} }),
      });
    try {
      stub(false);
      const wide = setup();
      expect(await screen.findByRole("columnheader", { name: "Per database" })).toBeInTheDocument();
      wide.unmount();

      stub(true);
      setup();
      const bo = (await screen.findByText("bo@example.test")).closest("tr")!;
      expect(screen.queryByRole("columnheader", { name: "Per database" })).toBeNull();
      // The narrowing chip now sits in the Postgres roles cell.
      expect(within(bo).getByText("billing_admin").closest("td")).toHaveTextContent("orders: Viewer");
    } finally {
      if (original) Object.defineProperty(window, "matchMedia", original);
      else delete (window as { matchMedia?: unknown }).matchMedia;
    }
  });

  it("no_access_calls_no_role_routes", async () => {
    const { f } = setup({ me: { ...meFor({ id: "x", role: "viewer" }), projects: [] } });
    expect(await screen.findByText("You don't have access to Shop.")).toBeInTheDocument();
    const paths = f.calls.map((r) => new URL(r.url).pathname);
    expect(paths.filter((p) => /branches|roles|grants/.test(p))).toEqual([]);
  });

  describe("on the hub's shared components (S4, H10)", () => {
    const linkTo = (a: HTMLElement) => new URL(a.getAttribute("href") ?? "", "http://x");

    it("access_tab_uses_the_shared_grant_card_and_links_to_members", async () => {
      const { f } = setup({
        routes: {
          "PUT /v1/projects/p1/grants/bo": ok({ member_id: "bo", platform_role: "developer", pg_roles: ["billing_admin", "reporting"], overrides: [] }),
        },
      });
      // "Manage in Members" and each person link into the hub, for this project.
      const manage = await screen.findByRole("link", { name: "Manage in Members" });
      expect(linkTo(manage).pathname).toBe("/app/members");
      expect(linkTo(manage).searchParams.get("project")).toBe("p1");
      expect([null, "people"]).toContain(linkTo(manage).searchParams.get("tab"));
      const boRow = (await screen.findByText("bo@example.test")).closest("tr")!;
      const bo = within(boRow).getByRole("link", { name: "Bo" });
      expect(linkTo(bo).pathname).toBe("/app/members/bo");
      expect(linkTo(bo).searchParams.get("project")).toBe("p1");
      const tiaRow = (await screen.findByText("tia@example.test")).closest("tr")!;
      expect(linkTo(within(tiaRow).getByRole("link", { name: "Tia" })).pathname).toBe("/app/members/tia");

      // The editor is the shared card: a role checklist (no combobox), the owner and personal logins never offered.
      const editor = await openEditor("Bo");
      expect(within(editor).queryByRole("combobox", { name: "Postgres roles" })).toBeNull();
      expect(within(editor).getByRole("checkbox", { name: "billing_admin" })).toBeChecked();
      expect(within(editor).getByRole("checkbox", { name: "reporting" })).not.toBeChecked();
      expect(within(editor).queryByRole("checkbox", { name: "o" })).toBeNull();
      // A clean card has no Save (the narrowing forms have their own, named differently).
      expect(within(editor).queryByRole("button", { name: "Save" })).toBeNull();

      // Save sends the person's own grant only: a platform role and the checked roles.
      await userEvent.click(within(editor).getByRole("checkbox", { name: "reporting" }));
      // One Save for the card once it is dirty.
      expect(within(editor).getAllByRole("button", { name: "Save" })).toHaveLength(1);
      const save = within(editor).getByRole("button", { name: "Save" });
      expect(save).toBeEnabled();
      await userEvent.click(save);
      await waitFor(() => expect(call(f, "PUT", "/v1/projects/p1/grants/bo")).toBeDefined());
      const body = await call(f, "PUT", "/v1/projects/p1/grants/bo")!.clone().json();
      expect(Object.keys(body).sort()).toEqual(["pg_roles", "platform_role"]);
      expect(body.platform_role).toBe("developer");
      expect([...body.pg_roles].sort()).toEqual(["billing_admin", "reporting"]);
      expect(await screen.findByRole("status")).toHaveTextContent("Saved Bo's access.");
      expect(screen.queryByRole("group", { name: "Access for Bo" })).toBeNull();
      // Nothing else was written: one PUT, no override, no other member.
      expect(f.calls.filter((r) => r.method === "PUT")).toHaveLength(1);
    });

    it("a_former_member_has_no_link_and_a_personal_login_is_never_a_choice", async () => {
      const ghost: ProjectGrant = { member_id: "ghost", platform_role: "viewer", pg_roles: [], overrides: [] };
      setup({
        routes: {
          "GET /v1/projects/p1/grants": ok([...grants, ghost]),
          "GET /v1/databases/d1/branches/m1/roles": ok([...ordersRoles, role("bo_personal", { origin: "personal", login: true })]),
        },
      });
      const ghostRow = (await screen.findByText("A former member")).closest("tr")!;
      expect(within(ghostRow).queryByRole("link")).toBeNull();
      const editor = await openEditor("Bo");
      await within(editor).findByRole("checkbox", { name: "reporting" });
      expect(within(editor).queryByRole("checkbox", { name: "bo_personal" })).toBeNull();
    });

    it("the_shared_card_still_blocks_admin_only_roles_below_admin", async () => {
      // Gus is a Developer; app_login is Admin-only (LOGIN): it can't be ticked, so Save stays off.
      const { f } = setup();
      const editor = await openEditor("Gus");
      const login = await within(editor).findByRole("checkbox", { name: "app_login" });
      expect(login).toBeDisabled();
      await userEvent.click(login);
      expect(login).not.toBeChecked();
      expect(within(editor).queryByRole("button", { name: "Save" })).toBeNull();
      expect(call(f, "PUT", "/v1/projects/p1/grants/gus")).toBeUndefined();
    });
  });
});
