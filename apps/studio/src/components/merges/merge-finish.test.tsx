import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { act } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { POLL_MS } from "@/lib/query";
import { jsonResponse, sequence } from "@/test/fake-fetch";
import { detailOf, mergeOf, planStep } from "@/test/merge-fixtures";
import { MERGE_URL, bodies, renderMergePage } from "@/test/merge-page-harness";

const plan = { steps: [planStep("g1", { sql: "ALTER TABLE t ADD c int" })] };

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("MergeFinish", () => {
  it("created_roles_link_to_connection_panel", async () => {
    const { fetch } = renderMergePage(mergeOf({ status: "done", plan, result: { created_roles: [{ name: "reporter" }], drift: [] } }));
    const finish = await screen.findByRole("region", { name: "Finish" });
    expect(finish).toHaveTextContent("Roles created on main");
    const link = within(finish).getByRole("link", { name: "Connection details for reporter on main" });
    const href = new URL(link.getAttribute("href")!, "http://studio.test");
    expect(href.pathname).toBe("/app/projects/p1");
    expect(Object.fromEntries(href.searchParams)).toEqual({
      database: "db",
      branch: "b-main",
      panel: "connection",
      role: "reporter",
    });
    // U3: no password on the merge page, and no reveal from it.
    expect(within(finish).queryByRole("button", { name: /Connection details/ })).toBeNull();
    expect(screen.queryByText("Password")).toBeNull();
    expect(screen.queryByRole("button", { name: /Copy password/ })).toBeNull();
    expect(fetch.calls.some((r) => new URL(r.url).pathname.endsWith("/connection"))).toBe(false);
  });

  it("export_downloads_sql", async () => {
    const created: Blob[] = [];
    const createObjectURL = vi.fn((b: Blob) => {
      created.push(b);
      return "blob:plan";
    });
    Object.assign(URL, { createObjectURL, revokeObjectURL: vi.fn() });
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) {
      expect(this.download).toBe("merge-m1.sql");
      expect(this.href).toBe("blob:plan");
    });
    const { fetch } = renderMergePage(mergeOf({ plan }), {
      routes: {
        "GET /v1/databases/db/merges/m1/plan.sql": () =>
          new Response("-- Phase 1\nBEGIN;\nALTER TABLE t ADD c int;\nCOMMIT;\n", { headers: { "Content-Type": "text/plain" } }),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Download the plan (.sql)" }));
    await vi.waitFor(() => expect(click).toHaveBeenCalledTimes(1));
    expect(await created[0].text()).toContain("ALTER TABLE t ADD c int;");
    expect(fetch.calls.some((r) => new URL(r.url).pathname === "/v1/databases/db/merges/m1/plan.sql")).toBe(true);
  });

  it("saves the after-merge choice for a merge into the parent", async () => {
    const { fetch } = renderMergePage(mergeOf({ plan }), {
      routes: { [`PATCH ${MERGE_URL}`]: () => jsonResponse(mergeOf({ status: "computing", version: 4 })) },
    });
    const finish = await screen.findByRole("region", { name: "Finish" });
    expect(within(finish).getByRole("radio", { name: "Keep dev" })).toBeChecked();
    await userEvent.click(within(finish).getByRole("radio", { name: "Reset dev to main" }));
    await userEvent.click(within(finish).getByRole("button", { name: "Save after-merge choice" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "PATCH", MERGE_URL)).toEqual([{ version: 3, options: { after: "reset" } }]));
  });

  it("has no after-merge choice when updating a branch from its parent", async () => {
    renderMergePage(mergeOf({ plan, direction: "from_parent", source_branch_id: "b-main", target_branch_id: "b-dev" }));
    const finish = await screen.findByRole("region", { name: "Finish" });
    expect(within(finish).queryByRole("radio")).toBeNull();
  });

  it("states the after-merge choice and that saving it keeps a rehearsed merge rehearsed (E34)", async () => {
    renderMergePage(mergeOf({ status: "rehearsed", plan }));
    const finish = await screen.findByRole("region", { name: "Finish" });
    expect(finish).toHaveTextContent("Saving the choice keeps the rehearsal");
  });

  it("words what the choice did instead of printing its fields", async () => {
    renderMergePage(mergeOf({ status: "done", plan, result: { created_roles: [], drift: [], after: { choice: "keep", base: "not_advanced" } } }));
    const finish = await screen.findByRole("region", { name: "Finish" });
    expect(finish).toHaveTextContent("Kept dev, but it changed after the merge was computed, so its base wasn't advanced");
    expect(finish).not.toHaveTextContent("keep: not_advanced");
  });

  it("lists the differences after the apply, but not what own SQL produced", async () => {
    const key = (name: string) => ({ kind: "column", schema: "public", table: "t", name });
    renderMergePage(
      mergeOf({
        status: "applied_with_drift",
        plan,
        result: {
          created_roles: [],
          drift: [
            { key: key("real"), kind: "different", expected: null, actual: null, produced: false },
            { key: key("mine"), kind: "different", expected: null, actual: null, produced: true },
          ],
          after: { choice: "keep", skipped: "the target doesn't read back as merged" },
        },
      }),
    );
    const drift = await screen.findByRole("region", { name: "Differences after the apply" });
    expect(drift).toHaveTextContent("public.t.real: different");
    expect(drift).not.toHaveTextContent("public.t.mine");
    const produced = screen.getByRole("region", { name: "What your own SQL produced" });
    expect(produced).toHaveTextContent("public.t.mine");
  });

  describe("running a skipped after-merge choice (E21)", () => {
    const skipped = (over = {}) =>
      mergeOf({
        status: "applied_with_drift",
        plan,
        options: { after: "reset" },
        result: { created_roles: [], drift: [], after: { choice: "reset", skipped: "the target doesn't read back as merged" } },
        ...over,
      });
    const FINISH = "/v1/databases/db/merges/m1/finish";

    it("offers keep, reset or delete into the parent, starting from the saved choice, and runs the one picked", async () => {
      const { fetch } = renderMergePage(skipped(), { routes: { [`POST ${FINISH}`]: () => jsonResponse(mergeOf({ status: "applied_with_drift" }), 202) } });
      const section = await screen.findByRole("region", { name: "The after-merge choice was skipped" });
      expect(section).toHaveTextContent("main doesn't read back as merged");
      expect(within(section).getByRole("radio", { name: "Reset dev to main" })).toBeChecked();
      await userEvent.click(within(section).getByRole("radio", { name: "Delete dev" }));
      await userEvent.click(within(section).getByRole("button", { name: "Run the after-merge choice" }));
      await vi.waitFor(async () => expect(await bodies(fetch, "POST", FINISH)).toEqual([{ after: "delete" }]));
    });

    it("offers only keep for an update from the parent", async () => {
      const { fetch } = renderMergePage(
        skipped({ direction: "from_parent", source_branch_id: "b-main", target_branch_id: "b-dev", options: {} }),
        { routes: { [`POST ${FINISH}`]: () => jsonResponse(mergeOf({ status: "applied_with_drift" }), 202) } },
      );
      const section = await screen.findByRole("region", { name: "The after-merge choice was skipped" });
      expect(within(section).queryByRole("radio")).toBeNull();
      expect(section).toHaveTextContent("An update from the parent only keeps");
      await userEvent.click(within(section).getByRole("button", { name: "Run the after-merge choice" }));
      await vi.waitFor(async () => expect(await bodies(fetch, "POST", FINISH)).toEqual([{ after: "keep" }]));
    });

    it("polls while the request waits, and shows the outcome when it lands", async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true });
      const requested = skipped({ result: { created_roles: [], drift: [], after: { choice: "reset", requested: true } } });
      const done = skipped({ result: { created_roles: [], drift: [], after: { choice: "reset", outcome: "done" } } });
      renderMergePage(skipped(), {
        get: sequence(
          () => jsonResponse(detailOf(requested)),
          () => jsonResponse(detailOf(done)),
        ),
      });
      const finish = await screen.findByRole("region", { name: "Finish" });
      expect(finish).toHaveTextContent("Waiting to reset dev…");
      expect(within(finish).queryByRole("button", { name: "Run the after-merge choice" })).toBeNull();
      await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
      await vi.waitFor(() => expect(finish).toHaveTextContent("dev was reset to main."));
    });

    it("has no run button once the choice ran, and shows a refusal in words", async () => {
      renderMergePage(skipped({ result: { created_roles: [], drift: [], after: { choice: "delete", refused: "dev has child branches: x" } } }));
      const finish = await screen.findByRole("region", { name: "Finish" });
      expect(finish).toHaveTextContent("Not done: dev has child branches: x. dev stays as it is; the merge itself is complete.");
      expect(within(finish).queryByRole("button", { name: "Run the after-merge choice" })).toBeNull();
    });
  });
});
