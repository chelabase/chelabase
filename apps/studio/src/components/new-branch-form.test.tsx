import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { getGetDatabaseQueryKey, getListBranchesQueryKey, type Branch } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { NewBranchForm } from "./new-branch-form";

const mk = (id: string, name: string, over: Partial<Branch> = {}): Branch => ({
  id,
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
  protected: false,
  main_database_missing: false,
  ...over,
});
const main = mk("m", "main");
const dev = mk("d", "dev", { parent_branch_id: main.id });
const backup = mk("b", "main-before-restore-x", { kind: "backup", parent_branch_id: main.id, host: null });

const CREATE = "POST /v1/databases/db/branches";
const accepted: FakeHandler = () => jsonResponse(dev, 202);

function setup(answer: FakeHandler = accepted, branches = [main, dev, backup]) {
  const f = fakeFetch({ [CREATE]: answer });
  const result = renderWithQuery(<NewBranchForm databaseId="db" branches={branches} historyWindow={86400} />, { fetch: f });
  return { ...result, fetch: f };
}
const create = () => screen.getByRole("button", { name: "Create branch" });
const creates = (f: FakeFetch) => f.calls.filter((r) => `${r.method} ${new URL(r.url).pathname}` === CREATE);
/** The body of the one create request, once it has been sent. */
async function sentBody(f: FakeFetch): Promise<unknown> {
  await vi.waitFor(() => expect(creates(f)).toHaveLength(1));
  return creates(f)[0].clone().json();
}

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
    setup(accepted, [main, dev, backup, pending, failed, resetting]);
    const select = screen.getByLabelText("Parent") as HTMLSelectElement;
    expect(select.value).toBe("m");
    expect([...select.options].map((o) => o.text)).toEqual(["main", "dev", "main-before-restore-x (backup)"]);
  });

  it("creates a branch from a restore backup", async () => {
    const { fetch } = setup();
    await userEvent.type(screen.getByLabelText("Branch name"), "from-backup");
    await userEvent.selectOptions(screen.getByLabelText("Parent"), "b");
    await userEvent.click(create());
    expect(await sentBody(fetch)).toEqual({ name: "from-backup", parent_branch_id: "b" });
  });

  it("creates a branch from main at now, then clears the name", async () => {
    const { fetch } = setup();
    await userEvent.type(screen.getByLabelText("Branch name"), "feature-1");
    await userEvent.click(create());
    expect(await sentBody(fetch)).toEqual({ name: "feature-1", parent_branch_id: "m" });
    await vi.waitFor(() => expect(screen.getByLabelText("Branch name")).toHaveValue(""));
  });

  it("branch_mutations_invalidate_list_and_database: create", async () => {
    const { fetch, queryClient } = setup();
    queryClient.setQueryData(getListBranchesQueryKey("db"), [main, dev]);
    queryClient.setQueryData(getGetDatabaseQueryKey("db"), { name: "web1" });
    queryClient.setQueryData(getListBranchesQueryKey("other"), [main]);
    await userEvent.type(screen.getByLabelText("Branch name"), "feature-1");
    await userEvent.click(create());
    await sentBody(fetch);
    await vi.waitFor(() => {
      expect(queryClient.getQueryState(getListBranchesQueryKey("db"))?.isInvalidated).toBe(true);
      expect(queryClient.getQueryState(getGetDatabaseQueryKey("db"))?.isInvalidated).toBe(true);
    });
    expect(queryClient.getQueryState(getListBranchesQueryKey("other"))?.isInvalidated).toBe(false);
  });

  it("creates at a chosen time, sent as UTC", async () => {
    const { fetch } = setup();
    await userEvent.type(screen.getByLabelText("Branch name"), "old");
    await userEvent.selectOptions(screen.getByLabelText("Parent"), "d");
    await userEvent.click(screen.getByLabelText("At a time"));
    const target = new Date(Date.now() - 30 * 60_000);
    const pad = (n: number) => String(n).padStart(2, "0");
    const local = `${target.getFullYear()}-${pad(target.getMonth() + 1)}-${pad(target.getDate())}T${pad(target.getHours())}:${pad(target.getMinutes())}`;
    await userEvent.type(screen.getByLabelText("Point in time"), local);
    await userEvent.click(create());
    expect(await sentBody(fetch)).toEqual({
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
    setup(accepted, [main, young]);
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
    setup(accepted, [main, bk]);
    expect(await pickerMin("b")).toBe(localMinute(start));
  });

  it("lets a past-point child's history before its creation be chosen", async () => {
    const start = new Date(Date.now() - 3 * 3600_000);
    const child = mk("p", "past", {
      parent_branch_id: main.id,
      created_at: new Date(Date.now() - 60_000).toISOString(),
      history_start: start.toISOString(),
    });
    setup(accepted, [main, child]);
    expect(await pickerMin("p")).toBe(localMinute(start));
  });

  it("bounds by the window alone when the parent's history start is unknown", async () => {
    const young = mk("y", "young", { parent_branch_id: main.id, created_at: new Date(Date.now() - 60_000).toISOString() });
    setup(accepted, [main, young]);
    const dayAgo = new Date(Date.now() - 86400_000);
    expect([localMinute(dayAgo), localMinute(new Date(dayAgo.getTime() - 60_000))]).toContain(await pickerMin("y"));
  });

  it("shows the API's refusal and keeps the name", async () => {
    setup(() => jsonResponse({ error: { code: "branch_cap_reached", message: "branch limit reached: 5 of 5" } }, 409));
    await userEvent.type(screen.getByLabelText("Branch name"), "x");
    await userEvent.click(create());
    expect(await screen.findByRole("alert")).toHaveTextContent("branch limit reached: 5 of 5");
    expect(screen.getByLabelText("Branch name")).toHaveValue("x");
  });

  it("disables Create while the request runs", async () => {
    setup(() => new Promise<never>(() => {}));
    await userEvent.type(screen.getByLabelText("Branch name"), "x");
    await userEvent.click(create());
    expect(create()).toBeDisabled();
  });
});
