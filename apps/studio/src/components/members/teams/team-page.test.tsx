import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { ANA, BILLING, DEV, P1, T1, apiError, calls, mountPage, ok, role, team } from "./test-setup";

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const card = (name: string) => screen.findByRole("region", { name });
const body = async (r: Request | undefined) => (r ? await r.clone().json() : undefined);

describe("Team page: members", () => {
  it("members_checklist_saves_changes_in_sequence_with_a_result_each", async () => {
    const m = mountPage(T1, {
      routes: {
        [`PUT /v1/teams/${T1}/members/${ANA}`]: ok(BILLING),
        [`DELETE /v1/teams/${T1}/members/${DEV}`]: apiError(422, "access_refused", "Dev would keep an Admin-only role"),
      },
    });
    // A pending sign-up isn't offered (it has to be approved first).
    await screen.findByRole("checkbox", { name: "Ana Ng" });
    expect(screen.queryByRole("checkbox", { name: "Ed Pending" })).toBeNull();
    expect(screen.getByRole("checkbox", { name: "Sofia Marchetti" })).toBeChecked();

    await m.user.click(screen.getByRole("checkbox", { name: "Ana Ng" }));
    await m.user.click(screen.getByRole("checkbox", { name: "Dev Dunn" }));
    expect(screen.getByText("2 unsaved changes")).toBeInTheDocument();
    await m.user.click(screen.getByRole("button", { name: "Save members" }));

    const results = await screen.findByRole("status", { name: "Results" });
    await waitFor(() => expect(within(results).getAllByRole("listitem")).toHaveLength(2));
    expect(within(results).getByText("Couldn't remove Dev Dunn: Dev would keep an Admin-only role")).toBeInTheDocument();
    expect(within(results).getByText("Added Ana Ng")).toBeInTheDocument();
    // One call per change, in the roster's order (Dev before Ana), the refused one not stopping the rest.
    const writes = m.f.calls.filter((r) => r.method === "PUT" || r.method === "DELETE").map((r) => `${r.method} ${new URL(r.url).pathname}`);
    expect(writes).toEqual([`DELETE /v1/teams/${T1}/members/${DEV}`, `PUT /v1/teams/${T1}/members/${ANA}`]);
  });

  it("a_reader_sees_who_is_in_the_team_without_checkboxes", async () => {
    mountPage(T1, { caller: "developer" });
    expect(await screen.findByRole("link", { name: "Sofia Marchetti" })).toBeInTheDocument();
    expect(screen.queryByRole("checkbox")).toBeNull();
    expect(screen.queryByRole("button", { name: "Save members" })).toBeNull();
  });
});

describe("Team page: access", () => {
  it("team_access_cards_have_no_narrowing_and_send_put_team_grant", async () => {
    const stored = team(T1, "Billing", { ...BILLING, projects: [{ project_id: P1, platform_role: "admin", pg_roles: [] }] });
    const m = mountPage(T1, { routes: { [`PUT /v1/teams/${T1}/grants/${P1}`]: ok(stored) } });
    const billing = await card("Billing app");
    expect(screen.queryByText(/Narrow on a database/)).toBeNull();
    // Developer with billing_reader; give billing_editor too, then move to Admin: the PUT carries the team's own grant only.
    await within(billing).findByRole("checkbox", { name: "billing_reader" });
    expect(within(billing).getByRole("checkbox", { name: "billing_reader" })).toBeChecked();
    await m.user.click(within(billing).getByRole("checkbox", { name: "billing_editor" }));
    await m.user.click(within(billing).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(m.f, "PUT", `/v1/teams/${T1}/grants/${P1}`)).toHaveLength(1));
    expect(await body(calls(m.f, "PUT", `/v1/teams/${T1}/grants/${P1}`)[0])).toEqual({ platform_role: "developer", pg_roles: ["billing_reader", "billing_editor"] });

    await m.user.click(within(billing).getByRole("radio", { name: "Admin" }));
    await m.user.click(within(billing).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(m.f, "PUT", `/v1/teams/${T1}/grants/${P1}`)).toHaveLength(2));
    // An Admin grant names no roles (R17).
    expect(await body(calls(m.f, "PUT", `/v1/teams/${T1}/grants/${P1}`)[1])).toEqual({ platform_role: "admin", pg_roles: [] });
  });

  it("taking_access_away_asks_then_deletes_the_grant", async () => {
    const m = mountPage(T1, { routes: { [`DELETE /v1/teams/${T1}/grants/${P1}`]: () => new Response(null, { status: 204 }) } });
    const billing = await card("Billing app");
    await m.user.click(await within(billing).findByRole("button", { name: "Take access away" }));
    expect(calls(m.f, "DELETE", `/v1/teams/${T1}/grants/${P1}`)).toHaveLength(0);
    await m.user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Take access away" }));
    await waitFor(() => expect(calls(m.f, "DELETE", `/v1/teams/${T1}/grants/${P1}`)).toHaveLength(1));
  });

  it("a_refused_grant_is_said_on_its_card", async () => {
    const m = mountPage(T1, { routes: { [`PUT /v1/teams/${T1}/grants/${P1}`]: apiError(422, "access_refused", "Sofia would hold a login role") } });
    const billing = await card("Billing app");
    await within(billing).findByRole("checkbox", { name: "billing_reader" });
    await m.user.click(within(billing).getByRole("checkbox", { name: "billing_editor" }));
    await m.user.click(within(billing).getByRole("button", { name: "Save" }));
    expect(await within(billing).findByText("Sofia would hold a login role")).toBeInTheDocument();
  });

  it("pg_roles_null_shows_the_platform_role_only", async () => {
    const hidden = team(T1, "Billing", { ...BILLING, projects: [{ project_id: P1, platform_role: "developer", pg_roles: null }] });
    mountPage(T1, { caller: "developer", teams: [hidden] });
    const billing = await card("Billing app");
    expect(within(billing).getByText("Developer")).toBeInTheDocument();
    expect(within(billing).queryByText(/Postgres roles/)).toBeNull();
    // Read-only: no editor at all.
    expect(within(billing).queryByRole("button")).toBeNull();
  });

  it("a_saved_grant_with_admin_only_roles_names_them_and_saves_once_they_are_unticked", async () => {
    const stored = team(T1, "Billing", {
      ...BILLING,
      projects: [{ project_id: P1, platform_role: "developer", pg_roles: ["billing_reader", "billing_admin", "ops_chain"] }],
    });
    const m = mountPage(T1, {
      routes: {
        "GET /v1/databases/d0000000-0000-4000-8000-000000000001/branches/b0000000-0000-4000-8000-000000000001/roles": ok([
          role("billing_reader"),
          role("billing_admin", { level: "owner_level" }),
          role("ops_chain", { level: "owner_level" }),
        ]),
        [`PUT /v1/teams/${T1}/grants/${P1}`]: ok(stored),
      },
      teams: [stored],
    });
    const billing = await card("Billing app");
    const note = await within(billing).findByText(/Admin-only\. Untick them/);
    expect(note).toHaveTextContent("billing_admin and ops_chain are Admin-only. Untick them, or set the level to Admin.");
    expect(within(billing).getByRole("checkbox", { name: "billing_admin" }).closest("li")).toHaveAttribute("data-invalid", "true");
    expect(within(billing).getByRole("checkbox", { name: "billing_reader" }).closest("li")).not.toHaveAttribute("data-invalid");
    // Nothing changed yet: no Save.
    expect(within(billing).queryByRole("button", { name: "Save" })).toBeNull();

    await m.user.click(within(billing).getByRole("checkbox", { name: "billing_admin" }));
    expect(within(billing).getByRole("button", { name: "Save" })).toBeDisabled();
    expect(note).toHaveTextContent("ops_chain is Admin-only. Untick it, or set the level to Admin.");
    await m.user.click(within(billing).getByRole("checkbox", { name: "ops_chain" }));
    expect(within(billing).queryByText(/Admin-only\./)).toBeNull();
    await m.user.click(within(billing).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(m.f, "PUT", `/v1/teams/${T1}/grants/${P1}`)).toHaveLength(1));
    expect(await body(calls(m.f, "PUT", `/v1/teams/${T1}/grants/${P1}`)[0])).toEqual({ platform_role: "developer", pg_roles: ["billing_reader"] });
  });

  it("role_names_render_escaped_in_the_cards", async () => {
    const bidi = "refunds‮review";
    mountPage(T1, {
      routes: { "GET /v1/databases/d0000000-0000-4000-8000-000000000001/branches/b0000000-0000-4000-8000-000000000001/roles": ok([role(bidi)]) },
      teams: [team(T1, "Billing", { ...BILLING, projects: [{ project_id: P1, platform_role: "developer", pg_roles: [bidi] }] })],
    });
    const billing = await card("Billing app");
    expect(await within(billing).findByRole("checkbox", { name: "refunds\\u202ereview" })).toBeChecked();
  });
});

describe("Team page: header, delete, back link", () => {
  it("only_tenant_admins_see_create_edit_and_delete", async () => {
    const admin = mountPage(T1);
    expect(await screen.findByRole("button", { name: "Edit" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Delete team" })).toBeInTheDocument();
    admin.unmount();
    mountPage(T1, { caller: "developer" });
    await screen.findByRole("heading", { name: "Billing" });
    expect(screen.queryByRole("button", { name: "Edit" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Delete team" })).toBeNull();
  });

  it("delete_confirm_names_how_many_members_get_access_through_the_team", async () => {
    const m = mountPage(T1, { routes: { [`DELETE /v1/teams/${T1}`]: () => new Response(null, { status: 204 }) } });
    await m.user.click(await screen.findByRole("button", { name: "Delete team" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(dialog).toHaveTextContent("2 members get access to 2 projects through Billing");
    await m.user.click(within(dialog).getByRole("button", { name: "Delete team" }));
    await waitFor(() => expect(calls(m.f, "DELETE", `/v1/teams/${T1}`)).toHaveLength(1));
    expect(await screen.findByText("members list route")).toBeInTheDocument();
    expect(m.router.state.location.search).toMatchObject({ tab: "teams" });
  });

  it("back_link_returns_to_the_teams_tab_with_its_view_and_filter", async () => {
    mountPage(T1, { initial: `/app/members/teams/${T1}?view=cards&q=bill` });
    const back = await screen.findByRole("link", { name: "Teams" });
    const url = new URL(back.getAttribute("href")!, "http://x");
    expect(url.pathname).toBe("/app/members");
    expect(Object.fromEntries(url.searchParams)).toEqual({ tab: "teams", view: "cards", q: "bill" });
  });

  it("an_unknown_team_shows_not_found", async () => {
    const unknown = "a1000000-0000-4000-8000-0000000000ff";
    mountPage(unknown, { routes: { [`GET /v1/teams/${unknown}`]: apiError(404, "not_found", "no such team") } });
    expect(await screen.findByRole("heading", { name: "Team not found" })).toBeInTheDocument();
  });

  it("an_id_that_is_not_a_uuid_is_not_found_and_asks_for_nothing", async () => {
    const m = mountPage("not-a-uuid");
    expect(await screen.findByRole("heading", { name: "Team not found" })).toBeInTheDocument();
    expect(m.f.calls.filter((r) => new URL(r.url).pathname.startsWith("/v1/teams/"))).toHaveLength(0);
  });

  it("team_names_and_roles_render_escaped", async () => {
    const hostile = { ...BILLING, name: "<img src=x onerror=alert(1)>", description: "<script>x</script>" };
    mountPage(T1, { teams: [hostile] });
    expect(await screen.findByRole("heading", { name: "<img src=x onerror=alert(1)>" })).toBeInTheDocument();
    expect(document.querySelector("img, script")).toBeNull();
  });

  it("a_name_taken_answer_is_said_on_the_name_field", async () => {
    const m = mountPage(T1, { routes: { [`PATCH /v1/teams/${T1}`]: apiError(409, "name_taken", "taken") } });
    await m.user.click(await screen.findByRole("button", { name: "Edit" }));
    await m.user.clear(screen.getByLabelText("Name"));
    await m.user.type(screen.getByLabelText("Name"), "Support");
    await m.user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("A team with that name already exists.")).toBeInTheDocument();
  });
});
