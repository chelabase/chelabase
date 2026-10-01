import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { MeResponse } from "@cheladb/types";
import { ApiError, type ApiClient } from "@/lib/api";
import { fakeApi as baseFakeApi } from "@/test/fake-api";
import { Dashboard } from "./dashboard";

const bound: MeResponse = {
  user: { sub: "u1", org_id: "org-1" },
  tenant: {
    id: "t1" as never,
    slug: "demo",
    projects: [
      {
        id: "p1" as never,
        slug: "default",
        databases: [
          { database_id: "d1" as never, name: "web1", status: "ready", host: "127.0.0.1", port: 49153, branch_count: 1 },
        ],
      },
    ],
  },
};

function fakeApi(me: () => Promise<MeResponse>): ApiClient {
  return baseFakeApi({ me: vi.fn(me) });
}

const profile = { name: "Ada Lovelace", email: "ada@example.com" };

describe("Dashboard", () => {
  it("shows the tenant, project and its databases", async () => {
    render(<Dashboard apiUrl="http://gw" api={fakeApi(async () => bound)} profile={profile} onSignOut={() => {}} />);
    expect(await screen.findByRole("heading", { name: "demo / default" })).toBeInTheDocument();
    expect(screen.getByText("web1")).toBeInTheDocument();
    expect(screen.getByText("ready")).toBeInTheDocument();
    expect(screen.getByText("127.0.0.1:49153")).toBeInTheDocument();
    expect(screen.getByText("Ada Lovelace")).toBeInTheDocument();
    expect(screen.getByText("ada@example.com")).toBeInTheDocument();
  });

  it("says when the tenant has no projects", async () => {
    const empty: MeResponse = { ...bound, tenant: { ...bound.tenant!, projects: [] } };
    render(<Dashboard apiUrl="http://gw" api={fakeApi(async () => empty)} profile={profile} onSignOut={() => {}} />);
    expect(await screen.findByText(/no projects yet/i)).toBeInTheDocument();
  });

  it("shows a sign-in restart instead of an error", async () => {
    const me = vi.fn<() => Promise<MeResponse>>().mockRejectedValue(new ApiError(401, "token expired", true));
    render(<Dashboard apiUrl="http://gw" api={fakeApi(me)} profile={profile} onSignOut={() => {}} />);
    expect(await screen.findByText(/signing you in again/i)).toBeInTheDocument();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("explains an organization with no tenant yet", async () => {
    render(
      <Dashboard apiUrl="http://gw" api={fakeApi(async () => ({ ...bound, tenant: null }))} profile={profile} onSignOut={() => {}} />,
    );
    expect(await screen.findByText(/isn't set up yet/)).toBeInTheDocument();
  });

  it("shows load errors and retries", async () => {
    const me = vi
      .fn<() => Promise<MeResponse>>()
      .mockRejectedValueOnce(new Error("auth unavailable"))
      .mockResolvedValueOnce(bound);
    render(<Dashboard apiUrl="http://gw" api={fakeApi(me)} profile={profile} onSignOut={() => {}} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("auth unavailable");
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("heading", { name: "demo / default" })).toBeInTheDocument();
  });

  it("keeps refreshing while a database is being deleted", async () => {
    const deleting: MeResponse = {
      ...bound,
      tenant: {
        ...bound.tenant!,
        projects: [{ ...bound.tenant!.projects[0], databases: [{ ...bound.tenant!.projects[0].databases[0], status: "deleting" }] }],
      },
    };
    const gone: MeResponse = {
      ...bound,
      tenant: { ...bound.tenant!, projects: [{ ...bound.tenant!.projects[0], databases: [] }] },
    };
    const me = vi.fn<() => Promise<MeResponse>>().mockResolvedValueOnce(deleting).mockResolvedValue(gone);
    render(<Dashboard apiUrl="http://gw" api={fakeApi(me)} profile={profile} onSignOut={() => {}} pollIntervalMs={0} />);
    expect(await screen.findByText("No databases yet.")).toBeInTheDocument();
    expect(me.mock.calls.length).toBeGreaterThanOrEqual(2);
  });

  it("keeps the last good list when a refresh fails, and keeps refreshing", async () => {
    const deleting: MeResponse = {
      ...bound,
      tenant: {
        ...bound.tenant!,
        projects: [{ ...bound.tenant!.projects[0], databases: [{ ...bound.tenant!.projects[0].databases[0], status: "deleting" }] }],
      },
    };
    const gone: MeResponse = {
      ...bound,
      tenant: { ...bound.tenant!, projects: [{ ...bound.tenant!.projects[0], databases: [] }] },
    };
    const me = vi
      .fn<() => Promise<MeResponse>>()
      .mockResolvedValueOnce(deleting)
      .mockRejectedValueOnce(new Error("gateway hiccup"))
      .mockResolvedValue(gone);
    render(<Dashboard apiUrl="http://gw" api={fakeApi(me)} profile={profile} onSignOut={() => {}} pollIntervalMs={0} />);
    expect(await screen.findByText("No databases yet.")).toBeInTheDocument();
    expect(me.mock.calls.length).toBeGreaterThanOrEqual(3);
    expect(screen.queryByText(/Couldn't load your workspace/)).toBeNull();
  });

  it("signs out", async () => {
    const onSignOut = vi.fn();
    render(<Dashboard apiUrl="http://gw" api={fakeApi(async () => bound)} profile={profile} onSignOut={onSignOut} />);
    await userEvent.click(await screen.findByRole("button", { name: "Sign out" }));
    expect(onSignOut).toHaveBeenCalledTimes(1);
  });

  it("expands a database into its branches", async () => {
    const api = fakeApi(async () => bound);
    api.getDatabase = vi.fn().mockResolvedValue({ history_window: 86400, max_branches: 5, branch_count: 1 });
    api.listBranches = vi.fn().mockResolvedValue([
      {
        id: "d1", name: "main", parent_branch_id: null, kind: "normal", status: "ready", host: "ep-aaaaaaaaaaaa.localtest.me",
        port: 4432, branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null,
      },
    ]);
    render(<Dashboard apiUrl="http://gw" api={api} profile={profile} onSignOut={() => {}} />);
    await userEvent.click(await screen.findByRole("button", { name: "Branches" }));
    expect(await screen.findByText("0 of 5 branches")).toBeInTheDocument();
    expect(api.listBranches).toHaveBeenCalledWith("d1");
    await userEvent.click(screen.getByRole("button", { name: "Branches" }));
    expect(screen.queryByText("0 of 5 branches")).toBeNull();
  });

  it("opens the Data API view of a database, switching with Branches", async () => {
    const api = fakeApi(async () => bound);
    api.getDatabase = vi.fn().mockResolvedValue({ history_window: 86400, max_branches: 5, branch_count: 1 });
    api.listBranches = vi.fn().mockResolvedValue([
      {
        id: "d1", name: "main", parent_branch_id: null, kind: "normal", status: "ready", host: "h",
        port: 4432, branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null,
      },
    ]);
    render(<Dashboard apiUrl="http://gw" api={api} profile={profile} onSignOut={() => {}} />);
    await userEvent.click(await screen.findByRole("button", { name: "Data API" }));
    expect(await screen.findByText("http://gw/data/v1/query")).toBeInTheDocument();
    expect(screen.queryByText(/of 5 branches/)).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Branches" }));
    expect(await screen.findByText("0 of 5 branches")).toBeInTheDocument();
    expect(screen.queryByText("http://gw/data/v1/query")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Branches" }));
    expect(screen.queryByText("0 of 5 branches")).not.toBeInTheDocument();
  });
});
