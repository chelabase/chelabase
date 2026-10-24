import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import type { Member, Project } from "@chelabase/management-client";
import { initials, memberName } from "@/lib/members";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { expectSearchIcon, expectThemedSelects } from "@/test/themed";
import { Route as MembersRoute } from "@/routes/app.members";
import { PeopleTab } from "../people-tab";
import { P1, P2, person } from "./fixtures";

/** Fix S, finish-review fix 4 (roster list) and the shared `initials()` of fix 5. */

const projects = [
  { id: P1, slug: "shop", name: "Shop", status: "active", databases: [] },
  { id: P2, slug: "ops", name: "Ops", status: "active", databases: [] },
] as unknown as Project[];

const roster: Member[] = [
  person("ana", { name: "Ana Maria Lopez", level: "tenant_admin", projects: [[P1, "admin"], [P2, "admin"]] }),
  person("pri", { name: "Priya", level: "admin", projects: [[P1, "admin"]] }),
  { ...person("dev1", { name: "Dev One", level: "developer", projects: [[P1, "developer"], [P2, "viewer"]] }), accepted_at: "2026-03-04T10:00:00Z" },
  person("ivy", { name: "Ivy", level: "developer", status: "invited", projects: [[P1, "developer"]] }),
  person("dot.ted", { name: "", level: "none" }),
];

function setup(search = "") {
  const f = fakeFetch({ "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }) });
  const root = createRootRoute();
  const members = createRoute({
    getParentRoute: () => root,
    path: "/app/members",
    validateSearch: (s: Record<string, unknown>) => (MembersRoute.options.validateSearch as (x: Record<string, unknown>) => Record<string, unknown>)(s),
    component: () => <PeopleTab members={roster} selfId="ana" tenantAdmin projects={projects} onStatus={() => {}} />,
  });
  const member = createRoute({ getParentRoute: () => root, path: "/app/members/$memberId", component: () => <p>member page</p> });
  const router = createRouter({ routeTree: root.addChildren([members, member]), history: createMemoryHistory({ initialEntries: [`/app/members${search}`] }) });
  return renderWithQuery(<RouterProvider router={router} />, { fetch: f });
}

const row = (name: RegExp) => screen.getByRole("row", { name });

beforeEach(() => localStorage.clear());

describe("roster list (fix 4)", () => {
  it("the_project_count_is_bare_in_the_list", async () => {
    setup("?view=list");
    await screen.findByText("5 people");
    const dev = row(/Dev One/);
    expect(within(dev).getByText("2")).toBeInTheDocument();
    expect(within(dev).queryByText(/projects?$/)).toBeNull();
    expect(within(row(/Priya/)).getByText("1")).toBeInTheDocument();
  });

  it("joined_reads_d_MMM_yyyy_from_accepted_at_else_created_at", async () => {
    setup("?view=list");
    await screen.findByText("5 people");
    expect(within(row(/Dev One/)).getByText("4 Mar 2026")).toBeInTheDocument();
    expect(within(row(/Priya/)).getByText("1 Oct 2026")).toBeInTheDocument();
  });

  it("an_invited_person_has_a_dash_and_no_level_tag_in_the_list_and_in_cards", async () => {
    setup("?view=list");
    await screen.findByText("5 people");
    const ivy = row(/Ivy/);
    expect(within(ivy).queryByText("Developer")).toBeNull();
    expect(within(ivy).getAllByText("—").length).toBeGreaterThanOrEqual(1);
  });

  it("an_invited_person_has_no_level_tag_in_cards_either", async () => {
    setup("?view=cards");
    await screen.findByText("5 people");
    expect(within(screen.getByRole("link", { name: /^Ivy/ })).queryByText("Developer")).toBeNull();
  });

  it("every_list_row_has_an_initials_square_that_matches_the_shared_initials", async () => {
    setup("?view=list");
    await screen.findByText("5 people");
    for (const [pattern, m] of [[/Ana Maria Lopez/, roster[0]], [/Priya/, roster[1]], [/Dev One/, roster[2]], [/Ivy/, roster[3]], [/dot\.ted@/, roster[4]]] as const) {
      // The square is the row's first cell and its text is the shared initials of the member's name.
      expect(row(pattern).textContent?.startsWith(initials(memberName(m))), memberName(m)).toBe(true);
    }
    // Three words: the first and the last, as on the member page.
    expect(within(row(/Ana Maria Lopez/)).getByText("AL")).toBeInTheDocument();
  });

  it("the_level_badge_does_not_wrap", async () => {
    setup("?view=list");
    await screen.findByText("5 people");
    expect(within(row(/Priya/)).getByText("Project admin").className).toContain("whitespace-nowrap");
  });

  it("with_a_project_filter_tenant_admins_are_marked_in_the_role_column", async () => {
    setup(`?view=list&project=${P1}`);
    await screen.findByText(/people · \d+ on Shop/);
    expect(within(row(/Ana Maria Lopez/)).getByText(/Tenant admin/)).toBeInTheDocument();
    expect(within(row(/Priya/)).queryByText(/Tenant admin/)).toBeNull();
    expect(within(row(/Priya/)).getByText("Admin")).toBeInTheDocument();
  });

  it("the_toolbar_has_a_filter_magnifier_a_plus_on_invite_people_and_a_themed_project_select", async () => {
    const { container } = setup("?view=list");
    await screen.findByText("5 people");
    const filter = screen.getByRole("searchbox", { name: "Filter by name or email" });
    expectSearchIcon(filter);
    expect(screen.getByRole("button", { name: "Invite people" }).querySelector("svg")).not.toBeNull();
    expectThemedSelects(container);
  });
});

describe("shared initials on the card (fix 5)", () => {
  it("the_card_square_uses_the_same_initials_as_the_member_page", async () => {
    setup("?view=cards");
    await screen.findByText("5 people");
    expect(within(screen.getByRole("link", { name: /^Ana Maria Lopez/ })).getByText("AL")).toBeInTheDocument();
    // An email-derived name goes through the same function.
    expect(within(screen.getByRole("link", { name: /dot\.ted@example\.test/ })).getByText(initials(memberName(roster[4])))).toBeInTheDocument();
  });
});
