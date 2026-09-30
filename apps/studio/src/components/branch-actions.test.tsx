import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { BranchSummary } from "@cheladb/types";
import { ApiError, type ApiClient } from "@/lib/api";
import { fakeApi } from "@/test/fake-api";
import { BranchActions } from "./branch-actions";

const mk = (over: Omit<Partial<BranchSummary>, "id"> & { name: string; id: string }): BranchSummary => ({
  parent_branch_id: null,
  kind: "normal",
  status: "ready",
  host: "ep-aaaaaaaaaaaa.localtest.me",
  port: 4432,
  branched_at_time: null,
  branched_at_lsn: null,
  created_at: "2026-09-30T10:00:00Z",
  history_start: null,
  error: null,
  ...over,
  id: over.id as never,
});
const main = mk({ id: "m", name: "main" });
const dev = mk({ id: "d", name: "dev", parent_branch_id: main.id as never });

function setup(branch: BranchSummary, all: BranchSummary[], api: Partial<ApiClient> = {}, window = 7 * 86400) {
  const onChanged = vi.fn();
  const full = fakeApi(api);
  render(
    <BranchActions
      api={full}
      databaseId="db"
      databaseName="web1"
      branch={branch}
      branches={all}
      historyWindow={window}
      onChanged={onChanged}
    />,
  );
  return { api: full, onChanged };
}
const btn = (name: string) => screen.getByRole("button", { name });

describe("BranchActions disabled reasons", () => {
  it("main can't be reset to a parent or deleted", () => {
    setup(main, [main]);
    expect(btn("Reset to parent")).toBeDisabled();
    expect(btn("Reset to parent")).toHaveAttribute("title", "main can't be reset to a parent");
    expect(btn("Delete branch")).toBeDisabled();
    expect(btn("Restore")).toBeEnabled();
  });

  it("names the child branches that block an action", () => {
    const kid = mk({ id: "k", name: "kid", parent_branch_id: dev.id as never });
    setup(dev, [main, dev, kid]);
    for (const name of ["Reset to parent", "Restore", "Delete branch"]) {
      expect(btn(name)).toBeDisabled();
      expect(btn(name)).toHaveAttribute("title", "has child branches: kid");
    }
  });

  it("a restore backup can't connect", () => {
    const backup = mk({ id: "b", name: "main-before-restore-x", kind: "backup", parent_branch_id: main.id as never, host: null });
    setup(backup, [main, backup]);
    expect(btn("Connection details")).toBeDisabled();
    expect(btn("Connection details")).toHaveAttribute("title", "restore backup: create a branch from it to connect");
    expect(btn("Delete branch")).toBeEnabled();
  });
});

describe("BranchActions disabled reasons, visible and linked", () => {
  const backup = mk({ id: "b", name: "main-before-restore-x", kind: "backup", parent_branch_id: main.id as never, host: null });
  const cases: [string, BranchSummary, BranchSummary[], [string, string][]][] = [
    [
      "main",
      main,
      [main],
      [
        ["Reset to parent", "main can't be reset to a parent"],
        ["Delete branch", "main can't be deleted; delete the database instead"],
      ],
    ],
    [
      "a backup",
      backup,
      [main, backup],
      [
        ["Connection details", "restore backup: create a branch from it to connect"],
        ["Reset password", "restore backup: create a branch from it to connect"],
        ["Reset to parent", "a restore backup can't be rewritten; create a branch from it"],
        ["Restore", "a restore backup can't be rewritten; create a branch from it"],
      ],
    ],
    [
      "a parent",
      dev,
      [main, dev, mk({ id: "k", name: "kid", parent_branch_id: dev.id as never })],
      [
        ["Reset to parent", "has child branches: kid"],
        ["Restore", "has child branches: kid"],
        ["Delete branch", "has child branches: kid"],
      ],
    ],
    [
      "a branch that is not ready",
      { ...dev, status: "resetting" },
      [main, { ...dev, status: "resetting" }],
      [
        ["Connection details", "branch is not ready (it is resetting)"],
        ["Reset to parent", "branch is not ready (it is resetting)"],
      ],
    ],
  ];
  for (const [what, branch, all, expected] of cases) {
    it(`shows why each action is off for ${what}`, () => {
      setup(branch, all);
      for (const [name, reason] of expected) {
        expect(btn(name)).toBeDisabled();
        expect(btn(name)).toHaveAccessibleDescription(reason);
        const [id] = (btn(name).getAttribute("aria-describedby") ?? "").split(" ");
        const text = document.getElementById(id);
        expect(text, name).toBeVisible();
        expect(text).toHaveTextContent(reason);
      }
      // Each distinct reason is written once.
      for (const reason of new Set(expected.map(([, r]) => r))) {
        expect(screen.getAllByText(reason)).toHaveLength(1);
      }
    });
  }

  it("shows no reason for an action that is available", () => {
    setup(dev, [main, dev]);
    for (const name of ["Connection details", "Reset password", "Reset to parent", "Restore", "Delete branch"]) {
      expect(btn(name)).toBeEnabled();
      expect(btn(name)).not.toHaveAttribute("aria-describedby");
    }
  });
});

describe("BranchActions", () => {
  it("reveals a branch's connection through the branch endpoint", async () => {
    const getBranchConnection = vi.fn().mockResolvedValue({
      host: "ep-aaaaaaaaaaaa.localtest.me",
      port: 4432,
      database: "web1",
      user: "postgres",
      password: "pw",
      uri: "postgresql://postgres:pw@ep-aaaaaaaaaaaa.localtest.me:4432/web1",
      http_url: "https://ep-aaaaaaaaaaaa.localtest.me:4444/sql",
    });
    setup(dev, [main, dev], { getBranchConnection });
    await userEvent.click(btn("Connection details"));
    expect(await screen.findByText("postgres")).toBeInTheDocument();
    expect(getBranchConnection).toHaveBeenCalledWith("db", "d");
  });

  it("resets the branch's password", async () => {
    const resetBranchPassword = vi.fn().mockResolvedValue(undefined);
    const getBranchConnection = vi.fn().mockRejectedValue(new Error("nope"));
    setup(dev, [main, dev], { resetBranchPassword, getBranchConnection });
    await userEvent.click(btn("Reset password"));
    await userEvent.click(btn("Confirm reset"));
    expect(resetBranchPassword).toHaveBeenCalledWith("db", "d");
  });

  it("resets to parent after a confirm", async () => {
    const resetBranchToParent = vi.fn().mockResolvedValue(undefined);
    const { onChanged } = setup(dev, [main, dev], { resetBranchToParent });
    await userEvent.click(btn("Reset to parent"));
    expect(resetBranchToParent).not.toHaveBeenCalled();
    await userEvent.click(btn("Confirm reset to parent"));
    expect(resetBranchToParent).toHaveBeenCalledWith("db", "d");
    expect(onChanged).toHaveBeenCalled();
  });

  it("deletes after a confirm", async () => {
    const deleteBranch = vi.fn().mockResolvedValue(undefined);
    const { onChanged } = setup(dev, [main, dev], { deleteBranch });
    await userEvent.click(btn("Delete branch"));
    await userEvent.click(btn("Confirm delete branch"));
    expect(deleteBranch).toHaveBeenCalledWith("db", "d");
    expect(onChanged).toHaveBeenCalled();
  });

  it("shows the API's message when an action is refused", async () => {
    const deleteBranch = vi.fn().mockRejectedValue(new ApiError(409, "branch is already being deleted"));
    setup(dev, [main, dev], { deleteBranch });
    await userEvent.click(btn("Delete branch"));
    await userEvent.click(btn("Confirm delete branch"));
    expect(await screen.findByRole("alert")).toHaveTextContent("branch is already being deleted");
  });

  describe("restore picker", () => {
    it("is bounded from now minus the history window to now", async () => {
      setup(main, [main], {}, 3600);
      await userEvent.click(btn("Restore"));
      const input = screen.getByLabelText("Restore to") as HTMLInputElement;
      expect(input.type).toBe("datetime-local");
      const bound = (d: Date) =>
        `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}T${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
      const now = new Date();
      // allow the minute to tick while the test runs
      expect([bound(now), bound(new Date(now.getTime() - 60_000))]).toContain(input.max);
      const hourAgo = new Date(now.getTime() - 3600_000 + 60_000);
      expect([bound(hourAgo), bound(new Date(hourAgo.getTime() - 60_000))]).toContain(input.min);
    });

    const localMinute = (d: Date) => {
      const pad = (n: number) => String(n).padStart(2, "0");
      const up = new Date(Math.ceil(d.getTime() / 60_000) * 60_000);
      return `${up.getFullYear()}-${pad(up.getMonth() + 1)}-${pad(up.getDate())}T${pad(up.getHours())}:${pad(up.getMinutes())}`;
    };

    it("starts no earlier than the branch's history begins", async () => {
      const start = new Date(Date.now() - 10 * 60_000);
      setup(mk({ id: "y", name: "young", history_start: start.toISOString() }), [main], {}, 7 * 86400);
      await userEvent.click(btn("Restore"));
      const input = screen.getByLabelText("Restore to") as HTMLInputElement;
      expect(input.min).toBe(localMinute(start));
      expect(screen.getByText(/since its history begins/)).toBeInTheDocument();
    });

    it("reaches back before the creation of a child made at a past point", async () => {
      // Created a minute ago at a point two days back: its history starts two days back.
      const start = new Date(Date.now() - 2 * 86400_000);
      const child = mk({
        id: "p",
        name: "past",
        parent_branch_id: main.id as never,
        created_at: new Date(Date.now() - 60_000).toISOString(),
        history_start: start.toISOString(),
      });
      setup(child, [main, child], {}, 7 * 86400);
      await userEvent.click(btn("Restore"));
      expect((screen.getByLabelText("Restore to") as HTMLInputElement).min).toBe(localMinute(start));
    });

    it("uses only the window when the history's start is unknown", async () => {
      const young = mk({ id: "y", name: "young", created_at: new Date(Date.now() - 60_000).toISOString() });
      setup(young, [main], {}, 3600);
      await userEvent.click(btn("Restore"));
      const input = screen.getByLabelText("Restore to") as HTMLInputElement;
      const hourAgo = new Date(Date.now() - 3600_000);
      expect([localMinute(hourAgo), localMinute(new Date(hourAgo.getTime() - 60_000))]).toContain(input.min);
    });

    it("sends the chosen time as a UTC timestamp and shows the backup", async () => {
      const restoreBranch = vi
        .fn()
        .mockResolvedValue({ status: "resetting", backup_branch_id: "bk", backup_name: "main-before-restore-20260930T101000Z" });
      const { onChanged } = setup(main, [main], { restoreBranch });
      await userEvent.click(btn("Restore"));
      const target = new Date(Date.now() - 5 * 60_000);
      const pad = (n: number) => String(n).padStart(2, "0");
      const local = `${target.getFullYear()}-${pad(target.getMonth() + 1)}-${pad(target.getDate())}T${pad(target.getHours())}:${pad(target.getMinutes())}`;
      const input = screen.getByLabelText("Restore to");
      await userEvent.clear(input);
      await userEvent.type(input, local);
      await userEvent.click(btn("Confirm restore"));
      const [db, id, req] = restoreBranch.mock.calls[0];
      expect([db, id]).toEqual(["db", "m"]);
      expect(req.at.timestamp).toBe(new Date(local).toISOString());
      expect(await screen.findByRole("status")).toHaveTextContent("main-before-restore-20260930T101000Z");
      expect(onChanged).toHaveBeenCalled();
    });

    it("drops the restore notice once a refresh shows the restore finished", async () => {
      const restoreBranch = vi
        .fn()
        .mockResolvedValue({ status: "resetting", backup_branch_id: "bk", backup_name: "main-before-restore-20260930T101000Z" });
      const full = fakeApi({ restoreBranch });
      const props = { api: full, databaseId: "db", databaseName: "web1", historyWindow: 7 * 86400, onChanged: vi.fn() };
      const { rerender } = render(<BranchActions {...props} branch={main} branches={[main]} />);
      await userEvent.click(btn("Restore"));
      const target = new Date(Date.now() - 5 * 60_000);
      const pad = (n: number) => String(n).padStart(2, "0");
      await userEvent.type(
        screen.getByLabelText("Restore to"),
        `${target.getFullYear()}-${pad(target.getMonth() + 1)}-${pad(target.getDate())}T${pad(target.getHours())}:${pad(target.getMinutes())}`,
      );
      await userEvent.click(btn("Confirm restore"));
      expect(await screen.findByRole("status")).toHaveTextContent("main-before-restore-20260930T101000Z");
      // The refresh right after: still restoring, so the notice stays.
      const resetting = { ...main, status: "resetting" as const };
      rerender(<BranchActions {...props} branch={resetting} branches={[resetting]} />);
      expect(screen.getByRole("status")).toBeInTheDocument();
      rerender(<BranchActions {...props} branch={main} branches={[main]} />);
      expect(screen.queryByRole("status")).toBeNull();
    });

    it("refuses a time outside the window without calling the API", async () => {
      const restoreBranch = vi.fn();
      setup(main, [main], { restoreBranch }, 3600);
      await userEvent.click(btn("Restore"));
      const input = screen.getByLabelText("Restore to");
      await userEvent.clear(input);
      await userEvent.type(input, "2020-01-01T00:00");
      await userEvent.click(btn("Confirm restore"));
      expect(await screen.findByRole("alert")).toHaveTextContent(/history window/i);
      expect(restoreBranch).not.toHaveBeenCalled();
    });
  });
});
