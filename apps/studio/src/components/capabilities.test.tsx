import { act, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { MeResponse as Me, PlatformRole, ProjectInfo } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, sequence } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { ProjectPage } from "./project-page";

const database = { database_id: "d1", name: "web1", postgres_database: "web1", status: "ready", host: "127.0.0.1", port: 49153, branch_count: 1 } as const;

function meAs(platform_role: PlatformRole, roles: string[] = []): Me {
  return {
    sub: "u2",
    email: "bo@example.test",
    name: "Bo",
    tenant_id: "t1",
    member: { id: "m-self", status: "active", tenant_admin: false },
    instance_admin: false, tenant_admins: [],
    projects: [{ project_id: "p1", platform_role, roles, databases: [] }],
    user: { sub: "u2", org_id: "org-1" },
    tenant: {
      id: "t1",
      slug: "demo",
      display_name: null,
      projects: [{ id: "p1", slug: "default", name: "Default", status: "active", databases: [database] }],
    },
  };
}
const info: ProjectInfo = {
  id: "p1", slug: "default", name: "Default", status: "active", error: null, app_users: null,
  created_at: "2026-10-03T10:00:00Z", updated_at: "2026-10-03T10:00:00Z",
};

function renderProject(me: Me) {
  const f = fakeFetch({ "GET /v1/me": () => jsonResponse(me), "GET /v1/projects/p1": () => jsonResponse(info) });
  return renderWithRouter(
    <ProjectPage projectId="p1" apiUrl="http://gw" tab="databases" onTabChange={() => {}} onGone={() => {}} />,
    { fetch: f },
  );
}

async function databaseRow() {
  const cell = await screen.findByRole("cell", { name: "web1" });
  return cell.closest("tr") as HTMLElement;
}

describe("capabilities", () => {
  it("viewer_has_no_sql_editor", async () => {
    renderProject(meAs("viewer"));
    const row = await databaseRow();
    expect(within(row).getByRole("link", { name: /^Open/ })).toBeInTheDocument();
    expect(within(row).queryByRole("button", { name: "SQL" })).toBeNull();
  });

  it("owner_actions_admin_only", async () => {
    const dev = renderProject(meAs("developer"));
    let row = await databaseRow();
    expect(within(row).queryByRole("button", { name: "Connection details" })).toBeNull();
    expect(within(row).queryByRole("button", { name: "Reset password" })).toBeNull();
    expect(within(row).queryByRole("button", { name: "Delete" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Create database" })).toBeNull();
    dev.unmount();

    renderProject({ ...meAs("admin"), projects: [{ project_id: "p1", platform_role: "admin", roles: [], databases: [] }] });
    row = await databaseRow();
    expect(within(row).getByRole("button", { name: "Connection details" })).toBeInTheDocument();
    expect(within(row).getByRole("button", { name: "Reset password" })).toBeInTheDocument();
    expect(within(row).getByRole("button", { name: "Delete" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create database" })).toBeInTheDocument();
  });

  it("a project URL without access shows the no-access screen and checks again every 30 seconds", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const none: Me = { ...meAs("viewer"), projects: [] };
      const f = fakeFetch({
        "GET /v1/me": sequence(() => jsonResponse(none), () => jsonResponse(meAs("viewer"))),
        "GET /v1/projects/p1": () => jsonResponse(info),
        "GET /v1/members": () => jsonResponse([]),
      });
      renderWithRouter(
        <ProjectPage projectId="p1" apiUrl="http://gw" tab="databases" onTabChange={() => {}} onGone={() => {}} />,
        { fetch: f },
      );
      expect(await screen.findByRole("heading", { name: "You don't have access to a project yet" })).toBeInTheDocument();
      await act(() => vi.advanceTimersByTimeAsync(30_000));
      expect(await screen.findByRole("cell", { name: "web1" })).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });
});
