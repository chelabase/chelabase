import { act, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getGetMeQueryKey, type DatabaseStatus, type DatabaseSummary, type Me } from "@cheladb/management-client";
import { POLL_MS } from "@/lib/query";
import { createReauthGuard, restartSignInOnce } from "@/lib/reauth";
import { fakeFetch, jsonResponse, sequence, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
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
        databases: [{ database_id: "d1", name: "web1", status: "ready", host: "127.0.0.1", port: 49153, branch_count: 1 }],
      },
    ],
  },
};

function withDatabases(databases: DatabaseSummary[]): Me {
  return { ...bound, tenant: { ...bound.tenant!, projects: [{ ...bound.tenant!.projects[0], databases }] } };
}
const withStatus = (status: DatabaseStatus) => withDatabases([{ ...bound.tenant!.projects[0].databases[0], status }]);
const gone = withDatabases([]);

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
  const result = renderWithQuery(
    <Dashboard apiUrl="http://gw" profile={profile} onSignOut={() => {}} {...props} />,
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

const mainBranch = {
  id: "b-main", name: "main", parent_branch_id: null, kind: "normal", status: "ready", host: "ep-aaaaaaaaaaaa.localtest.me",
  port: 4432, branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null,
};
/** The branches and Data API views of database d1. */
const databaseRoutes: Record<string, FakeHandler> = {
  "GET /v1/databases/d1": () =>
    jsonResponse({
      ...bound.tenant!.projects[0].databases[0], tenant_id: "t1", project_id: "p1", error: null, history_window: 86400,
      max_branches: 5, max_rows: 1000, statement_timeout_ms: 8000, max_connections: 5, default_schema: "public",
    }),
  "GET /v1/databases/d1/branches": () => jsonResponse([mainBranch]),
  "GET /v1/databases/d1/branches/b-main/keys": () => jsonResponse([]),
  "GET /v1/databases/d1/branches/b-main/data-api/schema": () =>
    jsonResponse({ version: "1", default_schema: "public", schemas: {} }),
};

describe("Dashboard", () => {
  it("shows the tenant, project and its databases", async () => {
    setup(ok(bound));
    expect(await screen.findByRole("heading", { name: "demo / default" })).toBeInTheDocument();
    expect(screen.getByText("web1")).toBeInTheDocument();
    expect(screen.getByText("ready")).toBeInTheDocument();
    expect(screen.getByText("127.0.0.1:49153")).toBeInTheDocument();
    expect(screen.getByText("Ada Lovelace")).toBeInTheDocument();
    expect(screen.getByText("ada@example.com")).toBeInTheDocument();
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
    expect(await screen.findByRole("heading", { name: "demo / default" })).toBeInTheDocument();
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
    expect(await screen.findByRole("heading", { name: "demo / default" })).toBeInTheDocument();
  });

  it("dashboard_polls_while_provisioning_and_stops_when_ready", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = setup(sequence(ok(withStatus("provisioning")), ok(withStatus("ready"))));
    expect(await screen.findByText("provisioning")).toBeInTheDocument();
    expect(meCalls(fetch)).toBe(1);

    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(await screen.findByText("ready")).toBeInTheDocument();
    expect(meCalls(fetch)).toBe(2);

    await act(() => vi.advanceTimersByTimeAsync(POLL_MS * 5));
    expect(meCalls(fetch)).toBe(2);
  });

  it("keeps refreshing while a database is being deleted", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = setup(sequence(ok(withStatus("deleting")), ok(gone)));
    expect(await screen.findByText("deleting")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(await screen.findByText("No databases yet.")).toBeInTheDocument();
    expect(meCalls(fetch)).toBe(2);
    // The last deleting database is gone: polling stops.
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS * 5));
    expect(meCalls(fetch)).toBe(2);
  });

  it("doesn't poll an empty list", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = setup(ok(gone));
    expect(await screen.findByText("No databases yet.")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS * 5));
    expect(meCalls(fetch)).toBe(1);
  });

  it("keeps the last good list when a refresh fails, and keeps refreshing", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = setup(
      sequence(ok(withStatus("deleting")), fail(502, "unavailable", "gateway hiccup"), ok(gone)),
    );
    expect(await screen.findByText("deleting")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(await screen.findByText("Couldn't refresh (gateway hiccup); retrying…")).toBeInTheDocument();
    expect(screen.getByText("web1")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(await screen.findByText("No databases yet.")).toBeInTheDocument();
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

  it("expands a database into its branches", async () => {
    const { fetch } = setup(ok(bound), {}, undefined, databaseRoutes);
    await userEvent.click(await screen.findByRole("button", { name: "Branches" }));
    expect(await screen.findByText("0 of 5 branches")).toBeInTheDocument();
    expect(fetch.calls.map((r) => new URL(r.url).pathname)).toContain("/v1/databases/d1/branches");
    await userEvent.click(screen.getByRole("button", { name: "Branches" }));
    expect(screen.queryByText("0 of 5 branches")).toBeNull();
  });

  it("opens the Data API view of a database, switching with Branches", async () => {
    setup(ok(bound), {}, undefined, databaseRoutes);
    await userEvent.click(await screen.findByRole("button", { name: "Data API" }));
    expect(await screen.findByText("http://gw/data/v1/query")).toBeInTheDocument();
    expect(screen.queryByText(/of 5 branches/)).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Branches" }));
    expect(await screen.findByText("0 of 5 branches")).toBeInTheDocument();
    expect(screen.queryByText("http://gw/data/v1/query")).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Branches" }));
    expect(screen.queryByText("0 of 5 branches")).not.toBeInTheDocument();
  });

  it("opens the Tables console of a database", async () => {
    setup(ok(bound), {}, undefined, {
      ...databaseRoutes,
      "GET /v1/databases/d1/branches/b-main/databases": () =>
        jsonResponse([{ name: "app", main: true, data_api: "on", data_api_error: null, default_schema: "public", origin: "owner", owner: "o" }]),
      "GET /v1/databases/d1/branches/b-main/roles": () => jsonResponse([]),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Tables" }));
    expect(await screen.findByLabelText("View as")).toBeInTheDocument();
    expect(screen.getByText(/Viewing as owner/)).toBeInTheDocument();
  });
});
