import { screen } from "@testing-library/react";
import { renderWithQuery } from "@/test/render";
import { renderWithRouter } from "@/test/render-router";
import { fakeFetch, jsonResponse, sequence, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { summaryOf } from "@/test/merge-fixtures";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import {
  getGetDatabaseQueryKey,
  getListBranchesQueryKey,
  type Branch as BranchSummary,
  type MergeSummary,
} from "@chelabase/management-client";
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
  guard_logins: true,
  review_acknowledged_hash: null,
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
      projectId="p1"
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
      <BranchActions projectId="p1" databaseId="db" databaseName="web1" branch={dev} branches={[main, dev]} historyWindow={86400} />,
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
      <BranchActions projectId="p1" databaseId="db" databaseName="web1" branch={dev} branches={[main, dev]} historyWindow={86400} />,
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
      const props = { projectId: "p1", databaseId: "db", databaseName: "web1", historyWindow: 7 * 86400 };
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
    // The panel links into the Members hub, so it needs a router.
    renderWithRouter(
      <BranchActions projectId="p1" databaseId="db" databaseName="web1" branch={main} branches={[main]} historyWindow={7 * 86400} />,
      { fetch: fakeFetch({ "GET /v1/databases/db/branches/m/roles": () => jsonResponse([]) }) },
    );
    await userEvent.click(await screen.findByRole("button", { name: "Roles" }));
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
    const { fetch } = setup(dev, [main, dev], {
      [PATCH]: patch,
      ["GET /v1/databases/db/branches/d/review"]: () => jsonResponse({ items: [], hash: "e0", acknowledged_hash: null }),
    });
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
    expect(await call.clone().json()).toEqual({ protected: true, review_hash: "e0" });
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

describe("BranchActions merges", () => {
  const MERGES = "POST /v1/databases/db/branches/d/merges";
  const openInto = summaryOf({ id: "m7", source_branch_id: "d", target_branch_id: "m", direction: "into_parent", status: "draft" });

  function routed(branch: BranchSummary, all: BranchSummary[], routes: Record<string, FakeHandler> = {}, openMerges: MergeSummary[] = []) {
    const f = fakeFetch(routes);
    const result = renderWithRouter(
      <BranchActions
        projectId="p1"
        databaseId="db"
        databaseName="web1"
        branch={branch}
        branches={all}
        historyWindow={86400}
        openMerges={openMerges}
      />,
      { fetch: f },
    );
    return { ...result, fetch: f };
  }

  it("merge_actions_disabled_with_reasons", () => {
    const backup = mk({ id: "b", name: "main-before-restore-x", kind: "backup", parent_branch_id: main.id, host: null });
    const busyMain = { ...main, status: "resetting" as const };
    const cases: [string, BranchSummary, BranchSummary[], string, string, string][] = [
      ["main", main, [main], "Merge into parent", "Update from parent", "main has no parent to merge with"],
      ["a backup", backup, [main, backup], "Merge into main", "Update from main", "a restore backup can't be merged; create a branch from it"],
      ["a branch that isn't ready", { ...dev, status: "provisioning" }, [main, dev], "Merge into main", "Update from main", "branch is not ready (it is provisioning)"],
      ["a parent that isn't ready", dev, [busyMain, dev], "Merge into main", "Update from main", "parent main is not ready (it is resetting)"],
    ];
    for (const [what, branch, all, into, from, reason] of cases) {
      const { unmount } = setup(branch, all);
      for (const name of [into, from]) {
        expect(btn(name), what).toBeDisabled();
        expect(btn(name), what).toHaveAccessibleDescription(reason);
      }
      unmount();
    }
    setup(dev, [main, dev]);
    expect(btn("Merge into main")).toBeEnabled();
    expect(btn("Update from main")).toBeEnabled();
  });

  it("open_merge_reason_links_to_it", async () => {
    routed(dev, [main, dev], {}, [openInto]);
    const into = await screen.findByRole("button", { name: "Merge into main" });
    expect(into).toBeDisabled();
    expect(into).toHaveAccessibleDescription(/a merge into main is already open/);
    const link = screen.getByRole("link", { name: "Open the merge into main" });
    expect(link).toHaveAttribute("href", "/app/projects/p1/databases/db/merges/m7");
    // The other direction is another pair: still available.
    expect(btn("Update from main")).toBeEnabled();
  });

  it("starts a merge and opens its page", async () => {
    const { fetch, router } = routed(dev, [main, dev], {
      [MERGES]: () => jsonResponse(summaryOf({ id: "m9", status: "computing" }), 202),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Update from main" }));
    await vi.waitFor(() => expect(router.state.location.pathname).toBe("/app/projects/p1/databases/db/merges/m9"));
    const post = fetch.calls.find((r) => r.method === "POST")!;
    expect(await post.clone().json()).toEqual({ direction: "from_parent" });
  });

  it("retries a start once, quietly, when a merge of the pair just finished", async () => {
    const { fetch, router } = routed(dev, [main, dev], {
      [MERGES]: sequence(
        () => jsonResponse({ error: { code: "merge_busy", message: "a merge of this pair just finished; try again" } }, 409),
        () => jsonResponse(summaryOf({ id: "m9", status: "computing" }), 202),
      ),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Merge into main" }));
    await vi.waitFor(() => expect(router.state.location.pathname).toBe("/app/projects/p1/databases/db/merges/m9"), { timeout: 3000 });
    expect(fetch.calls.filter((r) => r.method === "POST")).toHaveLength(2);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("links the open merge a refused start names", async () => {
    routed(dev, [main, dev], {
      [MERGES]: () => jsonResponse({ error: { code: "merge_open_exists", message: "raw", merge_id: "m5" } }, 409),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Merge into main" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("These branches already have an open merge.");
    expect(screen.getByRole("link", { name: "Open the merge" })).toHaveAttribute("href", "/app/projects/p1/databases/db/merges/m5");
  });

  it("links the merge that holds the branch when a delete is refused", async () => {
    routed(dev, [main, dev], {
      "DELETE /v1/databases/db/branches/d": () =>
        jsonResponse({ error: { code: "branch_merge_busy", message: "raw", merge_id: "m4" } }, 409),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Delete branch" }));
    await userEvent.click(btn("Confirm delete branch"));
    expect(await screen.findByRole("alert")).toHaveTextContent("A merge holds this branch: it is rehearsing or applying, or it failed half applied.");
    expect(screen.getByRole("link", { name: "Open the merge" })).toHaveAttribute("href", "/app/projects/p1/databases/db/merges/m4");
  });

  it("says a rehearsal's throwaway branch holds the branch when a reset is refused", async () => {
    routed(dev, [main, dev], {
      "POST /v1/databases/db/branches/d/reset-to-parent": () =>
        jsonResponse({ error: { code: "branch_merge_busy", message: "raw", rehearsal_branch_id: "r1" } }, 409),
    });
    await userEvent.click(await screen.findByRole("button", { name: "Reset to parent" }));
    await userEvent.click(btn("Confirm reset to parent"));
    expect(await screen.findByRole("alert")).toHaveTextContent("A merge's rehearsal branch is still being removed from this branch.");
  });

  it("disables reset, restore and delete while a merge holds the branch, and links it", async () => {
    const half = summaryOf({ id: "m3", source_branch_id: "d", target_branch_id: "m", status: "failed", failed_phase: "online" });
    routed(dev, [main, dev], {}, [half]);
    const reason = "a merge failed half applied on this branch: retry it or abandon it";
    for (const name of ["Reset to parent", "Restore", "Delete branch"]) {
      const b = await screen.findByRole("button", { name });
      expect(b).toBeDisabled();
      expect(b).toHaveAccessibleDescription(new RegExp(reason));
    }
    expect(screen.getByRole("link", { name: "Open the merge" })).toHaveAttribute("href", "/app/projects/p1/databases/db/merges/m3");
    // The half-applied merge still owns the pair: no second merge into main.
    expect(btn("Merge into main")).toBeDisabled();
  });
});

describe("protected branches (PB)", () => {
  const PATCH = "PATCH /v1/databases/db/branches/d";
  const REVIEW = "GET /v1/databases/db/branches/d/review";
  const ACK = "POST /v1/databases/db/branches/d/review/acknowledge";
  const FIX = "POST /v1/databases/db/branches/d/review/fix";
  const planted = {
    key: "web1/function/public.planted()",
    database: "web1",
    kind: "function",
    schema: "public",
    name: "planted",
    identity: "",
    function: "public.planted()",
    owner: "dev",
    reasons: [
      { kind: "security_definer", via: null },
      { kind: "policy", via: "public.t.p" },
    ],
    opaque_body: true,
    new: false,
  };
  const review = (over: object = {}) => ({ items: [planted], hash: "h1", acknowledged_hash: null, ...over });

  it("protect_shows_the_review_and_sends_its_hash", async () => {
    const patch = vi.fn(() => jsonResponse({ ...dev, protected: true, review_acknowledged_hash: "h1" }));
    const { fetch } = setup(dev, [main, dev], { [REVIEW]: () => jsonResponse(review()), [PATCH]: patch });
    await userEvent.click(btn("Protect"));
    expect(await screen.findByText("public.planted()")).toBeInTheDocument();
    expect(screen.getByText("security definer")).toBeInTheDocument();
    expect(screen.getByText("policy public.t.p")).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "Guard login roles" })).toBeChecked();
    await userEvent.click(btn("Confirm protect"));
    await vi.waitFor(() => expect(patch).toHaveBeenCalled());
    const call = fetch.calls.find((r) => r.method === "PATCH")!;
    expect(await call.clone().json()).toEqual({ protected: true, review_hash: "h1" });
  });

  it("protect_sends_the_switch_when_changed", async () => {
    const patch = vi.fn(() => jsonResponse({ ...dev, protected: true, guard_logins: false }));
    const { fetch } = setup(dev, [main, dev], {
      [REVIEW]: () => jsonResponse(review({ items: [], hash: "e0" })),
      [PATCH]: patch,
    });
    await userEvent.click(btn("Protect"));
    await userEvent.click(await screen.findByRole("switch", { name: "Guard login roles" }));
    await userEvent.click(btn("Confirm protect"));
    await vi.waitFor(() => expect(patch).toHaveBeenCalled());
    const call = fetch.calls.find((r) => r.method === "PATCH")!;
    expect(await call.clone().json()).toEqual({ protected: true, review_hash: "e0", guard_logins: false });
  });

  it("protect_shows_the_changed_review_on_review_changed", async () => {
    const later = { ...planted, key: "web1/function/public.later()", name: "later", function: "public.later()", new: true };
    const patch = vi.fn(() =>
      jsonResponse(
        { error: { code: "review_changed", message: "changed", review: review({ items: [planted, later], hash: "h2" }) } },
        409,
      ),
    );
    setup(dev, [main, dev], { [REVIEW]: () => jsonResponse(review()), [PATCH]: patch });
    await userEvent.click(btn("Protect"));
    await screen.findByText("public.planted()");
    await userEvent.click(btn("Confirm protect"));
    expect(await screen.findByText("The review changed since you loaded it. Check the list again.")).toBeInTheDocument();
    expect(await screen.findByText("public.later()")).toBeInTheDocument();
  });

  it("reassign_confirms_then_posts_the_fix", async () => {
    const fix = vi.fn(() =>
      jsonResponse({ sql: "ALTER ROUTINE public.planted() OWNER TO postgres;", review: review({ items: [], hash: "e0" }) }),
    );
    const { fetch } = setup(dev, [main, dev], { [REVIEW]: () => jsonResponse(review()), [FIX]: fix });
    await userEvent.click(btn("Protect"));
    await userEvent.click(await screen.findByRole("button", { name: "Reassign public.planted()" }));
    expect(
      screen.getByText("The owner takes this code over. A SECURITY DEFINER function then runs as the owner."),
    ).toBeInTheDocument();
    expect(fix).not.toHaveBeenCalled();
    await userEvent.click(btn("Confirm reassign"));
    await vi.waitFor(() => expect(fix).toHaveBeenCalled());
    const call = fetch.calls.find((r) => r.method === "POST" && r.url.endsWith("/review/fix"))!;
    expect(await call.clone().json()).toEqual({ key: planted.key, action: "reassign", hash: "h1" });
  });

  it("protection_panel_toggles_and_acknowledges", async () => {
    const guarded = { ...dev, protected: true, review_acknowledged_hash: "h0" };
    const patch = vi.fn(() => jsonResponse({ ...guarded, guard_logins: false }));
    const ack = vi.fn(() => jsonResponse(review({ acknowledged_hash: "h1" })));
    const { fetch } = setup(guarded, [main, guarded], {
      [REVIEW]: () => jsonResponse(review({ acknowledged_hash: "h0", items: [{ ...planted, new: true }] })),
      [PATCH]: patch,
      [ACK]: ack,
    });
    expect(await screen.findByText("Review changed")).toBeInTheDocument();
    await userEvent.click(btn("Protection"));
    await userEvent.click(await screen.findByRole("switch", { name: "Guard login roles" }));
    await vi.waitFor(() => expect(patch).toHaveBeenCalled());
    expect(await fetch.calls.find((r) => r.method === "PATCH")!.clone().json()).toEqual({ guard_logins: false });
    await userEvent.click(btn("Acknowledge"));
    await vi.waitFor(() => expect(ack).toHaveBeenCalled());
    expect(await fetch.calls.find((r) => r.url.endsWith("/review/acknowledge"))!.clone().json()).toEqual({ hash: "h1" });
  });

  it("no_badge_when_the_review_is_acknowledged", async () => {
    const guarded = { ...dev, protected: true, review_acknowledged_hash: "h1" };
    const { fetch } = setup(guarded, [main, guarded], { [REVIEW]: () => jsonResponse(review({ acknowledged_hash: "h1" })) });
    await vi.waitFor(() => expect(sent(fetch)).toContain("GET /v1/databases/db/branches/d/review"));
    expect(screen.queryByText("Review changed")).toBeNull();
  });

  it("an_unprotected_branch_loads_no_review_until_protect_opens", () => {
    const { fetch } = setup(dev, [main, dev], { [REVIEW]: () => jsonResponse(review()) });
    expect(sent(fetch)).not.toContain("GET /v1/databases/db/branches/d/review");
  });
});

describe("protected branches (PB): T5 review fixes", () => {
  const REVIEW = "GET /v1/databases/db/branches/d/review";
  const ACK = "POST /v1/databases/db/branches/d/review/acknowledge";
  const FIX = "POST /v1/databases/db/branches/d/review/fix";
  const item = (name: string, isNew = false) => ({
    key: `web1/function/public.${name}()`,
    database: "web1",
    kind: "function",
    schema: "public",
    name,
    identity: "",
    function: `public.${name}()`,
    owner: "dev",
    reasons: [{ kind: "security_definer", via: null }],
    opaque_body: true,
    new: isNew,
  });
  const guarded = { ...dev, protected: true, review_acknowledged_hash: "h1" };
  const current = { items: [item("planted")], hash: "h1", acknowledged_hash: "h1" };
  const changed = (review: object) =>
    jsonResponse({ error: { code: "review_changed", message: "changed", review } }, 409);
  const later = { items: [item("planted"), item("later", true)], hash: "h2", acknowledged_hash: "h1" };

  // Finding 1: a fix answered review_changed shows the new list and says so.
  it("a_fix_answered_review_changed_shows_the_new_list", async () => {
    setup(guarded, [main, guarded], { [REVIEW]: () => jsonResponse(current), [FIX]: () => changed(later) });
    await userEvent.click(btn("Protection"));
    await userEvent.click(await screen.findByRole("button", { name: "Reassign public.planted()" }));
    await userEvent.click(btn("Confirm reassign"));
    expect(await screen.findByText("The review changed since you loaded it. Check the list again.")).toBeInTheDocument();
    expect(await screen.findByText("public.later()")).toBeInTheDocument();
  });

  // Finding 1: so does an acknowledge.
  it("an_acknowledge_answered_review_changed_shows_the_new_list", async () => {
    setup(guarded, [main, guarded], { [REVIEW]: () => jsonResponse(current), [ACK]: () => changed(later) });
    await userEvent.click(btn("Protection"));
    await userEvent.click(await screen.findByRole("button", { name: "Acknowledge" }));
    expect(await screen.findByText("The review changed since you loaded it. Check the list again.")).toBeInTheDocument();
    expect(await screen.findByText("public.later()")).toBeInTheDocument();
  });

  // Finding 2: a review that can't load says why at once and offers a retry.
  it("a_review_that_cannot_load_offers_a_retry", async () => {
    const unavailable: FakeHandler = () =>
      jsonResponse({ error: { code: "unavailable", message: "the branch's compute isn't running" } }, 503);
    setup(dev, [main, dev], {
      [REVIEW]: sequence(unavailable, () => jsonResponse({ items: [item("planted")], hash: "h1", acknowledged_hash: null })),
    });
    await userEvent.click(btn("Protect"));
    await userEvent.click(await screen.findByRole("button", { name: "Retry" }));
    expect(await screen.findByText("public.planted()")).toBeInTheDocument();
    expect(btn("Confirm protect")).toBeEnabled();
  });

  // Finding 4: the fix confirm takes focus.
  it("the_fix_confirm_takes_focus", async () => {
    setup(guarded, [main, guarded], { [REVIEW]: () => jsonResponse(current) });
    await userEvent.click(btn("Protection"));
    await userEvent.click(await screen.findByRole("button", { name: "Drop public.planted()" }));
    expect(btn("Confirm drop")).toHaveFocus();
  });
});

describe("protected branches (PB): end-of-step", () => {
  const REVIEW = "GET /v1/databases/db/branches/d/review";

  // End-of-step Minor: a protected branch whose review can't load (a suspended compute) says so
  // on its row instead of hiding the badge.
  it("a_badge_review_that_cannot_load_says_so", async () => {
    const guarded = { ...dev, protected: true, review_acknowledged_hash: "h1" };
    setup(guarded, [main, guarded], {
      [REVIEW]: () => jsonResponse({ error: { code: "unavailable", message: "the branch's compute isn't running" } }, 503),
    });
    expect(await screen.findByText("Review unavailable")).toBeInTheDocument();
    expect(screen.queryByText("Review changed")).toBeNull();
  });
});
