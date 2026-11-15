import { useState } from "react";
import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { MeResponse as Me, ProjectInfo } from "@chelabase/management-client";
import { POLL_MS } from "@/lib/query";
import { fakeFetch, jsonResponse, sequence, type FakeHandler } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { ProjectPage, type ProjectPageProps, type ProjectTab } from "./project-page";

const database = { database_id: "d1", name: "web1", postgres_database: "web1", status: "ready", host: "127.0.0.1", port: 49153, branch_count: 1 } as const;
const me: Me = {
  sub: "u1",
  email: "u1@example.test",
  name: "U1",
  tenant_id: "t1",
  member: { id: "m-self", status: "active", tenant_admin: true },
  instance_admin: true, functions_available: true, tenant_admins: [],
  projects: [{ project_id: "p1", platform_role: "admin", roles: "all", databases: [] }],
  user: { sub: "u1" },
  tenant: {
    id: "t1",
    slug: "demo",
    display_name: null,
    projects: [{ id: "p1", slug: "default", name: "Default", status: "active", databases: [database] }],
  },
};
const info: ProjectInfo = {
  id: "p1", slug: "default", name: "Default", status: "active", error: null, app_users: null,
  created_at: "2026-10-03T10:00:00Z", updated_at: "2026-10-03T10:00:00Z",
};
const deleting: ProjectInfo = { ...info, status: "deleting", error: "database web1: delete failed" };
const ok = (body: unknown) => () => jsonResponse(body);
const notFound = () => jsonResponse({ error: { code: "not_found", message: "project not found" } }, 404);

const mainBranch = {
  id: "b-main", name: "main", parent_branch_id: null, kind: "normal", status: "ready", host: "ep-aaaaaaaaaaaa.localtest.me",
  port: 4432, branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null,
};
const databaseRoutes: Record<string, FakeHandler> = {
  "GET /v1/databases/d1": () =>
    jsonResponse({
      ...database, tenant_id: "t1", project_id: "p1", error: null, history_window: 86400,
      max_branches: 5, max_rows: 1000, statement_timeout_ms: 8000, max_connections: 5, default_schema: "public",
    }),
  "GET /v1/databases/d1/branches": () => jsonResponse([mainBranch]),
  "GET /v1/databases/d1/branches/b-main/keys": () => jsonResponse([]),
  "GET /v1/databases/d1/branches/b-main/data-api/schema": () => jsonResponse({ version: "1", default_schema: "public", schemas: {} }),
};

function setup(routes: Record<string, FakeHandler>, props: Partial<ProjectPageProps> = {}) {
  const f = fakeFetch({ "GET /v1/me": ok(me), "GET /v1/projects/p1": ok(info), ...databaseRoutes, ...routes });
  const onTabChange = vi.fn();
  const onGone = vi.fn();
  // The route owns the tab; this harness plays its part.
  function Harness() {
    const [tab, setTab] = useState<ProjectTab>("databases");
    return (
      <ProjectPage
        projectId="p1"
        apiUrl="http://gw"
        tab={tab}
        onTabChange={(next) => {
          onTabChange(next);
          setTab(next);
        }}
        onGone={onGone}
        {...props}
      />
    );
  }
  const result = renderWithRouter(<Harness />, { fetch: f });
  return { ...result, fetch: f, onTabChange, onGone };
}

afterEach(() => vi.useRealTimers());

describe("ProjectPage", () => {
  it("shows the header and the databases tab", async () => {
    setup({});
    expect(await screen.findByRole("heading", { name: "Default" })).toBeInTheDocument();
    expect(screen.getByText("default")).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "Databases" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByText("web1")).toBeInTheDocument();
    expect(screen.getByText("127.0.0.1:49153")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create database" })).toBeInTheDocument();
  });

  it("offers a ready database's row no Connection details button", async () => {
    setup({});
    await screen.findByText("web1");
    for (const name of ["Reset password", "Delete", "Branches", "Merges", "Connect"]) {
      expect(screen.getByRole("button", { name })).toBeInTheDocument();
    }
    expect(screen.getByRole("link", { name: "Open" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Connection details" })).toBeNull();
  });

  it("fs3_databases_table_scrolls_inside_a_relative_frame", async () => {
    setup({});
    const frame = (await screen.findByRole("table")).closest<HTMLElement>(".overflow-x-auto");
    expect(frame).not.toBeNull();
    expect(frame!.className).toMatch(/(^|\s)relative(\s|$)/);
  });

  it("create_form_shows_before_me_loads", async () => {
    // /v1/me never answers: the form needs no tenant, so it is there anyway.
    setup({ "GET /v1/me": () => new Promise<Response>(() => {}) });
    expect(await screen.findByRole("button", { name: "Create database" })).toBeInTheDocument();
  });

  it("users_tab_hidden_when_disabled", async () => {
    setup({});
    await screen.findByRole("heading", { name: "Default" });
    expect(screen.queryByRole("tab", { name: "Users" })).toBeNull();
  });

  it("users_tab_shown_when_enabled", async () => {
    const { onTabChange } = setup({
      "GET /v1/projects/p1": ok({ ...info, app_users: "enabled" }),
      "GET /v1/projects/p1/users": ok({ users: [], next_page_token: null }),
    });
    await userEvent.click(await screen.findByRole("tab", { name: "Users" }));
    expect(onTabChange).toHaveBeenCalledWith("users");
    expect(await screen.findByRole("button", { name: "Create user" })).toBeInTheDocument();
  });

  it("project_page_tabs_switch", async () => {
    const { onTabChange } = setup({});
    await screen.findByRole("heading", { name: "Default" });
    await userEvent.click(screen.getByRole("tab", { name: "Settings" }));
    expect(onTabChange).toHaveBeenCalledWith("settings");
    expect(await screen.findByRole("button", { name: "Delete project" })).toBeInTheDocument();
    expect(screen.getByText(/App users/)).toBeInTheDocument();
    expect(screen.queryByRole("cell", { name: "web1" })).toBeNull();
  });

  it("deleting_banner_disables_actions", async () => {
    setup({ "GET /v1/projects/p1": ok(deleting) });
    expect(await screen.findByRole("alert")).toHaveTextContent("being deleted");
    expect(screen.getByRole("alert")).toHaveTextContent("database web1: delete failed");
    expect(screen.getByRole("button", { name: "Create database" })).toBeDisabled();
    expect(screen.getByLabelText("Database name")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Branches" })).toBeDisabled();
  });

  it("polls a deleting project and goes back to the dashboard when it is gone", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { onGone, fetch } = setup({ "GET /v1/projects/p1": sequence(ok(deleting), notFound) });
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(onGone).not.toHaveBeenCalled();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    await vi.waitFor(() => expect(onGone).toHaveBeenCalledTimes(1));
    expect(fetch.calls.filter((r) => new URL(r.url).pathname === "/v1/projects/p1")).toHaveLength(2);
  });

  it("retry refetches the project and the workspace", async () => {
    let projectCalls = 0;
    const { fetch } = setup({
      "GET /v1/projects/p1": () => (projectCalls++ === 0 ? jsonResponse({ error: { code: "unavailable", message: "down" } }, 503) : jsonResponse(info)),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("heading", { name: "Default" })).toBeInTheDocument();
    const meCalls = fetch.calls.filter((r) => new URL(r.url).pathname === "/v1/me").length;
    expect(meCalls).toBeGreaterThanOrEqual(2);
  });

  it("goes back when the project does not exist", async () => {
    const { onGone } = setup({ "GET /v1/projects/p1": notFound });
    await vi.waitFor(() => expect(onGone).toHaveBeenCalledTimes(1));
  });

  it("expands a database into its branches", async () => {
    const { fetch } = setup({});
    await userEvent.click(await screen.findByRole("button", { name: "Branches" }));
    expect(await screen.findByText("0 of 5 branches")).toBeInTheDocument();
    expect(fetch.calls.map((r) => new URL(r.url).pathname)).toContain("/v1/databases/d1/branches");
    await userEvent.click(screen.getByRole("button", { name: "Branches" }));
    expect(screen.queryByText("0 of 5 branches")).toBeNull();
  });

  it("opens a branch's connection panel for a role from the link's params", async () => {
    const { fetch } = setup(
      {
        "GET /v1/databases/d1/merges": () => jsonResponse([]),
        "GET /v1/databases/d1/branches/b-main/roles": () => jsonResponse([]),
        "GET /v1/databases/d1/branches/b-main/databases": () => jsonResponse([]),
        "GET /v1/databases/d1/branches/b-main/connection": (req) =>
          jsonResponse({
            host: "ep-x.localtest.me", port: 4432, database: "web1", user: new URL(req.url).searchParams.get("role"),
            password: "pw-reporter", password_state: "generated", ca_file: null, http_url: "https://ep-x.localtest.me",
            uri: "postgres://reporter:pw-reporter@ep-x.localtest.me:4432/web1",
          }),
      },
      { focus: { databaseId: "d1", branchId: "b-main", role: "reporter" } },
    );
    // The database's branches are open, and the branch's connection shows the role (password masked).
    expect(await screen.findByText("reporter")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Branches" })).toHaveAttribute("aria-expanded", "true");
    expect(screen.queryByText("pw-reporter")).toBeNull();
    const call = fetch.calls.find((r) => new URL(r.url).pathname.endsWith("/connection"))!;
    expect(new URL(call.url).searchParams.get("role")).toBe("reporter");
  });

  it("opens the merges of a database", async () => {
    setup({
      "GET /v1/databases/d1/merges": () =>
        jsonResponse([
          {
            id: "m1", source_branch_id: "b-dev", target_branch_id: "b-main", direction: "into_parent", database: "web1",
            status: "draft", failed_phase: null, version: 1, rebaselined: false, error: null, created_by: "u1",
            created_at: "2026-10-06T10:00:00Z", updated_at: "2026-10-06T10:00:00Z",
          },
        ]),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Merges" }));
    const link = await screen.findByRole("link", { name: /into main/ });
    expect(link).toHaveAttribute("href", "/app/projects/p1/databases/d1/merges/m1");
    await userEvent.click(screen.getByRole("button", { name: "Merges" }));
    expect(screen.queryByRole("table", { name: "Merges" })).toBeNull();
  });

  it("database_row_buttons_are_branches_merges_connect_open", async () => {
    setup({});
    const row = (await screen.findByRole("cell", { name: "web1" })).closest("tr") as HTMLElement;
    const names = within(row)
      .getAllByRole("button")
      .map((b) => b.textContent);
    const link = within(row).getByRole("link", { name: /^Open/ });
    expect(names.slice(names.indexOf("Branches"))).toEqual(["Branches", "Merges", "Connect"]);
    expect(link).toHaveAttribute("href", "/app/tables/p1/d1");
    // Connect precedes Open in the row.
    expect(within(row).getByRole("button", { name: "Connect" }).compareDocumentPosition(link) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    for (const gone of ["Data API", "SQL", "Tables"]) {
      expect(within(row).queryByRole("button", { name: gone })).toBeNull();
      expect(within(row).queryByRole("link", { name: gone })).toBeNull();
    }
  });

  it("connect_toggles_the_panel_and_switches_with_branches", async () => {
    setup({});
    const connect = await screen.findByRole("button", { name: "Connect" });
    expect(connect).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(connect);
    expect(connect).toHaveAttribute("aria-expanded", "true");
    expect(await screen.findByRole("heading", { name: "Connect to web1" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Branches" }));
    expect(await screen.findByText("0 of 5 branches")).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Connect to web1" })).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Connect" }));
    expect(await screen.findByRole("heading", { name: "Connect to web1" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Connect" }));
    expect(screen.queryByRole("heading", { name: "Connect to web1" })).toBeNull();
    expect(screen.getByRole("button", { name: "Connect" })).toHaveAttribute("aria-expanded", "false");
  });

  it("not_ready_database_disables_connect_and_open", async () => {
    const starting = { ...database, status: "creating", host: null, port: null };
    const meStarting: Me = {
      ...me,
      tenant: { ...me.tenant!, projects: [{ ...me.tenant!.projects[0]!, databases: [starting] }] },
    } as Me;
    setup({ "GET /v1/me": ok(meStarting) });
    expect(await screen.findByRole("button", { name: "Connect" })).toBeDisabled();
    expect(screen.getByRole("button", { name: /^Open/ })).toBeDisabled();
    expect(screen.queryByRole("link", { name: /^Open/ })).toBeNull();
  });

  it("access_tab_is_in_the_strip", async () => {
    setup({
      "GET /v1/members": ok([]),
      "GET /v1/projects/p1/grants": ok([]),
      "GET /v1/projects/p1/access-events": ok({ events: [], next_before: null }),
    });
    await userEvent.click(await screen.findByRole("tab", { name: "Access" }));
    expect(await screen.findByRole("heading", { name: "People with access" })).toBeInTheDocument();
  });

  it("access_tab_says_when_the_project_is_not_in_the_workspace", async () => {
    const stranger: Me = { ...me, tenant: { ...me.tenant!, projects: [] } };
    const f = fakeFetch({ "GET /v1/me": ok(stranger), "GET /v1/projects/p1": ok(info), ...databaseRoutes });
    renderWithRouter(<ProjectPage projectId="p1" apiUrl="http://gw" tab="access" onTabChange={() => {}} onGone={() => {}} />, { fetch: f });
    expect(await screen.findByText("Default isn't in your workspace, or you can't see it. Go back to All projects and open it from there.")).toBeInTheDocument();
    expect(screen.queryByText("Loading access…")).toBeNull();
  });
});
