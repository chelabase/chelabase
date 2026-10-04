import { screen } from "@testing-library/react";
import { renderWithQuery } from "@/test/render";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { getGetDatabaseQueryKey, getListBranchesQueryKey, type Branch as BranchSummary } from "@chelabase/management-client";
import { BranchActions } from "./branch-actions";

const mk = (over: Partial<BranchSummary> & { name: string; id: string }): BranchSummary => ({
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
  protected: false,
  main_database_missing: false,
  ...over,
});
const main = mk({ id: "m", name: "main" });
const dev = mk({ id: "d", name: "dev", parent_branch_id: main.id });

const accepted: FakeHandler = () => jsonResponse({ status: "resetting" }, 202);
const restored: FakeHandler = () =>
  jsonResponse({ status: "resetting", backup_branch_id: "bk", backup_name: "main-before-restore-20260930T101000Z" }, 202);

function setup(
  branch: BranchSummary,
  all: BranchSummary[],
  routes: Record<string, FakeHandler> = {},
  window = 7 * 86400,
) {
  const f = fakeFetch(routes);
  const result = renderWithQuery(
    <BranchActions
      databaseId="db"
      databaseName="web1"
      branch={branch}
      branches={all}
      historyWindow={window}
    />,
    { fetch: f },
  );
  return { ...result, fetch: f };
}
const btn = (name: string) => screen.getByRole("button", { name });
const sent = (f: FakeFetch) => f.calls.map((r) => `${r.method} ${new URL(r.url).pathname}`);
const pad = (n: number) => String(n).padStart(2, "0");
const local = (d: Date) =>
  `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;

describe("BranchActions disabled reasons", () => {
  it("main can't be reset to a parent or deleted", () => {
    setup(main, [main]);
    expect(btn("Reset to parent")).toBeDisabled();
    expect(btn("Reset to parent")).toHaveAttribute("title", "main can't be reset to a parent");
    expect(btn("Delete branch")).toBeDisabled();
    expect(btn("Restore")).toBeEnabled();
  });

  it("names the child branches that block an action", () => {
    const kid = mk({ id: "k", name: "kid", parent_branch_id: dev.id });
    setup(dev, [main, dev, kid]);
    for (const name of ["Reset to parent", "Restore", "Delete branch"]) {
      expect(btn(name)).toBeDisabled();
      expect(btn(name)).toHaveAttribute("title", "has child branches: kid");
    }
  });

  it("a restore backup can't connect", () => {
    const backup = mk({ id: "b", name: "main-before-restore-x", kind: "backup", parent_branch_id: main.id, host: null });
    setup(backup, [main, backup]);
    expect(btn("Connection details")).toBeDisabled();
    expect(btn("Connection details")).toHaveAttribute("title", "restore backup: create a branch from it to connect");
    expect(btn("Delete branch")).toBeEnabled();
  });
});

describe("BranchActions disabled reasons, visible and linked", () => {
  const backup = mk({ id: "b", name: "main-before-restore-x", kind: "backup", parent_branch_id: main.id, host: null });
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
      [main, dev, mk({ id: "k", name: "kid", parent_branch_id: dev.id })],
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
  // Connection details and the password reset go through DatabaseActions, already on the management client.
  it("reveals a branch's connection through the branch endpoint", async () => {
    const f = fakeFetch({
      "GET /v1/databases/db/branches/d/connection": () =>
        jsonResponse({
          host: "ep-aaaaaaaaaaaa.localtest.me",
          port: 4432,
          database: "web1",
          user: "postgres",
          password: "pw",
          password_state: "generated",
          uri: "postgresql://postgres:pw@ep-aaaaaaaaaaaa.localtest.me:4432/web1",
          http_url: "https://ep-aaaaaaaaaaaa.localtest.me:4444/sql",
        }),
    });
    renderWithQuery(
      <BranchActions databaseId="db" databaseName="web1" branch={dev} branches={[main, dev]} historyWindow={86400} />,
      { fetch: f },
    );
    await userEvent.click(btn("Connection details"));
    expect(await screen.findByText("postgres")).toBeInTheDocument();
    // The connection panel also lists the branch's roles for its picker.
    expect(f.calls.map((r) => new URL(r.url).pathname).sort()).toEqual([
      "/v1/databases/db/branches/d/connection",
      "/v1/databases/db/branches/d/databases",
      "/v1/databases/db/branches/d/roles",
    ]);
  });

  it("resets the branch's password", async () => {
    const f = fakeFetch({ "POST /v1/databases/db/branches/d/reset-password": () => new Response(null, { status: 204 }) });
    renderWithQuery(
      <BranchActions databaseId="db" databaseName="web1" branch={dev} branches={[main, dev]} historyWindow={86400} />,
      { fetch: f },
    );
    await userEvent.click(btn("Reset password"));
    await userEvent.click(btn("Confirm reset"));
    await vi.waitFor(() =>
      expect(f.calls.map((r) => `${r.method} ${new URL(r.url).pathname}`)).toContain(
        "POST /v1/databases/db/branches/d/reset-password",
      ),
    );
  });

  it("resets to parent after a confirm", async () => {
    const { fetch } = setup(dev, [main, dev], { "POST /v1/databases/db/branches/d/reset-to-parent": accepted });
    await userEvent.click(btn("Reset to parent"));
    expect(fetch.calls).toHaveLength(0);
    await userEvent.click(btn("Confirm reset to parent"));
    await vi.waitFor(() => expect(sent(fetch)).toEqual(["POST /v1/databases/db/branches/d/reset-to-parent"]));
    await vi.waitFor(() => expect(screen.queryByRole("button", { name: "Confirm reset to parent" })).toBeNull());
  });

  it("deletes after a confirm", async () => {
    const { fetch } = setup(dev, [main, dev], { "DELETE /v1/databases/db/branches/d": accepted });
    await userEvent.click(btn("Delete branch"));
    await userEvent.click(btn("Confirm delete branch"));
    await vi.waitFor(() => expect(sent(fetch)).toEqual(["DELETE /v1/databases/db/branches/d"]));
    await vi.waitFor(() => expect(screen.queryByRole("button", { name: "Confirm delete branch" })).toBeNull());
  });

  it("shows the API's message when an action is refused", async () => {
    setup(dev, [main, dev], {
      "DELETE /v1/databases/db/branches/d": () =>
        jsonResponse({ error: { code: "already_deleting", message: "branch is already being deleted" } }, 409),
    });
    await userEvent.click(btn("Delete branch"));
    await userEvent.click(btn("Confirm delete branch"));
    expect(await screen.findByRole("alert")).toHaveTextContent("branch is already being deleted");
  });

  it("disables the confirm while an action runs", async () => {
    setup(dev, [main, dev], { "DELETE /v1/databases/db/branches/d": () => new Promise<never>(() => {}) });
    await userEvent.click(btn("Delete branch"));
    await userEvent.click(btn("Confirm delete branch"));
    expect(btn("Confirm delete branch")).toBeDisabled();
  });

  const actions: [string, string, string, FakeHandler, (b: BranchSummary) => Promise<void>][] = [
    ["reset to parent", "POST /v1/databases/db/branches/d/reset-to-parent", "Reset to parent", accepted, async () => {
      await userEvent.click(btn("Confirm reset to parent"));
    }],
    ["restore", "POST /v1/databases/db/branches/d/restore", "Restore", restored, async () => {
      await userEvent.type(screen.getByLabelText("Restore to"), local(new Date(Date.now() - 5 * 60_000)));
      await userEvent.click(btn("Confirm restore"));
    }],
    ["delete", "DELETE /v1/databases/db/branches/d", "Delete branch", accepted, async () => {
      await userEvent.click(btn("Confirm delete branch"));
    }],
  ];
  for (const [what, route, open, answer, confirm] of actions) {
    it(`branch_mutations_invalidate_list_and_database: ${what}`, async () => {
      const { fetch, queryClient } = setup(dev, [main, dev], { [route]: answer });
      queryClient.setQueryData(getListBranchesQueryKey("db"), [main, dev]);
      queryClient.setQueryData(getGetDatabaseQueryKey("db"), { name: "web1" });
      queryClient.setQueryData(getListBranchesQueryKey("other"), [main]);
      await userEvent.click(btn(open));
      await confirm(dev);
      await vi.waitFor(() => expect(sent(fetch)).toContain(route));
      await vi.waitFor(() => {
        expect(queryClient.getQueryState(getListBranchesQueryKey("db"))?.isInvalidated).toBe(true);
        expect(queryClient.getQueryState(getGetDatabaseQueryKey("db"))?.isInvalidated).toBe(true);
      });
      expect(queryClient.getQueryState(getListBranchesQueryKey("other"))?.isInvalidated).toBe(false);
    });
  }

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
        parent_branch_id: main.id,
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
      const { fetch } = setup(main, [main], { "POST /v1/databases/db/branches/m/restore": restored });
      await userEvent.click(btn("Restore"));
      const at = local(new Date(Date.now() - 5 * 60_000));
      const input = screen.getByLabelText("Restore to");
      await userEvent.clear(input);
      await userEvent.type(input, at);
      await userEvent.click(btn("Confirm restore"));
      await vi.waitFor(() => expect(sent(fetch)).toEqual(["POST /v1/databases/db/branches/m/restore"]));
      expect(await fetch.calls[0].clone().json()).toEqual({ at: { timestamp: new Date(at).toISOString() } });
      expect(await screen.findByRole("status")).toHaveTextContent("main-before-restore-20260930T101000Z");
    });

    it("drops the restore notice once a refresh shows the restore finished", async () => {
      const props = { databaseId: "db", databaseName: "web1", historyWindow: 7 * 86400 };
      const { rerender } = renderWithQuery(<BranchActions {...props} branch={main} branches={[main]} />, {
        fetch: fakeFetch({ "POST /v1/databases/db/branches/m/restore": restored }),
      });
      await userEvent.click(btn("Restore"));
      await userEvent.type(screen.getByLabelText("Restore to"), local(new Date(Date.now() - 5 * 60_000)));
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
      const { fetch } = setup(main, [main], {}, 3600);
      await userEvent.click(btn("Restore"));
      const input = screen.getByLabelText("Restore to");
      await userEvent.clear(input);
      await userEvent.type(input, "2020-01-01T00:00");
      await userEvent.click(btn("Confirm restore"));
      expect(await screen.findByRole("alert")).toHaveTextContent(/history window/i);
      expect(fetch.calls).toHaveLength(0);
    });
  });
});

describe("BranchActions roles", () => {
  it("toggles the Roles panel on a ready branch", async () => {
    setup(main, [main], { "GET /v1/databases/db/branches/m/roles": () => jsonResponse([]) });
    await userEvent.click(btn("Roles"));
    expect(await screen.findByRole("heading", { name: "Roles" })).toBeInTheDocument();
    await userEvent.click(btn("Roles"));
    expect(screen.queryByRole("heading", { name: "Roles" })).toBeNull();
  });

  it("disables Roles when the branch isn't ready", () => {
    setup({ ...main, status: "provisioning" }, [main]);
    expect(btn("Roles")).toBeDisabled();
  });
});

describe("BranchActions protection", () => {
  const PATCH = "PATCH /v1/databases/db/branches/d";

  it("main_database_missing_notice", () => {
    setup({ ...main, main_database_missing: true }, [main]);
    expect(screen.getByRole("status")).toHaveTextContent(
      "Main database missing: create a database named web1 to bring it back",
    );
  });

  it("no notice when the main database is there", () => {
    setup(main, [main]);
    expect(screen.queryByText(/Main database missing/)).toBeNull();
  });

  it("protect_toggle_confirms_and_patches", async () => {
    const patch = vi.fn(() => jsonResponse({ ...dev, protected: true }));
    const { fetch } = setup(dev, [main, dev], { [PATCH]: patch });
    await userEvent.click(btn("Protect"));
    expect(
      screen.getByText(
        "Children of a protected branch get new passwords for every role. It can't be deleted or reset to its parent until you unprotect it. Restore stays allowed.",
      ),
    ).toBeInTheDocument();
    expect(patch).not.toHaveBeenCalled();
    await userEvent.click(btn("Confirm protect"));
    await vi.waitFor(() => expect(patch).toHaveBeenCalled());
    const call = fetch.calls.find((r) => r.method === "PATCH")!;
    expect(await call.clone().json()).toEqual({ protected: true });
  });

  it("unprotect patches without a confirm", async () => {
    const patch = vi.fn(() => jsonResponse({ ...dev, protected: false }));
    const { fetch } = setup({ ...dev, protected: true }, [main, { ...dev, protected: true }], { [PATCH]: patch });
    await userEvent.click(btn("Unprotect"));
    await vi.waitFor(() => expect(patch).toHaveBeenCalled());
    const call = fetch.calls.find((r) => r.method === "PATCH")!;
    expect(await call.clone().json()).toEqual({ protected: false });
  });

  it("protected_branch_disables_delete_and_reset", () => {
    const p = { ...dev, protected: true };
    setup(p, [main, p]);
    for (const name of ["Reset to parent", "Delete branch"]) {
      expect(btn(name)).toBeDisabled();
      expect(btn(name)).toHaveAttribute("title", "Unprotect the branch first");
    }
    expect(screen.getAllByText("Unprotect the branch first").length).toBeGreaterThan(0);
    expect(btn("Restore")).toBeEnabled();
  });

  it("a backup has no Protect toggle", () => {
    const backup = mk({ id: "b", name: "main-before-restore-x", kind: "backup", parent_branch_id: main.id, host: null });
    setup(backup, [main, backup]);
    expect(screen.queryByRole("button", { name: "Protect" })).toBeNull();
  });
});
