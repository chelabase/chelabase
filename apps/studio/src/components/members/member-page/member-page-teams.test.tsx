import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Member, Team } from "@chelabase/management-client";
import { jsonResponse } from "@/test/fake-fetch";
import { MARA, P1, SELF_GRANTS, SOFIA, calls, mount, ok, sofiaFor } from "./test-setup";

const support = {
  id: "t-support",
  name: "Support",
  description: "",
  member_count: 3,
  projects: [{ project_id: P1, platform_role: "developer", pg_roles: ["billing_reader"] }],
  created_at: "",
  updated_at: "",
} as unknown as Team;
const billing = { ...support, id: "t-billing", name: "Billing", projects: [] } as unknown as Team;

/** Sofia holds billing_editor on her own and billing_reader through Support, on Billing app. */
const viaBoth = (): Member => {
  const m = sofiaFor("tenant_admin");
  return { ...m, access: { ...m.access, projects: m.access.projects.map((p) => (p.project_id === P1 ? { ...p, via: "both" as const } : p)) } };
};
const markerOf = (within_: HTMLElement, role: string) => within(within_).getByRole("checkbox", { name: role }).closest("li")!.querySelector<HTMLElement>("[data-team-marker]")!;
const card = (name: string) => screen.findByRole("region", { name });
const MEMBER_TEAMS = `/v1/members/${SOFIA}/teams`;

describe("MemberPage: via team", () => {
  it("a_team_role_is_an_annotation_and_the_own_box_stays_editable", async () => {
    const { user } = mount({ members: () => [viaBoth()], routes: { [`GET ${MEMBER_TEAMS}`]: ok([support]) } });
    const billingCard = await card("Billing app");
    // Support names billing_reader: not an own role, so the box is open and unticked, with the source beside it.
    const reader = await within(billingCard).findByRole("checkbox", { name: "billing_reader" });
    expect(reader).not.toBeChecked();
    expect(reader).toBeEnabled();
    expect(markerOf(billingCard, "billing_reader")).toHaveTextContent("✓ via the Support team");
    expect(within(billingCard).getByRole("link", { name: "Support" })).toHaveAttribute("href", "/app/members/teams/t-support");
    expect(within(billingCard).getByText("Own grant and Support team")).toBeInTheDocument();
    await user.click(reader);
    expect(reader).toBeChecked();
    expect(markerOf(billingCard, "billing_reader")).toHaveTextContent("also via the Support team");
  });

  it("an_own_role_a_team_also_names_stays_checked_editable_and_can_be_unticked", async () => {
    const both = { ...support, projects: [{ project_id: P1, platform_role: "developer", pg_roles: ["billing_editor"] }] } as unknown as Team;
    const { f, user } = mount({
      members: () => [viaBoth()],
      routes: { [`GET ${MEMBER_TEAMS}`]: ok([both]), [`PUT /v1/projects/${P1}/grants/${SOFIA}`]: ok({ ...SELF_GRANTS[P1], pg_roles: [] }) },
    });
    const billingCard = await card("Billing app");
    const editor = await within(billingCard).findByRole("checkbox", { name: "billing_editor" });
    expect(editor).toBeChecked();
    expect(editor).toBeEnabled();
    expect(markerOf(billingCard, "billing_editor")).toHaveTextContent("also via the Support team");
    await user.click(editor);
    expect(editor).not.toBeChecked();
    await user.click(within(billingCard).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(f, "PUT", "/v1/projects")).toHaveLength(1));
    const body = await calls(f, "PUT", "/v1/projects")[0].clone().json();
    expect(body.pg_roles).toEqual([]);
  });

  it("ticking_a_role_a_team_gives_adds_it_to_the_own_roles_exactly", async () => {
    const { f, user } = mount({
      members: () => [viaBoth()],
      routes: { [`GET ${MEMBER_TEAMS}`]: ok([support]), [`PUT /v1/projects/${P1}/grants/${SOFIA}`]: ok({ ...SELF_GRANTS[P1], pg_roles: ["billing_editor", "billing_reader"] }) },
    });
    const billingCard = await card("Billing app");
    await user.click(await within(billingCard).findByRole("checkbox", { name: "billing_reader" }));
    await user.click(within(billingCard).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(f, "PUT", "/v1/projects")).toHaveLength(1));
    const body = await calls(f, "PUT", "/v1/projects")[0].clone().json();
    expect([...body.pg_roles].sort()).toEqual(["billing_editor", "billing_reader"]);
  });

  it("a_team_only_project_is_read_only_until_own_access_is_added", async () => {
    const teamOnly = (): Member => {
      const m = sofiaFor("tenant_admin");
      return { ...m, access: { ...m.access, projects: m.access.projects.map((p) => (p.project_id === P1 ? { ...p, via: "team" as const } : p)) } };
    };
    const { f, user } = mount({
      members: () => [teamOnly()],
      grants: {},
      routes: { [`GET ${MEMBER_TEAMS}`]: ok([support]), [`PUT /v1/projects/${P1}/grants/${SOFIA}`]: ok({ ...SELF_GRANTS[P1], pg_roles: ["billing_reader"] }) },
    });
    const billingCard = await card("Billing app");
    const locked = await within(billingCard).findByRole("checkbox", { name: "billing_reader" });
    expect(locked).toBeChecked();
    expect(locked).toBeDisabled();
    expect(markerOf(billingCard, "billing_reader")).toHaveTextContent("via the Support team");
    expect(within(billingCard).queryByRole("button", { name: "Save" })).toBeNull();
    await user.click(within(billingCard).getByRole("button", { name: "Add own access" }));
    // Now an editable own grant: Save is there, the box is the own state (unticked), the source stays beside it.
    const reader = await within(billingCard).findByRole("checkbox", { name: "billing_reader" });
    expect(reader).toBeEnabled();
    expect(reader).not.toBeChecked();
    expect(within(billingCard).queryByRole("button", { name: "Add own access" })).toBeNull();
    await user.click(reader);
    await user.click(within(billingCard).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(f, "PUT", "/v1/projects")).toHaveLength(1));
    const body = await calls(f, "PUT", "/v1/projects")[0].clone().json();
    expect(body.pg_roles).toEqual(["billing_reader"]);
  });

  it("add_own_access_has_a_cancel_that_returns_to_the_read_only_card", async () => {
    const teamOnly = (): Member => {
      const m = sofiaFor("tenant_admin");
      return { ...m, access: { ...m.access, projects: m.access.projects.map((p) => (p.project_id === P1 ? { ...p, via: "team" as const } : p)) } };
    };
    const { user } = mount({ members: () => [teamOnly()], grants: {}, routes: { [`GET ${MEMBER_TEAMS}`]: ok([support]) } });
    const billingCard = await card("Billing app");
    await user.click(await within(billingCard).findByRole("button", { name: "Add own access" }));
    await user.click(within(billingCard).getByRole("button", { name: "Cancel" }));
    expect(await within(billingCard).findByRole("button", { name: "Add own access" })).toBeInTheDocument();
    expect(within(billingCard).getByRole("checkbox", { name: "billing_reader" })).toBeDisabled();
  });

  it("a_locked_team_role_is_never_sent_in_the_grant_put", async () => {
    const { f, user } = mount({
      members: () => [viaBoth()],
      routes: {
        [`GET ${MEMBER_TEAMS}`]: ok([support]),
        [`PUT /v1/projects/${P1}/grants/${SOFIA}`]: ok({ ...SELF_GRANTS[P1], pg_roles: [] }),
      },
    });
    const billingCard = await card("Billing app");
    await user.click(await within(billingCard).findByRole("checkbox", { name: "billing_editor" }));
    await user.click(within(billingCard).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(f, "PUT", "/v1/projects")).toHaveLength(1));
    const body = await calls(f, "PUT", "/v1/projects")[0].clone().json();
    expect(body.pg_roles).toEqual([]);
    expect(Object.keys(body).sort()).toEqual(["pg_roles", "platform_role"]);
  });

  it("effective_line_names_the_team", async () => {
    mount({ members: () => [viaBoth()], routes: { [`GET ${MEMBER_TEAMS}`]: ok([support]) } });
    const billingCard = await card("Billing app");
    await waitFor(() => expect(billingCard).toHaveTextContent(/\(\+ billing_reader via Support team\)/));
  });

  it("a_team_level_above_the_own_grant_is_said_under_the_platform_role", async () => {
    const admin = { ...support, projects: [{ project_id: P1, platform_role: "admin", pg_roles: [] }] } as unknown as Team;
    mount({ members: () => [viaBoth()], routes: { [`GET ${MEMBER_TEAMS}`]: ok([admin]) } });
    const billingCard = await card("Billing app");
    await waitFor(() => expect(billingCard).toHaveTextContent("Admin via the Support team"));
  });

  it("an_own_only_project_has_no_locked_rows_or_team_words", async () => {
    mount({ routes: { [`GET ${MEMBER_TEAMS}`]: ok([support]) } });
    const warehouse = await card("Data warehouse");
    expect(within(warehouse).queryByText(/via Support/)).toBeNull();
    expect(within(warehouse).queryByText(/Access through|Own grant and/)).toBeNull();
  });
});

describe("MemberPage: Teams section", () => {
  it("teams_section_adds_and_removes_for_tenant_admins_only", async () => {
    const put = `PUT /v1/teams/${billing.id}/members/${SOFIA}`;
    const del = `DELETE /v1/teams/${support.id}/members/${SOFIA}`;
    const { f, user } = mount({
      routes: {
        [`GET ${MEMBER_TEAMS}`]: ok([support]),
        "GET /v1/teams": ok([support, billing]),
        [put]: ok(billing),
        [del]: () => new Response(null, { status: 204 }),
      },
    });
    const section = await screen.findByRole("region", { name: "Teams" });
    const link = await within(section).findByRole("link", { name: "Support" });
    expect(link).toHaveAttribute("href", "/app/members/teams/t-support");
    // Add: only teams the member isn't in are offered.
    const select = within(section).getByRole("combobox", { name: "Team to add" });
    expect(within(select).queryByRole("option", { name: "Support" })).toBeNull();
    await user.selectOptions(select, "t-billing");
    await user.click(within(section).getByRole("button", { name: "Add to team" }));
    await waitFor(() => expect(calls(f, "PUT", "/v1/teams")).toHaveLength(1));
    // Remove asks first.
    await user.click(within(section).getByRole("button", { name: "Remove from Support" }));
    expect(calls(f, "DELETE", "/v1/teams")).toHaveLength(0);
    await user.click(await screen.findByRole("button", { name: "Remove from team" }));
    await waitFor(() => expect(calls(f, "DELETE", "/v1/teams")).toHaveLength(1));
  });

  it("a_project_admin_and_the_member_themselves_read_the_teams_without_editing", async () => {
    mount({ caller: "project_admin", routes: { [`GET ${MEMBER_TEAMS}`]: ok([support]), "GET /v1/teams": ok([support, billing]) } });
    const section = await screen.findByRole("region", { name: "Teams" });
    expect(await within(section).findByRole("link", { name: "Support" })).toBeInTheDocument();
    expect(within(section).queryByRole("button", { name: /Remove from|Add to team/ })).toBeNull();
    expect(within(section).queryByRole("combobox")).toBeNull();
  });

  it("says_so_when_the_member_is_in_no_team_and_the_caller_is_self", async () => {
    mount({ caller: "self", routes: { [`GET ${MEMBER_TEAMS}`]: () => jsonResponse([]) } });
    const section = await screen.findByRole("region", { name: "Teams" });
    expect(await within(section).findByText("Not in any team.")).toBeInTheDocument();
    expect(within(section).queryByRole("combobox")).toBeNull();
    void MARA;
  });
});
