import { act, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getGetDatabaseQueryKey, getGetMeQueryKey, type Database, type DatabaseStatus } from "@cheladb/management-client";
import { POLL_MS } from "@/lib/query";
import { fakeFetch, jsonResponse, sequence, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { CreateDatabaseForm } from "./create-database-form";

function detail(status: DatabaseStatus, error: string | null = null): Database {
  return {
    database_id: "d1",
    project_id: "p1",
    tenant_id: "t1",
    name: "web1",
    status,
    host: null,
    port: null,
    error,
    branch_count: 1,
    history_window: 604800,
    max_branches: 5,
    max_rows: 1000,
    statement_timeout_ms: 8000,
    max_connections: 5,
  };
}

const accepted = () => jsonResponse({ job_id: "j", database_id: "d1", status: "pending" }, 202);

function setup(routes: Record<string, FakeHandler>) {
  const f = fakeFetch(routes);
  const result = renderWithQuery(<CreateDatabaseForm tenantId="t1" projectId="p1" />, { fetch: f });
  return { ...result, fetch: f };
}
const calls = (f: ReturnType<typeof fakeFetch>, key: string) =>
  f.calls.filter((r) => `${r.method} ${new URL(r.url).pathname}` === key);

async function submit(name: string, user = userEvent.setup()) {
  await user.type(screen.getByLabelText("Database name"), name);
  await user.click(screen.getByRole("button", { name: "Create database" }));
}

afterEach(() => vi.useRealTimers());

// Review Focus 3.
describe("CreateDatabaseForm", () => {
  it("creates with the workspace ids, trims the name and polls until ready", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = setup({
      "POST /v1/databases": accepted,
      "GET /v1/databases/d1": sequence(() => jsonResponse(detail("provisioning")), () => jsonResponse(detail("ready"))),
    });

    await submit("  web1  ", userEvent.setup({ advanceTimers: vi.advanceTimersByTime }));
    expect(await screen.findByText("Creating web1… (provisioning)")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));

    expect(await screen.findByText("web1 is ready")).toBeInTheDocument();
    const [post] = calls(fetch, "POST /v1/databases");
    expect(await post.json()).toEqual({ tenant_id: "t1", project_id: "p1", name: "web1" });
    expect(calls(fetch, "GET /v1/databases/d1")).toHaveLength(2);

    // Settled: no more polling.
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS * 3));
    expect(calls(fetch, "GET /v1/databases/d1")).toHaveLength(2);
  });

  it("does not submit a blank name", async () => {
    const { fetch } = setup({});
    await userEvent.type(screen.getByLabelText("Database name"), "   ");
    expect(screen.getByRole("button", { name: "Create database" })).toBeDisabled();
    expect(fetch.calls).toHaveLength(0);
  });

  it("shows a provisioning failure", async () => {
    setup({
      "POST /v1/databases": accepted,
      "GET /v1/databases/d1": () => jsonResponse(detail("failed", "docker: image pull failed")),
    });
    await submit("web1");
    expect(await screen.findByRole("alert")).toHaveTextContent("docker: image pull failed");
  });

  it("shows the API's rejection", async () => {
    setup({
      "POST /v1/databases": () =>
        jsonResponse({ error: { code: "name_taken", message: "database name already exists in project" } }, 409),
    });
    await submit("web1");
    expect(await screen.findByRole("alert")).toHaveTextContent("already exists");
  });

  it("create_database_invalidates_me", async () => {
    const { queryClient } = setup({
      "POST /v1/databases": accepted,
      "GET /v1/databases/d1": () => jsonResponse(detail("provisioning")),
    });
    queryClient.setQueryData(getGetMeQueryKey(), { user: { sub: "u", org_id: "o" }, tenant: null });
    expect(queryClient.getQueryState(getGetMeQueryKey())?.isInvalidated).toBe(false);
    await submit("web1");
    await vi.waitFor(() => expect(queryClient.getQueryState(getGetMeQueryKey())?.isInvalidated).toBe(true));
  });

  it("stops following the database once it has settled", async () => {
    const { queryClient } = setup({
      "POST /v1/databases": accepted,
      "GET /v1/databases/d1": sequence(() => jsonResponse(detail("ready")), () => jsonResponse(detail("deleting"))),
    });
    await submit("web1");
    expect(await screen.findByText("web1 is ready")).toBeInTheDocument();

    // Later the database is being deleted (seen by a refetch, or by another screen sharing the cache).
    await act(() => queryClient.invalidateQueries({ queryKey: getGetDatabaseQueryKey("d1") }));
    act(() => queryClient.setQueryData(getGetDatabaseQueryKey("d1"), detail("deleting")));

    expect(screen.queryByText(/Creating web1…/)).toBeNull();
    expect(screen.getByText("web1 is ready")).toBeInTheDocument();
    await userEvent.type(screen.getByLabelText("Database name"), "web2");
    expect(screen.getByRole("button", { name: "Create database" })).toBeEnabled();
  });
});
