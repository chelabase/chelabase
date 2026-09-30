import { render, screen, within } from "@testing-library/react";

const rowOf = (name: string) => document.querySelector(`tr[data-branch="${name}"]`) as HTMLElement;
import { describe, expect, it } from "vitest";
import type { BranchSummary } from "@cheladb/types";
import { fakeApi } from "@/test/fake-api";
import { BranchTree } from "./branch-tree";

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
  id: "m" as never,
  name: "main",
  parent_branch_id: null,
  host: "ep-aaaaaaaaaaaa.localtest.me",
  created_at: "2026-09-30T10:00:00Z",
};
const dev: BranchSummary = {
  ...base,
  id: "d" as never,
  name: "dev",
  parent_branch_id: main.id,
  host: "ep-bbbbbbbbbbbb.localtest.me",
  branched_at_time: "2026-09-30T09:00:00Z",
  branched_at_lsn: "0/16B3748",
  created_at: "2026-09-30T10:01:00Z",
};
const feat: BranchSummary = { ...dev, id: "f" as never, name: "feat", parent_branch_id: dev.id, branched_at_time: null, created_at: "2026-09-30T10:02:00Z" };
const backup: BranchSummary = {
  ...base,
  id: "b" as never,
  name: "main-before-restore-20260930T101000Z",
  kind: "backup",
  parent_branch_id: main.id,
  host: null,
  port: null,
  created_at: "2026-09-30T10:03:00Z",
};

function renderTree(branches: BranchSummary[]) {
  return render(
    <BranchTree
      api={fakeApi()}
      databaseId="db"
      databaseName="web1"
      branches={branches}
      historyWindow={604800}
      onChanged={() => {}}
    />,
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
