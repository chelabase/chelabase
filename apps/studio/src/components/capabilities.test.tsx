import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
    // The database row has no Connection details; an Admin's owner connection is in its Connect panel.
    expect(within(row).queryByRole("button", { name: "Connection details" })).toBeNull();
    expect(within(row).getByRole("button", { name: "Reset password" })).toBeInTheDocument();
    expect(within(row).getByRole("button", { name: "Delete" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create database" })).toBeInTheDocument();
  });

  it("owner_connection_in_connect_admin_only", async () => {
    const connectionRoutes = {
      "GET /v1/databases/d1/branches": () => jsonResponse([{ id: "m", name: "main", parent_branch_id: null, kind: "normal", status: "ready", host: "h", port: 1 }]),
      "GET /v1/databases/d1/branches/m/databases": () => jsonResponse([{ name: "web1", main: true, origin: "main", owner: "o", data_api: "off", data_api_error: null, default_schema: "public" }]),
      "GET /v1/databases/d1/branches/m/connection": () =>
        jsonResponse({ host: "h", port: 4432, database: "web1", user: "o", password: "pw", uri: "u", http_url: "x", password_state: "generated", ca_file: null }),
    };
    const open = async (role: PlatformRole) => {
      const f = fakeFetch({ "GET /v1/me": () => jsonResponse(meAs(role)), "GET /v1/projects/p1": () => jsonResponse(info), ...connectionRoutes });
      const r = renderWithRouter(
        <ProjectPage projectId="p1" apiUrl="http://gw" tab="databases" onTabChange={() => {}} onGone={() => {}} />,
        { fetch: f },
      );
      await userEvent.click(await screen.findByRole("button", { name: "Connect" }));
      await screen.findByLabelText("Branch");
      return { f, r };
    };
    const dev = await open("developer");
    expect(screen.queryByRole("button", { name: "Show password" })).toBeNull();
    expect(dev.f.calls.some((q) => q.url.includes("/connection"))).toBe(false);
    dev.r.unmount();
    await open("admin");
    expect(await screen.findByRole("button", { name: "Show password" })).toBeInTheDocument();
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
