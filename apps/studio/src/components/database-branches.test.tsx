import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { BranchSummary, DatabaseDetail, DatabaseSummary } from "@cheladb/types";
import { fakeApi } from "@/test/fake-api";
import { DatabaseBranches } from "./database-branches";

const summary: DatabaseSummary = {
  database_id: "db" as never,
  name: "web1",
  status: "ready",
  host: "h",
  port: 4432,
  branch_count: 3,
};
const detail = { database_id: "db", name: "web1", status: "ready", history_window: 86400, max_branches: 5, branch_count: 3 } as unknown as DatabaseDetail;
const b = (name: string, over: Partial<BranchSummary> = {}): BranchSummary => ({
  id: `id-${name}` as never,
  name,
  parent_branch_id: name === "main" ? null : ("id-main" as never),
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

describe("DatabaseBranches", () => {
  it("shows the cap as branches other than main against the limit", async () => {
    const api = fakeApi({
      getDatabase: vi.fn().mockResolvedValue(detail),
      listBranches: vi.fn().mockResolvedValue([b("main"), b("dev"), b("feat")]),
    });
    render(<DatabaseBranches api={api} database={summary} />);
    expect(await screen.findByText("2 of 5 branches")).toBeInTheDocument();
    expect(document.querySelector("tr[data-branch=dev]")).not.toBeNull();
  });

  it("keeps polling while a branch is in a transitional state", async () => {
    const listBranches = vi
      .fn()
      .mockResolvedValueOnce([b("main"), b("dev", { status: "provisioning", host: null })])
      .mockResolvedValue([b("main"), b("dev")]);
    const api = fakeApi({ getDatabase: vi.fn().mockResolvedValue(detail), listBranches });
    render(<DatabaseBranches api={api} database={summary} pollIntervalMs={10} />);
    await waitFor(() => expect(listBranches.mock.calls.length).toBeGreaterThanOrEqual(2));
    await waitFor(() => expect(screen.getByText("ready", { selector: "[data-branch=dev] *" })).toBeInTheDocument());
    const calls = listBranches.mock.calls.length;
    await new Promise((r) => setTimeout(r, 60));
    expect(listBranches.mock.calls.length).toBe(calls);
  });

  it("shows a load error", async () => {
    const api = fakeApi({
      getDatabase: vi.fn().mockResolvedValue(detail),
      listBranches: vi.fn().mockRejectedValue(new Error("gateway down")),
    });
    render(<DatabaseBranches api={api} database={summary} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("gateway down");
  });
});
