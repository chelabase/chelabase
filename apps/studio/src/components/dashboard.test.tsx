import { act, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getGetMeQueryKey, type DatabaseSummary, type Me, type ProjectStatusView } from "@cheladb/management-client";
import { POLL_MS } from "@/lib/query";
import { createReauthGuard, restartSignInOnce } from "@/lib/reauth";
import { fakeFetch, jsonResponse, sequence, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithRouter } from "@/test/render-router";
import { Dashboard, type DashboardProps } from "./dashboard";

const bound: Me = {
  user: { sub: "u1", org_id: "org-1" },
  tenant: {
    id: "t1",
    slug: "demo",
    projects: [
      {
        id: "p1",
        slug: "default",
        name: "Default",
        status: "active",
        databases: [{ database_id: "d1", name: "web1", status: "ready", host: "127.0.0.1", port: 49153, branch_count: 1 }],
      },
    ],
  },
};

function withDatabases(databases: DatabaseSummary[]): Me {
  return { ...bound, tenant: { ...bound.tenant!, projects: [{ ...bound.tenant!.projects[0], databases }] } };
}
const withStatus = (status: ProjectStatusView): Me => ({
  ...bound,
  tenant: { ...bound.tenant!, projects: [{ ...bound.tenant!.projects[0], status }] },
});
const noDatabases = withDatabases([]);
const noProjects: Me = { ...bound, tenant: { ...bound.tenant!, projects: [] } };

const ok = (me: Me) => () => jsonResponse(me);
const fail = (status: number, code: string, message: string) => () => jsonResponse({ error: { code, message } }, status);
const meCalls = (f: FakeFetch) => f.calls.filter((r) => new URL(r.url).pathname === "/v1/me").length;

const profile = { name: "Ada Lovelace", email: "ada@example.com" };

function setup(
  me: FakeHandler,
  props: Partial<DashboardProps> = {},
  onUnauthorized?: () => boolean,
  routes: Record<string, FakeHandler> = {},
) {
  const f = fakeFetch({ "GET /v1/me": me, ...routes });
  const result = renderWithRouter(
    <Dashboard profile={profile} onSignOut={() => {}} {...props} />,
    { fetch: f, onUnauthorized },
  );
  return { ...result, fetch: f };
}

function memoryStorage() {
  const data = new Map<string, string>();
  return {
    getItem: (k: string) => data.get(k) ?? null,
    setItem: (k: string, v: string) => void data.set(k, v),
    removeItem: (k: string) => void data.delete(k),
  };
}

afterEach(() => vi.useRealTimers());

describe("Dashboard", () => {
  it("dashboard_lists_projects_and_links", async () => {
    setup(ok(bound));
    const link = await screen.findByRole("link", { name: "Default" });
    expect(link).toHaveAttribute("href", "/app/projects/p1");
    expect(screen.getByText("default")).toBeInTheDocument();
    expect(screen.getByText("active")).toBeInTheDocument();
    expect(screen.getByText("1 database")).toBeInTheDocument();
    expect(screen.getByText("Ada Lovelace")).toBeInTheDocument();
    expect(screen.getByText("ada@example.com")).toBeInTheDocument();
    await userEvent.click(link);
    expect(await screen.findByText("project route p1")).toBeInTheDocument();
  });

  it("new_project_dialog_creates_and_navigates", async () => {
    setup(ok(bound), {}, undefined, {
      "POST /v1/projects": () =>
        jsonResponse({ id: "p9", slug: "shop", name: "Shop", status: "active", error: null, app_users: null, created_at: "t", updated_at: "t" }, 201),
    });
    await userEvent.click(await screen.findByRole("button", { name: "New project" }));
    await userEvent.type(await screen.findByLabelText("Project name"), "Shop");
    await userEvent.click(screen.getByRole("button", { name: "Create project" }));
    expect(await screen.findByText("project route p9")).toBeInTheDocument();
  });

  it("says when the tenant has no projects", async () => {
    setup(ok({ ...bound, tenant: { ...bound.tenant!, projects: [] } }));
    expect(await screen.findByText(/no projects yet/i)).toBeInTheDocument();
  });

  it("shows a sign-in restart instead of an error", async () => {
    setup(fail(401, "unauthorized", "token expired"), {}, () => true);
    expect(await screen.findByText(/signing you in again/i)).toBeInTheDocument();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("second_401_shows_error_without_redirect_loop", async () => {
    const storage = memoryStorage();
    const restart = vi.fn();
    // The first 401 already restarted sign-in; this is the page after the round-trip.
    expect(createReauthGuard(storage).shouldRedirect()).toBe(true);
    const onUnauthorized = restartSignInOnce(createReauthGuard(storage), restart);
    setup(fail(401, "unauthorized", "token rejected"), {}, onUnauthorized);
    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't load your workspace: token rejected");
    expect(restart).not.toHaveBeenCalled();
    expect(screen.queryByText(/signing you in again/i)).toBeNull();
  });

  it("getme_success_resets_the_guard", async () => {
    const storage = memoryStorage();
    const guard = createReauthGuard(storage);
    expect(guard.shouldRedirect()).toBe(true);
    const onLoaded = vi.fn(() => guard.reset());
    setup(ok(bound), { onLoaded });
    expect(await screen.findByRole("link", { name: "Default" })).toBeInTheDocument();
    expect(onLoaded).toHaveBeenCalledTimes(1);
    // Re-armed: a later 401 may restart sign-in again.
    expect(createReauthGuard(storage).shouldRedirect()).toBe(true);
  });

  it("doesn't reset the guard when getMe fails", async () => {
    const onLoaded = vi.fn();
    setup(fail(403, "tenant_not_bound", "missing org claim"), { onLoaded });
    expect(await screen.findByRole("alert")).toHaveTextContent("missing org claim");
    expect(onLoaded).not.toHaveBeenCalled();
  });

  it("explains an organization with no tenant yet", async () => {
    setup(ok({ ...bound, tenant: null }));
    expect(await screen.findByText(/isn't set up yet/)).toBeInTheDocument();
  });

  it("shows load errors and retries", async () => {
    setup(sequence(fail(503, "unavailable", "auth unavailable"), ok(bound)));
    expect(await screen.findByRole("alert")).toHaveTextContent("auth unavailable");
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("link", { name: "Default" })).toBeInTheDocument();
  });

  it("dashboard_polls_while_a_project_is_deleting_and_stops_when_it_settles", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = setup(sequence(ok(withStatus("deleting")), ok(noProjects)));
    expect(await screen.findByText("deleting")).toBeInTheDocument();
    expect(meCalls(fetch)).toBe(1);
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(await screen.findByText(/no projects yet/i)).toBeInTheDocument();
    expect(meCalls(fetch)).toBe(2);
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS * 5));
    expect(meCalls(fetch)).toBe(2);
  });

  it("polls while a database is provisioning", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const provisioning = withDatabases([{ ...bound.tenant!.projects[0].databases[0], status: "provisioning" }]);
    const { fetch } = setup(sequence(ok(provisioning), ok(bound)));
    expect(await screen.findByRole("link", { name: "Default" })).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(meCalls(fetch)).toBe(2);
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS * 5));
    expect(meCalls(fetch)).toBe(2);
  });

  it("doesn't poll a settled list", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = setup(ok(noDatabases));
    expect(await screen.findByText("0 databases")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS * 5));
    expect(meCalls(fetch)).toBe(1);
  });

  it("keeps the last good list when a refresh fails, and keeps refreshing", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = setup(
      sequence(ok(withStatus("deleting")), fail(502, "unavailable", "gateway hiccup"), ok(noProjects)),
    );
    expect(await screen.findByText("deleting")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(await screen.findByText("Couldn't refresh (gateway hiccup); retrying…")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Default" })).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(await screen.findByText(/no projects yet/i)).toBeInTheDocument();
    expect(meCalls(fetch)).toBeGreaterThanOrEqual(3);
    expect(screen.queryByText(/Couldn't load your workspace/)).toBeNull();
    expect(screen.queryByText(/Couldn't refresh/)).toBeNull();
  });

  it("signs out and clears the query cache", async () => {
    const onSignOut = vi.fn();
    const { queryClient } = setup(ok(bound), { onSignOut });
    await userEvent.click(await screen.findByRole("button", { name: "Sign out" }));
    expect(onSignOut).toHaveBeenCalledTimes(1);
    expect(queryClient.getQueryData(getGetMeQueryKey())).toBeUndefined();
    expect(queryClient.getQueryCache().findAll()).toHaveLength(0);
  });
});
