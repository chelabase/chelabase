import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { BranchSummary } from "@cheladb/types";
import { ApiError, type ApiClient } from "@/lib/api";
import { fakeApi } from "@/test/fake-api";
import { NewBranchForm } from "./new-branch-form";

const mk = (id: string, name: string, over: Partial<BranchSummary> = {}): BranchSummary => ({
  id: id as never,
  name,
  parent_branch_id: null,
  kind: "normal",
  status: "ready",
  host: "h",
  port: 4432,
  branched_at_time: null,
  branched_at_lsn: null,
  created_at: "2026-09-30T10:00:00Z",
  history_start: null,
  error: null,
  ...over,
});
const main = mk("m", "main");
const dev = mk("d", "dev", { parent_branch_id: main.id });
const backup = mk("b", "main-before-restore-x", { kind: "backup", parent_branch_id: main.id, host: null });

function setup(api: Partial<ApiClient> = {}, branches = [main, dev, backup]) {
  const onCreated = vi.fn();
  const full = fakeApi(api);
  render(<NewBranchForm api={full} databaseId="db" branches={branches} historyWindow={86400} onCreated={onCreated} />);
  return { api: full, onCreated };
}
const create = () => screen.getByRole("button", { name: "Create branch" });

describe("NewBranchForm", () => {
  it("validates the name as it is typed", async () => {
    setup();
    const name = screen.getByLabelText("Branch name");
    await userEvent.type(name, "Bad Name");
    expect(screen.getByText(/use 1-63 characters/i)).toBeInTheDocument();
    expect(create()).toBeDisabled();
    await userEvent.clear(name);
    await userEvent.type(name, "main");
    expect(screen.getByText(/reserved/i)).toBeInTheDocument();
    await userEvent.clear(name);
    await userEvent.type(name, "feature-1");
    expect(screen.queryByText(/use 1-63 characters/i)).toBeNull();
    expect(create()).toBeEnabled();
  });

  it("defaults the parent to main and offers only ready branches, backups marked", () => {
    const pending = mk("p", "wip", { parent_branch_id: main.id, status: "pending" });
    const failed = mk("f", "broken", { parent_branch_id: main.id, status: "failed" });
    const resetting = mk("r", "busy", { parent_branch_id: main.id, status: "resetting" });
    setup({}, [main, dev, backup, pending, failed, resetting]);
    const select = screen.getByLabelText("Parent") as HTMLSelectElement;
    expect(select.value).toBe("m");
    expect([...select.options].map((o) => o.text)).toEqual(["main", "dev", "main-before-restore-x (backup)"]);
  });

  it("creates a branch from a restore backup", async () => {
    const createBranch = vi.fn().mockResolvedValue(dev);
    setup({ createBranch });
    await userEvent.type(screen.getByLabelText("Branch name"), "from-backup");
    await userEvent.selectOptions(screen.getByLabelText("Parent"), "b");
    await userEvent.click(create());
    expect(createBranch).toHaveBeenCalledWith("db", { name: "from-backup", parent_branch_id: "b" });
  });

  it("creates a branch from main at now", async () => {
    const createBranch = vi.fn().mockResolvedValue(dev);
    const { onCreated } = setup({ createBranch });
    await userEvent.type(screen.getByLabelText("Branch name"), "feature-1");
    await userEvent.click(create());
    expect(createBranch).toHaveBeenCalledWith("db", { name: "feature-1", parent_branch_id: "m" });
    expect(onCreated).toHaveBeenCalled();
  });

  it("creates at a chosen time, sent as UTC", async () => {
    const createBranch = vi.fn().mockResolvedValue(dev);
    setup({ createBranch });
    await userEvent.type(screen.getByLabelText("Branch name"), "old");
    await userEvent.selectOptions(screen.getByLabelText("Parent"), "d");
    await userEvent.click(screen.getByLabelText("At a time"));
    const target = new Date(Date.now() - 30 * 60_000);
    const pad = (n: number) => String(n).padStart(2, "0");
    const local = `${target.getFullYear()}-${pad(target.getMonth() + 1)}-${pad(target.getDate())}T${pad(target.getHours())}:${pad(target.getMinutes())}`;
    await userEvent.type(screen.getByLabelText("Point in time"), local);
    await userEvent.click(create());
    expect(createBranch).toHaveBeenCalledWith("db", {
      name: "old",
      parent_branch_id: "d",
      at: { timestamp: new Date(local).toISOString() },
    });
  });

  const localMinute = (d: Date) => {
    const pad = (n: number) => String(n).padStart(2, "0");
    const up = new Date(Math.ceil(d.getTime() / 60_000) * 60_000);
    return `${up.getFullYear()}-${pad(up.getMonth() + 1)}-${pad(up.getDate())}T${pad(up.getHours())}:${pad(up.getMinutes())}`;
  };
  async function pickerMin(parent: string): Promise<string> {
    await userEvent.selectOptions(screen.getByLabelText("Parent"), parent);
    await userEvent.click(screen.getByLabelText("At a time"));
    return (screen.getByLabelText("Point in time") as HTMLInputElement).min;
  }

  it("bounds the time picker by where the chosen parent's history begins", async () => {
    const start = new Date(Date.now() - 10 * 60_000);
    const young = mk("y", "young", { parent_branch_id: main.id, history_start: start.toISOString() });
    setup({}, [main, young]);
    expect(await pickerMin("y")).toBe(localMinute(start));
  });

  it("lets a backup's older history be chosen, though it was made just now", async () => {
    // Made by a restore a minute ago, holding the old timeline's history from hours back.
    const start = new Date(Date.now() - 5 * 3600_000);
    const bk = mk("b", "main-before-restore-x", {
      kind: "backup",
      parent_branch_id: main.id,
      host: null,
      created_at: new Date(Date.now() - 60_000).toISOString(),
      history_start: start.toISOString(),
    });
    setup({}, [main, bk]);
    expect(await pickerMin("b")).toBe(localMinute(start));
  });

  it("lets a past-point child's history before its creation be chosen", async () => {
    const start = new Date(Date.now() - 3 * 3600_000);
    const child = mk("p", "past", {
      parent_branch_id: main.id,
      created_at: new Date(Date.now() - 60_000).toISOString(),
      history_start: start.toISOString(),
    });
    setup({}, [main, child]);
    expect(await pickerMin("p")).toBe(localMinute(start));
  });

  it("bounds by the window alone when the parent's history start is unknown", async () => {
    const young = mk("y", "young", { parent_branch_id: main.id, created_at: new Date(Date.now() - 60_000).toISOString() });
    setup({}, [main, young]);
    const dayAgo = new Date(Date.now() - 86400_000);
    expect([localMinute(dayAgo), localMinute(new Date(dayAgo.getTime() - 60_000))]).toContain(await pickerMin("y"));
  });

  it("shows the API's refusal", async () => {
    const createBranch = vi.fn().mockRejectedValue(new ApiError(409, "branch limit reached: 5 of 5"));
    setup({ createBranch });
    await userEvent.type(screen.getByLabelText("Branch name"), "x");
    await userEvent.click(create());
    expect(await screen.findByRole("alert")).toHaveTextContent("branch limit reached: 5 of 5");
  });
});
