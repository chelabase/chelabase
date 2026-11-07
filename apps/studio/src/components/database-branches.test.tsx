import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Branch, BranchStatus, Database, DatabaseSummary } from "@chelabase/management-client";
import { POLL_MS } from "@/lib/query";
import { fakeFetch, jsonResponse, sequence, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { renderWithRouter } from "@/test/render-router";
import { DatabaseBranches } from "./database-branches";

const summary: DatabaseSummary = {
  database_id: "db",
  name: "web1",
  postgres_database: "web1",
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
  postgres_database: "web1",
  status: "ready",
  error: null,
  host: "h",
  port: 4432,
  history_window: 86400,
  max_branches: 5,
  max_rows: 1000,
  statement_timeout_ms: 8000,
  max_connections: 5,
  storage_max_connections: 2,
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
  protected: false,
  main_database_missing: false, guard_logins: true, review_acknowledged_hash: null,
  ...over,
});

const LIST = "GET /v1/databases/db/branches";
const GET = "GET /v1/databases/db";
const listCalls = (f: FakeFetch) => f.calls.filter((r) => `${r.method} ${new URL(r.url).pathname}` === LIST).length;

function setup(list: FakeHandler, get: FakeHandler = () => jsonResponse(detail)) {
  const f = fakeFetch({ [LIST]: list, [GET]: get });
  const result = renderWithQuery(<DatabaseBranches projectId="p1" database={summary} />, { fetch: f });
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

  it("refreshes the merges while one is busy, so the merge buttons free up when it ends (Studio minor 11)", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const merge = (status: string) => ({
      id: "m1", source_branch_id: "id-dev", target_branch_id: "id-main", direction: "into_parent", database: "web1",
      status, failed_phase: null, version: 1, rebaselined: false, error: null, created_by: "u1",
      created_at: "2026-10-06T10:00:00Z", updated_at: "2026-10-06T10:00:00Z",
    });
    const f = fakeFetch({
      [LIST]: () => jsonResponse([b("main"), b("dev")]),
      [GET]: () => jsonResponse(detail),
      "GET /v1/databases/db/merges": sequence(() => jsonResponse([merge("rehearsing")]), () => jsonResponse([merge("done")])),
    });
    renderWithRouter(<DatabaseBranches projectId="p1" database={summary} />, { fetch: f });
    const mergeCalls = () => f.calls.filter((r) => new URL(r.url).pathname === "/v1/databases/db/merges").length;
    await screen.findByText("1 of 5 branches");
    await waitFor(() => expect(mergeCalls()).toBe(1));
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    await waitFor(() => expect(mergeCalls()).toBe(2));
    // It ended: no more polling.
    for (let i = 0; i < 5; i++) await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    expect(mergeCalls()).toBe(2);
  });

  it("finds an open merge by the Postgres database name, which sanitizing changes (D1)", async () => {
    const named: DatabaseSummary = { ...summary, name: "web-1 prod", postgres_database: "web_1_prod" };
    const merge = (id: string, database: string) => ({
      id, source_branch_id: "id-dev", target_branch_id: "id-main", direction: "into_parent", database,
      status: "draft", failed_phase: null, version: 1, rebaselined: false, error: null, created_by: "u1",
      created_at: "2026-10-06T10:00:00Z", updated_at: "2026-10-06T10:00:00Z",
    });
    const f = fakeFetch({
      [LIST]: () => jsonResponse([b("main"), b("dev")]),
      [GET]: () => jsonResponse({ ...detail, name: "web-1 prod", postgres_database: "web_1_prod" }),
      "GET /v1/databases/db/merges": () => jsonResponse([merge("m-other", "analytics"), merge("m1", "web_1_prod")]),
    });
    renderWithRouter(<DatabaseBranches projectId="p1" database={named} />, { fetch: f });
    const row = await waitFor(() => {
      const r = document.querySelector<HTMLElement>("tr[data-branch=dev]");
      if (!r) throw new Error("no dev row yet");
      return r;
    });
    const into = await within(row).findByRole("button", { name: "Merge into main" });
    await waitFor(() => expect(into).toBeDisabled());
    expect(into).toHaveAccessibleDescription(/a merge into main is already open/);
    expect(within(row).getByRole("link", { name: "Open the merge into main" })).toHaveAttribute(
      "href",
      "/app/projects/p1/databases/db/merges/m1",
    );
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
    renderWithQuery(<DatabaseBranches projectId="p1" database={summary} />, { fetch: f });
    await userEvent.type(await screen.findByLabelText("Branch name"), "feat");
    await userEvent.click(screen.getByRole("button", { name: "Create branch" }));
    expect(await screen.findByText("1 of 5 branches")).toBeInTheDocument();
    expect(document.querySelector("tr[data-branch=feat]")).not.toBeNull();
  });
});
