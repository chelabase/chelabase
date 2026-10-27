import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ProjectTeamGrantView } from "@chelabase/management-client";
import { jsonResponse } from "@/test/fake-fetch";
import { ANA, DEV, MARA, P1, SOFIA, calls, mountPage } from "./test-setup";
import { holdersByMember, teamHoldersOf } from "./model";

const TEAM_GRANTS_URL = `/v1/projects/${P1}/team-grants`;
const DB = "d0000000-0000-4000-8000-000000000001";
const support: ProjectTeamGrantView = { team_id: "t-support", name: "Support", platform_role: "developer", pg_roles: ["billing_editor"], member_ids: [DEV, ANA] };
const admins: ProjectTeamGrantView = { team_id: "t-admins", name: "Leads", platform_role: "admin", pg_roles: [], member_ids: [MARA] };

describe("role page Members: via team", () => {
  it("role_page_members_show_via_team_for_admins", async () => {
    const m = mountPage({ routes: { [`GET ${TEAM_GRANTS_URL}`]: () => jsonResponse([support, admins]) } });
    await screen.findByRole("table", { name: "Tables and views in billing" });
    expect(await screen.findByRole("tab", { name: /^Members 3/ })).toBeInTheDocument();
    await m.user.click(screen.getByRole("tab", { name: /^Members\b/ }));
    const panel = await screen.findByRole("tabpanel");
    const dev = within(panel).getByRole("link", { name: "Dev Dunn" }).closest("li")!;
    expect(dev).toHaveTextContent("via the Support team");
    // A team with Admin holds every role: it isn't listed as a holder.
    expect(within(panel).queryByRole("link", { name: "Mara Okafor" })).toBeNull();
  });

  it("a_member_with_own_and_team_sources_is_one_row_naming_both", async () => {
    const both: ProjectTeamGrantView = { ...support, member_ids: [SOFIA] };
    const m = mountPage({ routes: { [`GET ${TEAM_GRANTS_URL}`]: () => jsonResponse([both]) } });
    await screen.findByRole("table", { name: "Tables and views in billing" });
    // Sofia, Ana: two members, though Sofia has two sources.
    expect(await screen.findByRole("tab", { name: /^Members 2/ })).toBeInTheDocument();
    await m.user.click(screen.getByRole("tab", { name: /^Members\b/ }));
    const panel = await screen.findByRole("tabpanel");
    const rows = within(panel).getAllByRole("link", { name: "Sofia Marchetti" });
    expect(rows).toHaveLength(1);
    expect(rows[0].closest("li")).toHaveTextContent(/granted directly.*via the Support team/);
  });

  it("a_developer_never_asks_for_the_team_grants", async () => {
    const m = mountPage({ caller: "developer", routes: { [`GET ${TEAM_GRANTS_URL}`]: () => jsonResponse([support]) } });
    await screen.findByRole("table", { name: "Tables and views in billing" });
    expect(calls(m.f, "GET", TEAM_GRANTS_URL)).toHaveLength(0);
    await waitFor(() => expect(screen.queryByRole("tab", { name: /^Members\b/ })).toBeNull());
  });
});

describe("teamHoldersOf", () => {
  const rows = [] as never[];
  it("lists_each_member_of_a_team_that_names_the_role", () => {
    expect(teamHoldersOf("billing_editor", [support], rows, [], DB)).toEqual([
      { memberId: DEV, via: null, team: "Support", teamId: "t-support" },
      { memberId: ANA, via: null, team: "Support", teamId: "t-support" },
    ]);
  });
  it("skips_admin_teams_and_members_who_are_admin_on_their_own", () => {
    const own = [{ member_id: ANA, platform_role: "admin", pg_roles: "all", overrides: [] }] as never[];
    expect(teamHoldersOf("billing_editor", [admins, support], rows, own, DB).map((h) => h.memberId)).toEqual([DEV]);
  });
  it("lists_nothing_for_a_role_no_team_gives", () => {
    expect(teamHoldersOf("other", [support], rows, [], DB)).toEqual([]);
  });
  it("a_members_override_on_the_database_narrows_what_a_team_gives", () => {
    const ov = (database_id: string, pg_roles: string[] | null) => [{ member_id: DEV, platform_role: "developer", pg_roles: [], overrides: [{ database_id, pg_roles, platform_role: null }] }] as never[];
    // Dev's override on this database drops billing_editor: not a holder; Ana (no override) still is.
    expect(teamHoldersOf("billing_editor", [support], rows, ov(DB, ["billing_reader"]), DB).map((h) => h.memberId)).toEqual([ANA]);
    // An empty list takes every role away; a list that keeps it, or a null list, leaves Dev in.
    expect(teamHoldersOf("billing_editor", [support], rows, ov(DB, []), DB).map((h) => h.memberId)).toEqual([ANA]);
    expect(teamHoldersOf("billing_editor", [support], rows, ov(DB, ["billing_editor"]), DB).map((h) => h.memberId)).toEqual([DEV, ANA]);
    expect(teamHoldersOf("billing_editor", [support], rows, ov(DB, null), DB).map((h) => h.memberId)).toEqual([DEV, ANA]);
    // An override on another database doesn't matter here.
    expect(teamHoldersOf("billing_editor", [support], rows, ov("d-other", []), DB).map((h) => h.memberId)).toEqual([DEV, ANA]);
  });
  it("a_member_who_is_admin_through_any_team_is_not_listed_for_another_team", () => {
    const lead: ProjectTeamGrantView = { ...admins, member_ids: [DEV] };
    expect(teamHoldersOf("billing_editor", [lead, support], rows, [], DB).map((h) => h.memberId)).toEqual([ANA]);
    expect(teamHoldersOf("billing_editor", [support, lead], rows, [], DB).map((h) => h.memberId)).toEqual([ANA]);
  });
});

describe("holdersByMember", () => {
  it("lists_a_member_with_own_and_team_sources_once", () => {
    const rowsOf = holdersByMember([
      { memberId: DEV, via: null },
      { memberId: ANA, via: null, team: "Support", teamId: "t-support" },
      { memberId: DEV, via: null, team: "Support", teamId: "t-support" },
    ]);
    expect(rowsOf.map((r) => r.memberId)).toEqual([DEV, ANA]);
    expect(rowsOf[0].sources).toHaveLength(2);
  });
});
