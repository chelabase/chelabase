import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Member, Project } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { Route as MembersRoute } from "@/routes/app.members";
import { PeopleTab } from "../people-tab";
import { P1, P2, person } from "./fixtures";

const KEY = "chelabase.members.view.roster";

const projects = [
  { id: P1, slug: "shop", name: "Shop", status: "active", databases: [] },
  { id: P2, slug: "ops", name: "Ops", status: "active", databases: [] },
] as unknown as Project[];

const roster: Member[] = [
  person("ana", { name: "Ana", level: "tenant_admin", projects: [[P1, "admin"], [P2, "admin"]] }),
  person("pri", { name: "Priya", level: "admin", projects: [[P1, "admin"]] }),
  person("dev1", { name: "Dev One", level: "developer", projects: [[P1, "developer"], [P2, "viewer"]] }),
  person("dev2", { name: "Dev Two", level: "developer", projects: [[P2, "developer"]] }),
  person("vi", { name: "Vera", level: "viewer", projects: [[P1, "viewer"]] }),
  person("oth", { name: "Otto", level: "other" }),
  person("non", { name: "Nina", level: "none" }),
  person("inv", { name: "Ivan", level: "none", status: "invited" }),
  person("sus", { name: "Sue", level: "developer", status: "suspended", projects: [[P1, "developer"]] }),
  person("ed", { name: "Ed", level: "none", status: "pending" }),
];

const TITLES = [
  "Tenant admins", "Project admins", "Developers", "Viewers", "Other projects", "No access yet", "Invited", "Suspended",
  "Admins on", "Developers on", "Viewers on", "No access on",
];

function setup(opts: { search?: string; members?: Member[]; tenantAdmin?: boolean } = {}) {
  const f = fakeFetch({ "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }) });
  const root = createRootRoute();
  const members = createRoute({
    getParentRoute: () => root,
    path: "/app/members",
    validateSearch: (s: Record<string, unknown>) => (MembersRoute.options.validateSearch as (x: Record<string, unknown>) => Record<string, unknown>)(s),
    component: () => (
      <PeopleTab members={opts.members ?? roster} selfId="ana" tenantAdmin={opts.tenantAdmin ?? true} projects={projects} onStatus={() => {}} />
    ),
  });
  const member = createRoute({ getParentRoute: () => root, path: "/app/members/$memberId", component: () => <p>member page</p> });
  const router = createRouter({
    routeTree: root.addChildren([members, member]),
    history: createMemoryHistory({ initialEntries: [`/app/members${opts.search ?? ""}`] }),
  });
  return { ...renderWithQuery(<RouterProvider router={router} />, { fetch: f }), router, f };
}

/** The group headings on the page, in order, as "Title count". */
function headings() {
  return screen
    .getAllByRole("heading")
    .map((h) => h.textContent?.replace(/\s+/g, " ").trim() ?? "")
    .filter((t) => TITLES.some((x) => t.startsWith(x)));
}

beforeEach(() => localStorage.clear());
afterEach(() => vi.restoreAllMocks());

describe("People tab groups", () => {
  it("shows the MH-35 groups in order with counts, hides empty ones and keeps Requests apart", async () => {
    setup();
    await screen.findByText("9 people");
    expect(headings().map((h) => h.replace(/\s*\d+$/, ""))).toEqual([
      "Tenant admins", "Project admins", "Developers", "Viewers", "Other projects", "No access yet", "Invited", "Suspended",
    ]);
    expect(screen.getByRole("heading", { name: /^Developers/ })).toHaveTextContent(/Developers\s*2/);
    // The pending request is in Requests, in no group.
    expect(screen.getByRole("heading", { name: /^Requests/ })).toBeInTheDocument();
    expect(screen.getByText("Ed")).toBeInTheDocument();
    for (const t of ["Tenant admins", "Project admins", "Developers", "Viewers", "Other projects", "No access yet", "Invited", "Suspended"]) {
      expect(screen.getByRole("heading", { name: new RegExp(`^${t}`) }).closest("section, tbody, div")?.textContent).not.toContain("Ed");
    }
  });

  it("hides a group when nobody is in it", async () => {
    setup({ members: [person("ana", { name: "Ana", level: "tenant_admin" }), person("vi", { name: "Vera", level: "viewer", projects: [[P1, "viewer"]] })] });
    await screen.findByText("2 people");
    expect(headings().map((h) => h.replace(/\s*\d+$/, ""))).toEqual(["Tenant admins", "Viewers"]);
  });

  it("project_filter_regroups_by_that_projects_role", async () => {
    setup({ search: `?project=${P1}` });
    await screen.findByText("9 people · 4 on Shop");
    expect(headings().map((h) => h.replace(/\s*\d+$/, ""))).toEqual([
      "Admins on Shop", "Developers on Shop", "Viewers on Shop", "No access on Shop", "Invited", "Suspended",
    ]);
    expect(screen.getByRole("heading", { name: /^No access on Shop/ })).toHaveTextContent(/3$/);
    // The list's Level column becomes the role on that project.
    expect(screen.getByRole("columnheader", { name: "On Shop" })).toBeInTheDocument();
  });

  it("changing the project select regroups and writes the project into the URL", async () => {
    const { router } = setup();
    await screen.findByText("9 people");
    await userEvent.selectOptions(screen.getByLabelText("Project"), P2);
    await screen.findByRole("heading", { name: /^Admins on Ops/ });
    expect(router.state.location.search).toMatchObject({ project: P2 });
    await userEvent.selectOptions(screen.getByLabelText("Project"), "");
    await screen.findByRole("heading", { name: /^Tenant admins/ });
    expect(router.state.location.search).not.toHaveProperty("project");
  });

  it("applies the name filter within the groups and carries it in the URL", async () => {
    const { router } = setup();
    await screen.findByText("9 people");
    await userEvent.type(screen.getByRole("searchbox", { name: "Filter by name or email" }), "dev");
    await waitFor(() => expect(screen.queryByText("Priya")).toBeNull());
    expect(screen.getByText("Dev One")).toBeInTheDocument();
    expect(headings().map((h) => h.replace(/\s*\d+$/, ""))).toEqual(["Developers"]);
    await waitFor(() => expect(router.state.location.search).toMatchObject({ q: "dev" }));
  });

  it("a Developer sees the groups without Invite and with the levels note", async () => {
    setup({ tenantAdmin: false });
    await screen.findByText("9 people");
    expect(screen.queryByRole("button", { name: "Invite people" })).toBeNull();
    expect(screen.getByText(/Levels count only the projects you can see/)).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: /^Requests/ })).toBeNull();
  });

  it("shows names as text, with ordinary non-ASCII names intact", async () => {
    const { container } = setup({
      members: [
        person("ana", { name: "Ana", level: "tenant_admin" }),
        person("x", { name: "<img src=x onerror=alert(1)>", level: "viewer", projects: [[P1, "viewer"]] }),
        person("t", { name: "Tomás Ibarra", level: "viewer", projects: [[P1, "viewer"]] }),
      ],
    });
    await screen.findByText("3 people");
    expect(container.querySelector("img")).toBeNull();
    expect(screen.getByRole("heading", { name: /^Viewers/ })).toHaveTextContent(/Viewers\s*2/);
    expect(screen.getByText("<img src=x onerror=alert(1)>")).toBeInTheDocument();
    expect(screen.getByText("Tomás Ibarra")).toBeInTheDocument();
  });

  it("marks a suspended member with the Suspended word", async () => {
    setup();
    await screen.findByText("9 people");
    const row = screen.getByRole("row", { name: /Sue/ });
    expect(within(row).getByText("Suspended")).toBeInTheDocument();
  });
});

describe("People tab views", () => {
  it("view_toggle_persists_and_falls_back_to_list_when_storage_throws", async () => {
    // No stored choice: the list.
    const first = setup();
    await screen.findByText("9 people");
    expect(screen.getByRole("table", { name: "People" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("radio", { name: "Cards" }));
    expect(screen.queryByRole("table", { name: "People" })).toBeNull();
    expect(localStorage.getItem(KEY)).toBe("cards");
    first.unmount();

    // A new visit without a view in the URL starts from the stored choice.
    const second = setup();
    await screen.findByText("9 people");
    expect(screen.queryByRole("table", { name: "People" })).toBeNull();
    expect(screen.getByRole("radio", { name: "Cards" })).toBeChecked();
    second.unmount();

    // A stored value that isn't one of the two is ignored.
    localStorage.setItem(KEY, "grid");
    const third = setup();
    await screen.findByText("9 people");
    expect(screen.getByRole("table", { name: "People" })).toBeInTheDocument();
    third.unmount();

    // Storage that throws on read and write: the list, and the toggle still works.
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });
    setup();
    await screen.findByText("9 people");
    expect(screen.getByRole("table", { name: "People" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("radio", { name: "Cards" }));
    expect(screen.queryByRole("table", { name: "People" })).toBeNull();
    expect(screen.getByRole("link", { name: /^Dev One/ })).toBeInTheDocument();
  });

  it("lets the view in the URL win over the stored one", async () => {
    localStorage.setItem(KEY, "list");
    setup({ search: "?view=cards" });
    await screen.findByText("9 people");
    expect(screen.queryByRole("table", { name: "People" })).toBeNull();
  });

  it("shows cards with the level and the project count, each a link to the member's page", async () => {
    setup({ search: "?view=cards" });
    await screen.findByText("9 people");
    const card = screen.getByRole("link", { name: /^Dev One/ });
    expect(card).toHaveAttribute("href", expect.stringMatching(/^\/app\/members\/dev1(\?|$)/));
    expect(within(card).getByText("Developer")).toBeInTheDocument();
    expect(within(card).getByText("2 projects")).toBeInTheDocument();
    expect(within(screen.getByRole("link", { name: /^Priya/ })).getByText("1 project")).toBeInTheDocument();
    expect(within(screen.getByRole("link", { name: /^Ana/ })).getByText("Tenant admin")).toBeInTheDocument();
  });

  it("back_link_restores_tab_filter_and_view: member links carry project, view and q", async () => {
    setup({ search: `?tab=people&project=${P1}&view=cards&q=dev` });
    await screen.findByText(/people · \d+ on Shop/);
    const href = screen.getByRole("link", { name: /^Dev One/ }).getAttribute("href")!;
    const url = new URL(href, "http://studio.test");
    expect(url.pathname).toBe("/app/members/dev1");
    expect(url.searchParams.get("project")).toBe(P1);
    expect(url.searchParams.get("view")).toBe("cards");
    expect(url.searchParams.get("q")).toBe("dev");
  });

  it("links to the member in the list view as well", async () => {
    setup({ search: "?view=list" });
    await screen.findByText("9 people");
    const table = screen.getByRole("table", { name: "People" });
    expect(within(table).getByRole("link", { name: /^Dev One/ })).toHaveAttribute("href", expect.stringContaining("/app/members/dev1"));
  });
});
