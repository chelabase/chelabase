import { useState } from "react";
import { act, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Me, ProjectInfo } from "@cheladb/management-client";
import { POLL_MS } from "@/lib/query";
import { fakeFetch, jsonResponse, sequence, type FakeHandler } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { ProjectPage, type ProjectPageProps, type ProjectTab } from "./project-page";

const database = { database_id: "d1", name: "web1", status: "ready", host: "127.0.0.1", port: 49153, branch_count: 1 } as const;
const me: Me = {
  user: { sub: "u1", org_id: "org-1" },
  tenant: {
    id: "t1",
    slug: "demo",
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
          profile={{ name: "Ada" }}
        onSignOut={() => {}}
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

  it("opens the Data API view of a database, switching with Branches", async () => {
    setup({});
    await userEvent.click(await screen.findByRole("button", { name: "Data API" }));
    expect(await screen.findByText("http://gw/data/v1/query")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Branches" }));
    expect(await screen.findByText("0 of 5 branches")).toBeInTheDocument();
    expect(screen.queryByText("http://gw/data/v1/query")).not.toBeInTheDocument();
  });

  it("opens the Tables console of a database", async () => {
    setup({
      "GET /v1/databases/d1/branches/b-main/databases": () =>
        jsonResponse([{ name: "app", main: true, data_api: "on", data_api_error: null, default_schema: "public", origin: "owner", owner: "o" }]),
      "GET /v1/databases/d1/branches/b-main/roles": () => jsonResponse([]),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Tables" }));
    expect(await screen.findByLabelText("View as")).toBeInTheDocument();
    expect(screen.getByText(/Viewing as owner/)).toBeInTheDocument();
  });
});
