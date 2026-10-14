import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { MeResponse as Me, PlatformRole, ProjectInfo } from "@chelabase/management-client";
import { ProjectCapsContext, projectCaps } from "@/lib/capabilities";
import { fakeFetch, jsonResponse, sequence, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { renderWithRouter } from "@/test/render-router";
import { ProjectPage } from "./project-page";
import { SqlEditor } from "./sql-editor/sql-editor";

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
    expect(within(row).getByRole("link", { name: "Tables" })).toBeInTheDocument();
    expect(within(row).queryByRole("button", { name: "SQL" })).toBeNull();
  });

  it("a developer has the SQL editor", async () => {
    renderProject(meAs("developer"));
    const row = await databaseRow();
    expect(within(row).getByRole("button", { name: "SQL" })).toBeInTheDocument();
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

  it("the SQL editor's Run as lists only allowed roles and schema reads carry ?role=", async () => {
    const B = "/v1/databases/d1/branches";
    const BR = `${B}/b1`;
    const role = (name: string, extra: Record<string, unknown> = {}) => ({
      name, level: "plain", login: false, origin: "sql", password: "none", pending: false, blocked_keys: [], member_of: [],
      created_at: "2026-10-01T00:00:00Z", ...extra,
    });
    const routes: Record<string, FakeHandler> = {
      "GET /v1/me": () => jsonResponse(meAs("developer", ["billing_admin"])),
      [`GET ${B}`]: () => jsonResponse([{ id: "b1", name: "main", kind: "normal", status: "ready", parent_branch_id: null, created_at: "2026-10-01T00:00:00Z", protected: false }]),
      [`GET ${BR}/databases`]: () => jsonResponse([{ name: "app", main: true, data_api: "off", data_api_error: null, default_schema: "public", origin: "main", owner: "o" }]),
      [`GET ${BR}/roles`]: () =>
        jsonResponse([
          role("app_owner", { origin: "owner" }),
          role("billing_admin", { member_of: ["billing"] }),
          role("billing"),
          role("reporter"),
        ]),
      [`GET ${BR}/sql/schema`]: () => jsonResponse({ schemas: [] }),
      "GET /v1/databases/d1/sql-queries": () => jsonResponse([]),
    };
    const f = fakeFetch(routes);
    const me = meAs("developer", ["billing_admin"]);
    renderWithQuery(
      <ProjectCapsContext.Provider value={projectCaps(me, "p1")}>
        <SqlEditor databaseId="d1" createClient={() => Promise.reject(new Error("no worker"))} />
      </ProjectCapsContext.Provider>,
      { fetch: f },
    );
    const runAs = await screen.findByRole("combobox", { name: "Run as" });
    await waitFor(() =>
      expect(within(runAs).getAllByRole("option").map((o) => o.textContent)).toEqual(["billing_admin", "billing"]),
    );
    await waitFor(() => {
      const schema = f.calls.filter((r) => r.url.includes("/sql/schema")).at(-1);
      expect(schema && new URL(schema.url).searchParams.get("role")).toBe("billing_admin");
    });
    await userEvent.selectOptions(runAs, "billing");
    await waitFor(() => {
      const schema = f.calls.filter((r) => r.url.includes("/sql/schema")).at(-1)!;
      expect(new URL(schema.url).searchParams.get("role")).toBe("billing");
    });
    expect(screen.queryByText(/Running as owner/)).toBeNull();
  });
});
