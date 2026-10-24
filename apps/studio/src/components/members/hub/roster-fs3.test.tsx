import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import type { Member, Project } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { Route as MembersRoute } from "@/routes/app.members";
import { PeopleTab } from "../people-tab";
import { P1, person } from "./fixtures";

/** Fix S3 (finish review round 2, item 8): the list view's emails wrap like the cards'. */

const projects = [{ id: P1, slug: "shop", name: "Shop", status: "active", databases: [] }] as unknown as Project[];
const EMAIL = "ana.maria.lopez@northwind-systems.example";
const roster: Member[] = [{ ...person("ana", { name: "Ana Maria Lopez", level: "tenant_admin", projects: [[P1, "admin"]] }), email: EMAIL }];

beforeEach(() => localStorage.clear());

describe("roster list email", () => {
  it("wraps_with_break_opportunities_instead_of_truncating", async () => {
    const f = fakeFetch({ "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }) });
    const root = createRootRoute();
    const members = createRoute({
      getParentRoute: () => root,
      path: "/app/members",
      validateSearch: (s: Record<string, unknown>) => (MembersRoute.options.validateSearch as (x: Record<string, unknown>) => Record<string, unknown>)(s),
      component: () => <PeopleTab members={roster} selfId="ana" tenantAdmin projects={projects} onStatus={() => {}} />,
    });
    const member = createRoute({ getParentRoute: () => root, path: "/app/members/$memberId", component: () => <p>member page</p> });
    const router = createRouter({ routeTree: root.addChildren([members, member]), history: createMemoryHistory({ initialEntries: ["/app/members?view=list"] }) });
    renderWithQuery(<RouterProvider router={router} />, { fetch: f });
    const table = await screen.findByRole("table", { name: "People" });
    const breaks = [...table.querySelectorAll("wbr")];
    // After "@" and each of the three dots, and nowhere inside a truncating box.
    expect(breaks.length).toBeGreaterThanOrEqual(4);
    for (const b of breaks) expect(b.closest(".truncate")).toBeNull();
    expect(table.textContent).toContain(EMAIL);
    expect(within(table).queryByText(EMAIL)?.closest(".truncate") ?? null).toBeNull();
  });
});
