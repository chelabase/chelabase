import { screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithCrumbs } from "@/test/render-crumbs";
import { ProjectPage } from "./project-page";

const database = { database_id: "d1", name: "web1", postgres_database: "web1", status: "ready", host: null, port: null, branch_count: 0 };
const me = {
  sub: "u1", email: "u@example.test", name: "U", tenant_id: "t1", member: { id: "m", status: "active", tenant_admin: true },
  instance_admin: true, tenant_admins: [], user: { sub: "u1", org_id: "o" },
  projects: [{ project_id: "p1", platform_role: "admin", roles: "all", databases: [] }],
  tenant: { id: "t1", slug: "demo", display_name: null, projects: [{ id: "p1", slug: "x", name: "Acme / shop é", status: "active", databases: [database] }] },
};
const info = { id: "p1", slug: "x", name: "Acme / shop é", status: "active", error: null, app_users: null, created_at: "2026-10-03T10:00:00Z", updated_at: "2026-10-03T10:00:00Z" };

describe("ProjectPage crumb label", () => {
  it("shows the project's name, and the loading mark before it loads", async () => {
    const f = fakeFetch({
      "GET /v1/me": () => jsonResponse(me),
      "GET /v1/projects/p1": () => jsonResponse(info),
      "GET /v1/databases/d1/branches": () => jsonResponse([]),
    });
    renderWithCrumbs(
      <ProjectPage projectId="p1" apiUrl="http://gw" tab="databases" onTabChange={vi.fn()} onGone={vi.fn()} />,
      { path: "/app/projects/$projectId", at: "/app/projects/p1" },
      { fetch: f },
    );
    const trail = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(trail).toHaveTextContent("…");
    expect(await within(trail).findByText("Acme / shop é")).toHaveAttribute("aria-current", "page");
  });
});
