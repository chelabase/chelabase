import { act, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Branch, BranchStatus, Database, DatabaseSummary } from "@cheladb/management-client";
import { POLL_MS } from "@/lib/query";
import { fakeFetch, jsonResponse, sequence, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { DatabaseBranches } from "./database-branches";

const summary: DatabaseSummary = {
  database_id: "db",
  name: "web1",
  status: "ready",
  host: "h",
  port: 4432,
  branch_count: 3,
};
const detail: Database = {
  database_id: "db",
  tenant_id: "t1",
  project_id: "p1",
  name: "web1",
  status: "ready",
  error: null,
  host: "h",
  port: 4432,
  history_window: 86400,
  max_branches: 5,
  max_rows: 1000,
  statement_timeout_ms: 8000,
  max_connections: 5,
  default_schema: "public",
  branch_count: 3,
};
const b = (name: string, over: Partial<Branch> = {}): Branch => ({
  id: `id-${name}`,
  name,
  parent_branch_id: name === "main" ? null : "id-main",
  kind: "normal",
  status: "ready",
  host: "h",
  port: 4432,
  branched_at_time: null,
  branched_at_lsn: null,
  created_at: `2026-09-30T10:0${name.length}:00Z`,
  history_start: null,
  error: null,
  ...over,
});

const LIST = "GET /v1/databases/db/branches";
const GET = "GET /v1/databases/db";
const listCalls = (f: FakeFetch) => f.calls.filter((r) => `${r.method} ${new URL(r.url).pathname}` === LIST).length;

function setup(list: FakeHandler, get: FakeHandler = () => jsonResponse(detail)) {
  const f = fakeFetch({ [LIST]: list, [GET]: get });
  const result = renderWithQuery(<DatabaseBranches database={summary} />, { fetch: f });
  return { ...result, fetch: f };
}

afterEach(() => vi.useRealTimers());

describe("DatabaseBranches", () => {
  it("shows the cap as branches other than main against the limit", async () => {
    setup(() => jsonResponse([b("main"), b("dev"), b("feat")]));
    expect(await screen.findByText("2 of 5 branches")).toBeInTheDocument();
    expect(document.querySelector("tr[data-branch=dev]")).not.toBeNull();
  });

  it("counts without the limit until the database's details arrive", async () => {
    setup(
      () => jsonResponse([b("main"), b("dev")]),
      () => new Promise<never>(() => {}),
    );
    expect(await screen.findByText("1 branches")).toBeInTheDocument();
  });

  it("shows the settings once the database's details arrive", async () => {
    setup(() => jsonResponse([b("main")]));
    expect(await screen.findByLabelText("Max rows")).toHaveValue(1000);
  });

  it("branches_poll_while_resetting: polls while a branch is resetting and stops once it settles", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const resetting = (status: BranchStatus) => () => jsonResponse([b("main"), b("dev", { status })]);
    const { fetch } = setup(sequence(resetting("resetting"), resetting("resetting"), resetting("ready")));
    expect(await screen.findByText("resetting", { selector: "[data-branch=dev] *" })).toBeInTheDocument();
    expect(listCalls(fetch)).toBe(1);
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    await waitFor(() => expect(listCalls(fetch)).toBe(2));
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    await waitFor(() => expect(listCalls(fetch)).toBe(3));
    expect(await screen.findByText("ready", { selector: "[data-branch=dev] *" })).toBeInTheDocument();
    for (let i = 0; i < 5; i++) await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(listCalls(fetch)).toBe(3);
  });

  it("keeps polling while a branch is provisioning, deleting or pending", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    for (const status of ["provisioning", "deleting", "pending"] as const) {
      const { fetch, unmount } = setup(sequence(() => jsonResponse([b("main"), b("dev", { status, host: null })])));
      await screen.findByText(status, { selector: "[data-branch=dev] *" });
      await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
      await waitFor(() => expect(listCalls(fetch)).toBe(2));
      unmount();
    }
  });

  it("doesn't poll when every branch has settled", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = setup(() => jsonResponse([b("main"), b("dev", { status: "failed", error: "boom" })]));
    await screen.findByText("boom");
    for (let i = 0; i < 5; i++) await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(listCalls(fetch)).toBe(1);
  });

  it("shows a load error", async () => {
    setup(() => jsonResponse({ error: { code: "unavailable", message: "gateway down" } }, 503));
    expect(await screen.findByRole("alert")).toHaveTextContent("gateway down");
  });

  it("refreshes the list after a branch is created", async () => {
    const f = fakeFetch({
      [LIST]: sequence(
        () => jsonResponse([b("main")]),
        () => jsonResponse([b("main"), b("feat", { status: "provisioning", host: null })]),
      ),
      [GET]: () => jsonResponse(detail),
      "POST /v1/databases/db/branches": () => jsonResponse(b("feat", { status: "pending" }), 202),
    });
    renderWithQuery(<DatabaseBranches database={summary} />, { fetch: f });
    await userEvent.type(await screen.findByLabelText("Branch name"), "feat");
    await userEvent.click(screen.getByRole("button", { name: "Create branch" }));
    expect(await screen.findByText("1 of 5 branches")).toBeInTheDocument();
    expect(document.querySelector("tr[data-branch=feat]")).not.toBeNull();
  });
});
