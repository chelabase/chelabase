import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { screen } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import type { Member, Project } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { Route as MembersRoute } from "@/routes/app.members";
import { PeopleTab } from "../people-tab";
import { P1, person } from "./fixtures";

/** Fix S2 (recapture at 390 px): roster card emails break at `@` and dots, not mid-word. */

const projects = [{ id: P1, slug: "shop", name: "Shop", status: "active", databases: [] }] as unknown as Project[];
const roster: Member[] = [
  { ...person("ana", { name: "Ana Maria Lopez", level: "tenant_admin", projects: [[P1, "admin"]] }), email: "ana.maria.lopez@northwind-systems.example" },
];

beforeEach(() => localStorage.clear());

describe("roster card email", () => {
  it("has_break_opportunities_after_the_at_sign_and_each_dot", async () => {
    const f = fakeFetch({ "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }) });
    const root = createRootRoute();
    const members = createRoute({
      getParentRoute: () => root,
      path: "/app/members",
      validateSearch: (s: Record<string, unknown>) => (MembersRoute.options.validateSearch as (x: Record<string, unknown>) => Record<string, unknown>)(s),
      component: () => <PeopleTab members={roster} selfId="ana" tenantAdmin projects={projects} onStatus={() => {}} />,
    });
    const member = createRoute({ getParentRoute: () => root, path: "/app/members/$memberId", component: () => <p>member page</p> });
    const router = createRouter({ routeTree: root.addChildren([members, member]), history: createMemoryHistory({ initialEntries: ["/app/members?view=cards"] }) });
    renderWithQuery(<RouterProvider router={router} />, { fetch: f });
    const email = await screen.findByText((_, el) => el?.tagName === "SPAN" && el.textContent === "ana.maria.lopez@northwind-systems.example");
    // After "@" and after each of the three dots.
    expect(email.querySelectorAll("wbr").length).toBeGreaterThanOrEqual(4);
  });
});
