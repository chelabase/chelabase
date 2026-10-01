import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Branch as BranchSummary } from "@cheladb/management-client";
// The actions in each row (DatabaseActions, the branch mutations) need a QueryClient.
import { renderWithQuery } from "@/test/render";
import { BranchTree } from "./branch-tree";

const rowOf = (name: string) => document.querySelector(`tr[data-branch="${name}"]`) as HTMLElement;

const base = {
  kind: "normal" as const,
  status: "ready" as const,
  port: 4432,
  branched_at_time: null,
  branched_at_lsn: null,
  history_start: null,
  error: null,
};
const main: BranchSummary = {
  ...base,
  id: "m",
  name: "main",
  parent_branch_id: null,
  host: "ep-aaaaaaaaaaaa.localtest.me",
  created_at: "2026-09-30T10:00:00Z",
};
const dev: BranchSummary = {
  ...base,
  id: "d",
  name: "dev",
  parent_branch_id: main.id,
  host: "ep-bbbbbbbbbbbb.localtest.me",
  branched_at_time: "2026-09-30T09:00:00Z",
  branched_at_lsn: "0/16B3748",
  created_at: "2026-09-30T10:01:00Z",
};
const feat: BranchSummary = { ...dev, id: "f", name: "feat", parent_branch_id: dev.id, branched_at_time: null, created_at: "2026-09-30T10:02:00Z" };
const backup: BranchSummary = {
  ...base,
  id: "b",
  name: "main-before-restore-20260930T101000Z",
  kind: "backup",
  parent_branch_id: main.id,
  host: null,
  port: null,
  created_at: "2026-09-30T10:03:00Z",
};

function renderTree(branches: BranchSummary[]) {
  return renderWithQuery(
    <BranchTree databaseId="db" databaseName="web1" branches={branches} historyWindow={604800} />,
  );
}

describe("BranchTree", () => {
  it("lists main first and indents children by depth", () => {
    renderTree([feat, backup, dev, main]);
    const rows = screen.getAllByRole("row").slice(1);
    expect(rows.map((r) => r.getAttribute("data-branch"))).toEqual([
      "main",
      "dev",
      "feat",
      "main-before-restore-20260930T101000Z",
    ]);
    expect(rows.map((r) => r.getAttribute("data-depth"))).toEqual(["0", "1", "2", "1"]);
  });

  it("shows status, host and where each branch came from", () => {
    renderTree([main, dev, feat]);
    const devRow = rowOf("dev");
    expect(within(devRow).getByText("ready")).toBeInTheDocument();
    expect(within(devRow).getByText("ep-bbbbbbbbbbbb.localtest.me:4432")).toBeInTheDocument();
    expect(within(devRow).getByText(/from main at 2026-09-30/)).toBeInTheDocument();
    // no requested time: the LSN
    expect(within(rowOf("feat")).getByText(/from dev at 0\/16B3748/)).toBeInTheDocument();
  });

  it("marks backups", () => {
    renderTree([main, backup]);
    expect(within(rowOf("main-before-restore-20260930T101000Z")).getByText("backup")).toBeInTheDocument();
  });

  it("shows a failed branch's error in red", () => {
    renderTree([main, { ...dev, status: "failed", error: "creating timeline: boom" }]);
    expect(screen.getByText("creating timeline: boom")).toHaveClass("text-destructive");
  });

  it("shows a ready branch's last error as a muted notice, prefixed neutrally", () => {
    // A failed restore, a refused delete, an interrupted attempt: each shown as it was recorded.
    for (const error of [
      "resolving the point: no data at that time",
      "cannot delete dev: it has child branches (feat); delete them first",
      "The last reset or restore was interrupted before it switched timelines; the branch is unchanged. Try again.",
    ]) {
      const { unmount } = renderTree([main, { ...dev, error }]);
      const notice = within(rowOf("dev")).getByText(`Last attempt: ${error}`);
      expect(notice).toHaveClass("text-muted-foreground");
      expect(notice).not.toHaveClass("text-destructive");
      unmount();
    }
  });
});
