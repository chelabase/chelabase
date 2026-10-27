import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { BILLING, P1, SUPPORT, T1, ok, mountTab, team } from "./test-setup";

beforeEach(() => {
  localStorage.clear();
});

describe("Teams tab", () => {
  it("teams_tab_shows_cards_and_list_with_visible_project_counts", async () => {
    const m = mountTab();
    // The remembered default is the list: a row per team with its member count and project names.
    const table = await screen.findByRole("table", { name: "Teams" });
    const billing = within(table).getByRole("row", { name: /Billing/ });
    expect(within(billing).getByRole("link", { name: "Billing" })).toHaveAttribute("href", `/app/members/teams/${T1}`);
    expect(billing).toHaveTextContent("2");
    expect(billing).toHaveTextContent("Billing app, Data warehouse");
    expect(within(table).getByRole("row", { name: /Support/ })).toHaveTextContent("No projects");

    await m.user.click(screen.getByRole("radio", { name: "Cards" }));
    const card = screen.getByRole("link", { name: /Billing/ });
    expect(card).toHaveTextContent("2 members");
    expect(card).toHaveTextContent("2 projects");
    expect(screen.getByRole("link", { name: /Support/ })).toHaveTextContent("0 members");
    expect(localStorage.getItem("chelabase.members.view.teams")).toBe("cards");
  });

  it("counts_only_the_projects_the_server_lets_the_caller_see", async () => {
    // A developer is sent the grants on their own projects only; the card says so, never "2".
    const visible = team(T1, "Billing", { member_count: 2, projects: [{ project_id: P1, platform_role: "developer", pg_roles: null }] });
    const m = mountTab({ caller: "developer", teams: [visible] });
    await m.user.click(await screen.findByRole("radio", { name: "Cards" }));
    expect(screen.getByRole("link", { name: /Billing/ })).toHaveTextContent("1 project");
    expect(screen.queryByRole("link", { name: /2 projects/ })).toBeNull();
  });

  it("only_tenant_admins_see_create_edit_and_delete_on_the_tab", async () => {
    const admin = mountTab();
    expect(await screen.findByRole("button", { name: "Create team" })).toBeInTheDocument();
    admin.unmount();
    mountTab({ caller: "developer" });
    await screen.findByRole("table", { name: "Teams" });
    expect(screen.queryByRole("button", { name: "Create team" })).toBeNull();
    expect(screen.getByText(/Tenant admins create teams/)).toBeInTheDocument();
  });

  it("team_names_and_roles_render_escaped", async () => {
    const hostile = team(T1, "<img src=x onerror=alert(1)>", { description: "<b>bold</b>", member_count: 1 });
    mountTab({ teams: [hostile, SUPPORT] });
    const table = await screen.findByRole("table", { name: "Teams" });
    // Text, never markup.
    expect(within(table).getByText("<img src=x onerror=alert(1)>")).toBeInTheDocument();
    expect(table.querySelector("img, b")).toBeNull();
  });

  it("filters_by_name_and_says_when_nothing_matches", async () => {
    const m = mountTab({ teams: [BILLING, SUPPORT] });
    await screen.findByRole("table", { name: "Teams" });
    await m.user.type(screen.getByRole("searchbox", { name: "Filter teams" }), "supp");
    expect(screen.queryByRole("link", { name: "Billing" })).toBeNull();
    expect(screen.getByRole("link", { name: "Support" })).toBeInTheDocument();
    await m.user.clear(screen.getByRole("searchbox", { name: "Filter teams" }));
    await m.user.type(screen.getByRole("searchbox", { name: "Filter teams" }), "zzz");
    expect(screen.getByText(/No team matches/)).toBeInTheDocument();
  });

  it("creating_a_team_validates_the_name_then_posts_it", async () => {
    const m = mountTab({ routes: { "POST /v1/teams": ok(team(T1, "New team"), 201) } });
    await m.user.click(await screen.findByRole("button", { name: "Create team" }));
    await m.user.click(screen.getByRole("button", { name: "Create team", hidden: false }));
    expect(await screen.findByText("Give the team a name.")).toBeInTheDocument();
    expect(m.f.calls.some((r) => r.method === "POST")).toBe(false);
    await m.user.type(screen.getByLabelText("Name"), "  New team ");
    await m.user.click(screen.getByRole("button", { name: "Create team", hidden: false }));
    const post = m.f.calls.find((r) => r.method === "POST");
    expect(await post?.clone().json()).toEqual({ name: "New team", description: "" });
  });
});
