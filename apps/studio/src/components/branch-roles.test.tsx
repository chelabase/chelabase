import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { BranchRole, ConnectionDetails } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, sequence, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { ProjectCapsContext, type ProjectCaps } from "@/lib/capabilities";
import { BranchRoles } from "./branch-roles";

const role = (over: Partial<BranchRole> & { name: string }): BranchRole => ({
  origin: "api",
  level: "plain",
  login: true,
  password: "generated",
  blocked_keys: [],
  pending: false,
  created_at: "2026-10-01T09:00:00Z",
  inherit: true,
  connection_limit: -1,
  valid_until: null,
  bypass_rls: false,
  member_of: [],
  member_no_set: [],
  member_admin: [],
  guard_exempt: false,
  ...over,
});
const owner = role({ name: "chelabase_owner", origin: "owner", level: "owner_level" });
const reader = role({ name: "reporting" });
const admin = role({ name: "ops", level: "owner_level" });
const sqlRole = role({ name: "analyst", origin: "sql", password: "set_in_sql" });
const md5Role = role({ name: "legacy", origin: "sql", password: "md5_unusable" });

const details = (user: string, password: string): ConnectionDetails => ({
  host: "ep-aaaaaaaaaaaa.localtest.me",
  port: 4432,
  database: "neondb",
  user,
  password,
  password_state: "generated",
  ca_file: null,
  http_url: "https://ep-aaaaaaaaaaaa.localtest.me",
  uri: `postgres://${user}:${password}@ep-aaaaaaaaaaaa.localtest.me:4432/neondb`,
});

const ROLES = "/v1/databases/db/branches/br/roles";
const CONNECTION = "/v1/databases/db/branches/br/connection";
const list = (rows: BranchRole[]): FakeHandler => () => jsonResponse(rows);
const connection: FakeHandler = (req) => {
  const r = new URL(req.url).searchParams.get("role") ?? "chelabase_owner";
  if (r === "analyst") {
    return jsonResponse({ ...details(r, ""), password: null, password_state: "set_in_sql" });
  }
  return jsonResponse(details(r, `${r}-password-xyz`));
};
const refused = (status: number, code: string, message: string, extra: object = {}): FakeHandler => () =>
  jsonResponse({ error: { code, message, ...extra } }, status);

const LOGINS = "/v1/databases/db/personal-logins";
// The panel links into the Members hub, so it renders inside a router and is given its project.
function setup(routes: Record<string, FakeHandler> = {}, rows: BranchRole[] = [owner, reader, admin]) {
  const f = fakeFetch({ [`GET ${ROLES}`]: list(rows), [`GET ${CONNECTION}`]: connection, [`GET ${LOGINS}`]: () => jsonResponse({ logins: [] }), ...routes });
  const result = renderWithRouter(<BranchRoles projectId="p1" databaseId="db" branchId="br" />, { fetch: f });
  return { ...result, fetch: f };
}
const sent = (f: FakeFetch) => f.calls.map((r) => `${r.method} ${new URL(r.url).pathname}`);
const btn = (name: string | RegExp) => screen.getByRole("button", { name });
const rowOf = async (name: string) => within(await screen.findByRole("row", { name: new RegExp(name) }));
function cachedState(queryClient: ReturnType<typeof setup>["queryClient"]): string {
  return JSON.stringify([
    queryClient.getQueryCache().findAll().map((q) => q.state),
    queryClient.getMutationCache().getAll().map((m) => m.state),
  ]);
}

describe("BranchRoles", () => {
  it("lists_roles_with_badges", async () => {
    setup({}, [owner, role({ name: "reporting", pending: true }), admin]);
    const r = await rowOf("reporting");
    expect(r.getByText("Plain")).toBeInTheDocument();
    expect(r.getByText("Pending")).toBeInTheDocument();
    expect((await rowOf("ops")).getByText("Owner-level")).toBeInTheDocument();
    expect((await rowOf("chelabase_owner")).getByText("Owner-level")).toBeInTheDocument();
  });

  it("roles_show_origin_level_and_password_state", async () => {
    setup({}, [owner, reader, sqlRole, md5Role]);
    const o = await rowOf("chelabase_owner");
    expect(o.getByText("Owner")).toBeInTheDocument();
    const r = await rowOf("reporting");
    expect(r.getByText("Studio")).toBeInTheDocument();
    expect(r.getByText("Plain")).toBeInTheDocument();
    expect(r.getByText("Generated")).toBeInTheDocument();
    expect(r.getByText("Yes")).toBeInTheDocument();
    const a = await rowOf("analyst");
    expect(a.getByText("SQL")).toBeInTheDocument();
    expect(a.getByText("Password set in SQL — reset to get a revealable one")).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Origin" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Login" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Password" })).toBeInTheDocument();
  });

  it("role_list_shows_new_attributes", async () => {
    const group = role({ name: "readers", login: false, origin: "sql", password: "none" });
    const member = role({
      name: "etl",
      inherit: false,
      connection_limit: 5,
      valid_until: "2030-01-01T00:00:00Z",
      bypass_rls: true,
      member_of: ["readers", "writers"],
    });
    setup({}, [owner, group, member]);
    expect(await screen.findByRole("columnheader", { name: "Attributes" })).toBeInTheDocument();
    const m = await rowOf("etl");
    expect(m.getByText("Bypasses RLS")).toBeInTheDocument();
    expect(m.getByText("No inherit")).toBeInTheDocument();
    expect(m.getByText("At most 5 connections")).toBeInTheDocument();
    expect(m.getByText("Valid until 2030-01-01 00:00 UTC")).toBeInTheDocument();
    expect(m.getByText("Member of readers, writers")).toBeInTheDocument();
    // Defaults say nothing; the list stays read-only.
    const g = await rowOf("^readers");
    expect(g.queryByText(/Bypasses RLS|No inherit|connections|Valid until|Member of/)).toBeNull();
    expect(screen.queryByRole("checkbox")).toBeNull();
  });

  it("md5_warning", async () => {
    setup({}, [owner, md5Role]);
    const r = await rowOf("legacy");
    expect(r.getByText("MD5 password: can't log in through Chelabase; reset it")).toBeInTheDocument();
    expect(r.getByRole("button", { name: "Reset password for legacy" })).toBeInTheDocument();
  });

  it("blocked_keys_are_named", async () => {
    setup({}, [owner, role({ name: "ops", level: "owner_level", blocked_keys: [{ id: "k1", name: "ci-key" }, { id: "k2", name: "worker" }] })]);
    expect(await screen.findByText("Data API keys blocked: ci-key, worker")).toBeInTheDocument();
  });

  it("sql_role_reset_offers_revealable_password", async () => {
    setup(
      {
        [`POST ${ROLES}/analyst/reset-password`]: () => new Response(null, { status: 204 }),
        [`GET ${CONNECTION}`]: () => jsonResponse(details("analyst", "analyst-fresh-pw")),
      },
      [owner, sqlRole],
    );
    await userEvent.click((await rowOf("analyst")).getByRole("button", { name: "Reset password for analyst" }));
    await userEvent.click(btn("Confirm reset"));
    expect(await screen.findByText("analyst-fresh-pw")).toBeInTheDocument();
  });

  it("reveal_without_a_password_shows_the_state_text", async () => {
    setup({}, [owner, sqlRole]);
    await userEvent.click((await rowOf("analyst")).getByRole("button", { name: "Reveal analyst" }));
    const section = await screen.findByRole("region", { name: "Connection for analyst" });
    expect(within(section).getAllByText("Password set in SQL — reset to get a revealable one").length).toBe(1);
    expect(within(section).queryByRole("button", { name: "Copy password" })).not.toBeInTheDocument();
  });

  it("role_reveal_shows_ca_file_hint", async () => {
    setup(
      { [`GET ${CONNECTION}`]: () => jsonResponse({ ...details("chelabase_owner", "pw-xyz"), ca_file: ".chelabase/neon/proxy-ca.crt" }) },
      [owner, reader],
    );
    await userEvent.click((await rowOf("chelabase_owner")).getByRole("button", { name: "Reveal chelabase_owner" }));
    const section = await screen.findByRole("region", { name: "Connection for chelabase_owner" });
    expect(within(section).getByText(/Local dev: pass the proxy's CA with/)).toHaveTextContent(
      "sslrootcert=.chelabase/neon/proxy-ca.crt",
    );
  });

  it("polls_while_a_role_is_pending", async () => {
    const f = setup({
      [`GET ${ROLES}`]: sequence(list([owner, role({ name: "reporting", pending: true })]), list([owner, reader])),
    });
    expect(await screen.findByText("Pending")).toBeInTheDocument();
    await vi.waitFor(() => expect(screen.queryByText("Pending")).not.toBeInTheDocument(), { timeout: 4000 });
    expect(sent(f.fetch).filter((s) => s === `GET ${ROLES}`).length).toBeGreaterThanOrEqual(2);
  });

  // Owner-level creation stays in the panel (Part 4): its own small form, owner-level only. Plain
  // roles are made through the shared dialog (see "on the hub's shared components").
  it("owner_level_create_reveals_the_password", async () => {
    const created = role({ name: "boss", level: "owner_level", pending: true });
    const { fetch } = setup({ [`POST ${ROLES}`]: () => jsonResponse(created, 201) });
    await screen.findByText("reporting");
    await userEvent.type(screen.getByLabelText("Owner-level role name"), "boss");
    await userEvent.click(btn("Create owner-level role"));
    expect(await screen.findByText("boss-password-xyz")).toBeInTheDocument();
    const post = fetch.calls.find((r) => r.method === "POST")!;
    expect(await post.clone().json()).toEqual({ name: "boss", owner_level: true });
    expect(fetch.calls.some((r) => r.url.includes("/connection?role=boss"))).toBe(true);
    expect(screen.getByText("Copy this password now. You can reveal it again later.")).toBeInTheDocument();
  });

  it("delete_goes_through_the_shared_dialog_and_shows_keys_on_role_in_use", async () => {
    const del = vi.fn(refused(409, "role_in_use", "role is used by Data API keys", { keys: [{ id: "k1", name: "ci-key" }] }));
    setup({ [`DELETE ${ROLES}/reporting`]: del, [`GET ${ROLES}/reporting/delete-preview`]: () => jsonResponse({ successor: "chelabase_orphaned", exposures: [], confirm: null }) });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Delete reporting" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete role reporting" });
    // The old inline confirm is gone.
    expect(screen.queryByText(/Objects owned by reporting move to/)).toBeNull();
    expect(del).not.toHaveBeenCalled();
    const go = within(dialog).getByRole("button", { name: "Delete role" });
    await vi.waitFor(() => expect(go).toBeEnabled());
    await userEvent.click(go);
    expect(await within(dialog).findByText("ci-key")).toBeInTheDocument();
    expect(within(dialog).getByText(/Data API tab/)).toBeInTheDocument();
  });

  // A role made in SQL may be named anything: the name is one encoded path segment, so
  // "%61" isn't decoded to "a" and "/", "?" and "#" don't change the route.
  it("role_names_are_encoded_in_the_path", async () => {
    const odd = role({ name: "a/b%61?#", origin: "sql", password: "set_in_sql" });
    const encoded = `${ROLES}/a%2Fb%2561%3F%23`;
    const del = vi.fn(() => jsonResponse({ successor: "chelabase_orphaned", reassigned: { count: 0, definers: [] } }));
    const reset = vi.fn(() => new Response(null, { status: 204 }));
    const { fetch } = setup(
      {
        [`DELETE ${encoded}`]: del,
        [`POST ${encoded}/reset-password`]: reset,
        [`GET ${encoded}/delete-preview`]: () => jsonResponse({ successor: "chelabase_orphaned", exposures: [], confirm: null }),
      },
      [owner, odd],
    );
    await userEvent.click((await rowOf("a/b%61")).getByRole("button", { name: "Reset password for a/b%61?#" }));
    await userEvent.click(btn("Confirm reset"));
    await vi.waitFor(() => expect(reset).toHaveBeenCalled());
    await userEvent.click((await rowOf("a/b%61")).getByRole("button", { name: "Delete a/b%61?#" }));
    const dialog = await screen.findByRole("dialog", { name: /^Delete role / });
    const go = within(dialog).getByRole("button", { name: "Delete role" });
    await vi.waitFor(() => expect(go).toBeEnabled());
    await userEvent.click(go);
    await vi.waitFor(() => expect(del).toHaveBeenCalled());
    const raw = fetch.calls.filter((r) => r.method !== "GET").map((r) => `${r.method} ${new URL(r.url).pathname}`);
    expect(raw).toEqual([`POST ${encoded}/reset-password`, `DELETE ${encoded}`]);
  });

  it("owner_has_no_delete", async () => {
    setup();
    const r = await rowOf("chelabase_owner");
    expect(r.getByRole("button", { name: "Reveal chelabase_owner" })).toBeInTheDocument();
    expect(r.getByRole("button", { name: "Reset password for chelabase_owner" })).toBeInTheDocument();
    expect(r.queryByRole("button", { name: /Delete/ })).not.toBeInTheDocument();
  });

  it("reset_reveals_the_new_password", async () => {
    const { fetch } = setup({ [`POST ${ROLES}/reporting/reset-password`]: () => new Response(null, { status: 204 }) });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Reset password for reporting" }));
    await userEvent.click(btn("Confirm reset"));
    expect(await screen.findByText("reporting-password-xyz")).toBeInTheDocument();
    expect(fetch.calls.some((r) => r.url.includes("/connection?role=reporting"))).toBe(true);
  });

  it("reset_not_applied_shows_the_notice", async () => {
    setup({ [`POST ${ROLES}/reporting/reset-password`]: refused(503, "not_applied", "not applied") });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Reset password for reporting" }));
    await userEvent.click(btn("Confirm reset"));
    expect(await screen.findByText("The new password is stored but not yet applied; it will be retried.")).toBeInTheDocument();
    expect(await screen.findByText("reporting-password-xyz")).toBeInTheDocument();
  });

  it("create_not_applied_refreshes_and_shows_the_pending_role", async () => {
    const message = "the role was stored but not yet applied; it will be retried";
    setup({
      [`GET ${ROLES}`]: sequence(list([owner, reader]), list([owner, reader, role({ name: "newbie", pending: true })])),
      [`POST ${ROLES}`]: refused(503, "not_applied", message),
    });
    await screen.findByText("reporting");
    await userEvent.type(screen.getByLabelText("Owner-level role name"), "newbie");
    await userEvent.click(btn("Create owner-level role"));
    expect(await screen.findByRole("alert")).toHaveTextContent(message);
    const r = await rowOf("newbie");
    expect(r.getByText("Pending")).toBeInTheDocument();
  });

  it("a_failed_delete_refreshes_the_list", async () => {
    setup({
      [`GET ${ROLES}`]: sequence(list([owner, reader, admin]), list([owner, admin])),
      [`DELETE ${ROLES}/reporting`]: refused(503, "not_applied", "deleted, not applied yet"),
      [`GET ${ROLES}/reporting/delete-preview`]: () => jsonResponse({ successor: "chelabase_orphaned", exposures: [], confirm: null }),
    });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Delete reporting" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete role reporting" });
    const go = within(dialog).getByRole("button", { name: "Delete role" });
    await vi.waitFor(() => expect(go).toBeEnabled());
    await userEvent.click(go);
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("deleted, not applied yet");
    // The delete may have landed: closing the dialog refreshes the list.
    await userEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await vi.waitFor(() => expect(screen.queryByText("reporting")).not.toBeInTheDocument());
  });

  it("a_failed_reset_refreshes_the_list", async () => {
    const { fetch } = setup({
      [`POST ${ROLES}/reporting/reset-password`]: refused(500, "internal", "internal error"),
    });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Reset password for reporting" }));
    const before = sent(fetch).filter((s) => s === `GET ${ROLES}`).length;
    await userEvent.click(btn("Confirm reset"));
    expect(await screen.findByRole("alert")).toHaveTextContent("internal error");
    await vi.waitFor(() => expect(sent(fetch).filter((s) => s === `GET ${ROLES}`).length).toBeGreaterThan(before));
  });

  it("name_hint_shows_only_after_a_submit_with_a_bad_name", async () => {
    const { fetch } = setup();
    await screen.findByText("reporting");
    expect(screen.queryByText("Enter a name for the role.")).not.toBeInTheDocument();
    await userEvent.click(btn("Create owner-level role"));
    const hint = screen.getByText("Enter a name for the role.");
    expect(screen.getByLabelText("Owner-level role name")).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByLabelText("Owner-level role name")).toHaveAccessibleDescription(hint.textContent!);
    await userEvent.type(screen.getByLabelText("Owner-level role name"), "Bad-Name");
    expect(screen.queryByText("Enter a name for the role.")).not.toBeInTheDocument();
    await userEvent.click(btn("Create owner-level role"));
    expect(screen.getByText(/lowercase letters, digits and underscores/)).toBeInTheDocument();
    expect(fetch.calls.some((r) => r.method === "POST")).toBe(false);
  });

  it("passwords_never_enter_the_query_cache", async () => {
    const { queryClient } = setup({
      [`POST ${ROLES}`]: () => jsonResponse(role({ name: "newbie" }), 201),
      [`POST ${ROLES}/reporting/reset-password`]: () => new Response(null, { status: 204 }),
    });
    await userEvent.type(await screen.findByLabelText("Owner-level role name"), "newbie");
    await userEvent.click(btn("Create owner-level role"));
    await screen.findByText("newbie-password-xyz");
    expect(cachedState(queryClient)).not.toContain("newbie-password-xyz");
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Reset password for reporting" }));
    await userEvent.click(btn("Confirm reset"));
    await screen.findByText("reporting-password-xyz");
    expect(cachedState(queryClient)).not.toContain("reporting-password-xyz");
    await userEvent.click((await rowOf("ops")).getByRole("button", { name: "Reveal ops" }));
    await screen.findByText("ops-password-xyz");
    expect(cachedState(queryClient)).not.toContain("password-xyz");
  });
});

describe("login guard exemptions (PB)", () => {
  const EXEMPT = (name: string) => `/v1/databases/db/branches/br/guard-exemptions/${name}`;

  it("exempt_toggle_puts_and_deletes", async () => {
    const exempt = { ...reader, guard_exempt: true };
    const put = vi.fn(() => jsonResponse({ exemptions: ["analyst", "reporting"] }));
    const del = vi.fn(() => jsonResponse({ exemptions: [] }));
    renderWithRouter(<BranchRoles projectId="p1" databaseId="db" branchId="br" />, {
      fetch: fakeFetch({
        [`GET ${ROLES}`]: () => jsonResponse([owner, exempt, sqlRole]),
        [`PUT ${EXEMPT("analyst")}`]: put,
        [`DELETE ${EXEMPT("reporting")}`]: del,
      }),
    });
    const on = await screen.findByRole("switch", { name: "Exempt reporting from the login guard" });
    expect(on).toBeChecked();
    const off = screen.getByRole("switch", { name: "Exempt analyst from the login guard" });
    expect(off).not.toBeChecked();
    expect(screen.queryByRole("switch", { name: `Exempt ${owner.name} from the login guard` })).toBeNull();
    await userEvent.click(off);
    await vi.waitFor(() => expect(put).toHaveBeenCalled());
    await userEvent.click(on);
    await vi.waitFor(() => expect(del).toHaveBeenCalled());
  });
});

describe("on the hub's shared components (S4, H10, R3)", () => {
  const hrefOf = (a: HTMLElement) => new URL(a.getAttribute("href") ?? "", "http://x");
  const personal = (name: string) => role({ name, origin: "personal" });
  const login = (member_id: string, role_name: string, status = "active") => ({
    member_id, database_id: "db", database_name: "orders", project_id: "p1", role_name, valid_until: "2099-01-01T00:00:00Z",
    created_at: "2026-08-03T00:00:00Z", status, revoked_at: null, revoked_reason: null,
  });
  const mixed = [owner, reader, sqlRole, personal("sofia"), personal("priya")];
  const preview = (confirm: string | null, exposures: unknown[] = []): FakeHandler => (req) =>
    jsonResponse({ successor: new URL(req.url).searchParams.get("successor") ?? "chelabase_orphaned", exposures, confirm });
  const developer = {
    role: "developer", roles: [], tenantAdmin: false, atLeast: (l: string) => l === "viewer" || l === "developer",
  } as unknown as ProjectCaps;

  it("branch_roles_panel_hides_personal_logins_and_links_to_the_role_page", async () => {
    setup(
      { [`GET ${LOGINS}`]: () => jsonResponse({ logins: [login("m1", "sofia"), login("m2", "priya"), login("m3", "gone", "revoked")] }) },
      mixed,
    );
    // The roles are listed with a link to their page; the owner is not a hub role, so no link.
    const link = await screen.findByRole("link", { name: "reporting" });
    expect(hrefOf(link).pathname).toBe("/app/members/roles/db/br/reporting");
    expect(screen.getByRole("link", { name: "analyst" })).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "chelabase_owner" })).toBeNull();
    // Personal logins are never rows (R3).
    expect(screen.queryByText("sofia")).toBeNull();
    expect(screen.queryByText("priya")).toBeNull();
    expect(screen.queryByRole("row", { name: /sofia|priya/ })).toBeNull();
    // P24: an Admin gets the count of active logins as a link to the hub's Roles tab for this branch.
    const count = await screen.findByRole("link", { name: /2 personal logins/ });
    for (const a of [count, screen.getByRole("link", { name: "Manage in Members" })]) {
      const u = hrefOf(a);
      expect(u.pathname).toBe("/app/members");
      expect(u.searchParams.get("tab")).toBe("roles");
      expect(u.searchParams.get("project")).toBe("p1");
      expect(u.searchParams.get("database")).toBe("db");
      expect(u.searchParams.get("branch")).toBe("br");
    }
  });

  it("no_count_link_without_active_logins_and_none_for_non_admins", async () => {
    const none = setup({}, mixed);
    await screen.findByRole("link", { name: "reporting" });
    expect(screen.queryByRole("link", { name: /personal login/ })).toBeNull();
    none.unmount();

    const f = fakeFetch({ [`GET ${ROLES}`]: list(mixed), [`GET ${LOGINS}`]: () => jsonResponse({ logins: [login("m1", "sofia")] }) });
    renderWithRouter(
      <ProjectCapsContext.Provider value={developer}>
        <BranchRoles projectId="p1" databaseId="db" branchId="br" />
      </ProjectCapsContext.Provider>,
      { fetch: f },
    );
    await screen.findByRole("link", { name: "reporting" });
    expect(screen.queryByRole("link", { name: /personal login/ })).toBeNull();
    // The Admin-only read is not even requested; the plain link into the hub stays.
    expect(f.calls.filter((r) => new URL(r.url).pathname === LOGINS)).toHaveLength(0);
    expect(screen.getByRole("link", { name: "Manage in Members" })).toBeInTheDocument();
  });

  it("create_role_opens_the_shared_dialog_and_refreshes_the_list", async () => {
    const { fetch } = setup({
      [`GET ${ROLES}`]: sequence(list([owner, reader]), list([owner, reader, role({ name: "newbie", login: false, password: "none" })])),
      [`POST ${ROLES}`]: () => jsonResponse(role({ name: "newbie", login: false, password: "none" }), 201),
    });
    await screen.findByText("reporting");
    await userEvent.click(btn("Create role"));
    const dialog = await screen.findByRole("dialog", { name: "Create a role" });
    // The shared dialog: no owner-level option (that stays in the panel).
    expect(within(dialog).queryByText(/Owner-level/i)).toBeNull();
    await userEvent.type(within(dialog).getByLabelText("Name"), "newbie");
    await userEvent.click(within(dialog).getByRole("button", { name: "Create role" }));
    await vi.waitFor(() => expect(fetch.calls.some((r) => r.method === "POST")).toBe(true));
    expect(await fetch.calls.find((r) => r.method === "POST")!.clone().json()).toEqual({ name: "newbie", login: false, member_of: [] });
    await vi.waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(await screen.findByRole("link", { name: "newbie" })).toBeInTheDocument();
  });

  it("delete_offers_a_successor_and_asks_to_confirm_what_it_would_expose", async () => {
    const { fetch } = setup({
      [`GET ${ROLES}/doomed/delete-preview`]: preview(null),
    }, [owner, reader, sqlRole, admin, role({ name: "doomed", login: false })]);
    await userEvent.click((await rowOf("doomed")).getByRole("button", { name: "Delete doomed" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete role doomed" });
    const select = within(dialog).getByLabelText("Successor") as HTMLSelectElement;
    // Quarantine by default; managed, plain roles are offered, never the owner or the role itself.
    expect(select.value).toBe("chelabase_orphaned");
    const values = within(select).getAllByRole("option").map((o) => (o as HTMLOptionElement).value);
    expect(values).toEqual(expect.arrayContaining(["reporting", "analyst"]));
    expect(values).not.toContain("chelabase_owner");
    expect(values).not.toContain("doomed");
    // The owner-level role (ops) can't take objects.
    expect(values).not.toContain("ops");
    // The default successor is previewed as the dialog opens.
    await vi.waitFor(() =>
      expect(fetch.calls.some((r) => new URL(r.url).pathname.endsWith("/doomed/delete-preview") && new URL(r.url).searchParams.get("successor") === "chelabase_orphaned")).toBe(true),
    );
  });

  it("delete_with_a_named_successor_sends_the_preview_token_and_clears_a_revealed_connection", async () => {
    const exposure = { database: "orders", kind: "function", identity: "public.refund(uuid)", grantees: ["analyst"] };
    const del = vi.fn(() => jsonResponse({ successor: "analyst", reassigned: { count: 2, definers: [] } }));
    const { fetch } = setup(
      {
        [`GET ${ROLES}/reporting/delete-preview`]: preview("tok-1", [exposure]),
        [`DELETE ${ROLES}/reporting`]: del,
      },
      [owner, reader, sqlRole],
    );
    // Reveal first: the delete must close that panel.
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Reveal reporting" }));
    await screen.findByRole("region", { name: "Connection for reporting" });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Delete reporting" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete role reporting" });
    await userEvent.selectOptions(within(dialog).getByLabelText("Successor"), "analyst");
    expect(await within(dialog).findByText("public.refund(uuid)")).toBeInTheDocument();
    const go = within(dialog).getByRole("button", { name: "Delete role" });
    expect(go).toBeDisabled();
    await userEvent.click(within(dialog).getByRole("checkbox", { name: /I understand/ }));
    await userEvent.click(go);
    await vi.waitFor(() => expect(del).toHaveBeenCalled());
    const url = new URL(fetch.calls.find((r) => r.method === "DELETE")!.url);
    expect(url.searchParams.get("successor")).toBe("analyst");
    expect(url.searchParams.get("confirm")).toBe("tok-1");
    await userEvent.click(await within(dialog).findByRole("button", { name: "Done" }));
    await vi.waitFor(() => expect(screen.queryByRole("region", { name: "Connection for reporting" })).toBeNull());
  });
});
