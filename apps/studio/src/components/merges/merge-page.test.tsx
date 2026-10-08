import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { POLL_MS } from "@/lib/query";
import { jsonResponse, sequence } from "@/test/fake-fetch";
import { columnSnap, detailOf, mergeOf, planStep } from "@/test/merge-fixtures";
import { MERGE_URL, bodies, renderMergePage } from "@/test/merge-page-harness";

afterEach(() => vi.useRealTimers());

const getCalls = (f: { calls: Request[] }) => f.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname === MERGE_URL).length;

describe("MergePage", () => {
  it("merge_page_polls_while_busy_and_stops", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const { fetch } = renderMergePage(mergeOf(), {
      get: sequence(
        () => jsonResponse(detailOf(mergeOf({ status: "computing", analysis: null, plan: null }))),
        () => jsonResponse(detailOf(mergeOf({ status: "draft" }))),
      ),
    });
    expect(await screen.findByRole("heading", { level: 1, name: "Merge dev into main" })).toBeInTheDocument();
    expect(screen.getByTestId("merge-status")).toHaveTextContent("Computing");
    expect(screen.getByText(/Reading both schemas/)).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    await vi.waitFor(() => expect(screen.getByTestId("merge-status")).toHaveTextContent("Draft"));
    const after = getCalls(fetch);
    expect(after).toBe(2);
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS * 3));
    expect(getCalls(fetch)).toBe(after);
  });

  it("version_conflict_reloads", async () => {
    const conflict = { id: "c1", kind: "edit", items: [], base: null, target: columnSnap("a", "int8"), source: columnSnap("a", "int4") };
    const mine = mergeOf({ analysis: { items: [], conflicts: [conflict], reported: [] }, plan: null });
    const theirs = mergeOf({ ...mine, version: 5, resolutions: [{ conflict: "c1", choice: "keep_target" }] });
    const { fetch } = renderMergePage(mine, {
      get: sequence(
        () => jsonResponse(detailOf(mine)),
        () => jsonResponse(detailOf(theirs)),
      ),
      routes: {
        [`PATCH ${MERGE_URL}`]: sequence(
          () => jsonResponse({ error: { code: "merge_version_conflict", message: "raw", current: theirs } }, 409),
          () => jsonResponse(mergeOf({ ...theirs, status: "computing", version: 6 })),
        ),
      },
    });
    await userEvent.click(await screen.findByRole("radio", { name: "Take dev's version" }));
    await userEvent.click(screen.getByRole("button", { name: "Save choices" }));
    expect(await screen.findByText("This merge changed elsewhere and was reloaded. Check it and save again.")).toBeInTheDocument();
    // The other save's choice is what the page shows now.
    await vi.waitFor(() => expect(screen.getByRole("radio", { name: "Keep main's version" })).toBeChecked());
    await userEvent.click(screen.getByRole("radio", { name: "Take dev's version" }));
    await userEvent.click(screen.getByRole("button", { name: "Save choices" }));
    await vi.waitFor(async () => expect(((await bodies(fetch, "PATCH", MERGE_URL))[1] as { version: number }).version).toBe(5));
  });

  it("offers re-baseline when the branch point's schema is gone, and says what it means", async () => {
    const gone = mergeOf({
      error: "merge_base_unavailable: the branch point is older than the history window",
      analysis: null,
      plan: null,
    });
    const { fetch } = renderMergePage(gone, {
      routes: { "POST /v1/databases/db/merges/m1/recompute": () => jsonResponse(mergeOf({ status: "computing" }), 202) },
    });
    const warning = await screen.findByRole("region", { name: "The branch point's schema is gone" });
    expect(warning).toHaveTextContent("main's own changes since the branch point will show as things to undo");
    expect(warning).toHaveTextContent("Drops still need acknowledging");
    await userEvent.click(within(warning).getByRole("button", { name: "Re-baseline" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "POST", "/v1/databases/db/merges/m1/recompute")).toEqual([{ rebaseline: true }]));
  });

  it("offers re-baseline on a failed merge whose base is gone, as the API allows", async () => {
    const failed = mergeOf({
      status: "failed",
      failed_phase: "compute",
      error: "merge_base_unavailable: the branch point is older than the history window",
      analysis: null,
      plan: null,
    });
    const { fetch } = renderMergePage(failed, {
      routes: { "POST /v1/databases/db/merges/m1/recompute": () => jsonResponse(mergeOf({ status: "computing" }), 202) },
    });
    const warning = await screen.findByRole("region", { name: "The branch point's schema is gone" });
    const button = within(warning).getByRole("button", { name: "Re-baseline" });
    expect(button).toBeEnabled();
    await userEvent.click(button);
    await vi.waitFor(async () => expect(await bodies(fetch, "POST", "/v1/databases/db/merges/m1/recompute")).toEqual([{ rebaseline: true }]));
  });

  it("doesn't offer re-baseline once the merge failed in its online phase", async () => {
    renderMergePage(
      mergeOf({ status: "failed", failed_phase: "online", error: "merge_base_unavailable", analysis: null, plan: null }),
    );
    const warning = await screen.findByRole("region", { name: "The branch point's schema is gone" });
    expect(within(warning).getByRole("button", { name: "Re-baseline" })).toBeDisabled();
  });

  it("keeps warning on a re-baselined merge", async () => {
    renderMergePage(mergeOf({ rebaselined: true }));
    expect(await screen.findByText(/This merge was re-baselined/)).toBeInTheDocument();
  });

  it("cancels a merge that isn't changing the target", async () => {
    const { fetch } = renderMergePage(mergeOf(), {
      routes: { "POST /v1/databases/db/merges/m1/cancel": () => jsonResponse(mergeOf({ status: "cancelled" }), 202) },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Cancel merge" }));
    await vi.waitFor(() => expect(fetch.calls.some((r) => r.method === "POST" && r.url.endsWith("/m1/cancel"))).toBe(true));
  });

  it("says when the merge doesn't exist", async () => {
    renderMergePage(mergeOf(), { get: () => jsonResponse({ error: { code: "not_found", message: "merge not found" } }, 404) });
    expect(await screen.findByRole("alert")).toHaveTextContent("merge not found");
  });

  describe("what the page says about the merge's error", () => {
    it("names the last compute as the source for a draft's planner error", async () => {
      renderMergePage(mergeOf({ plan: null, error: "1 statement(s) can't run in a merge:\nstep s: no" }));
      const alert = await screen.findByRole("alert");
      expect(alert).toHaveTextContent("The last compute couldn't finish the plan.");
      expect(alert).toHaveTextContent("step s: no");
    });

    it("lists a refused resolution before the planner's lines, and says its conflict is open again", async () => {
      renderMergePage(
        mergeOf({ plan: null, error: "merge_resolution_refused: c1: own SQL is empty; c2: not valid for this conflict\n1 statement(s) can't run in a merge:\nstep s: no" }),
      );
      const alert = await screen.findByRole("alert");
      expect(alert).toHaveTextContent("A saved choice was refused. Its conflict is open again: choose again and save.");
      expect(alert).toHaveTextContent("c1: own SQL is empty");
      expect(alert).toHaveTextContent("c2: not valid for this conflict");
      expect(alert).toHaveTextContent("step s: no");
      expect(alert).not.toHaveTextContent("The last compute couldn't finish the plan.");
    });

    it("does not say a rehearsed merge's interrupted apply applied nothing", async () => {
      renderMergePage(
        mergeOf({
          status: "rehearsed",
          error: "interrupted before the plan's transaction committed; roles and enum values it made before that stay on the target",
        }),
      );
      const alert = await screen.findByRole("alert");
      expect(alert).toHaveTextContent("The apply was interrupted. The merge is rehearsed again: apply it once more.");
      expect(alert).toHaveTextContent("roles and enum values it made before that stay on the target");
      expect(alert).not.toHaveTextContent("The last compute couldn't finish the plan.");
      expect(screen.queryByText(/nothing was applied/i)).toBeNull();
    });

    it.each([
      ["branch_not_ready: branch main is resetting", "branch main is resetting"],
      ["merge_conflicts_open: 1 conflict(s) have no resolution: c1", "1 conflict(s) have no resolution: c1"],
      [
        "merge_drops_unacknowledged: acknowledge these data-loss, security or breaking changes first: s1",
        "acknowledge these data-loss, security or breaking changes first: s1",
      ],
    ])("says the apply didn't start when a rehearsed merge's apply was refused: %s", async (error, reason) => {
      renderMergePage(mergeOf({ status: "rehearsed", error }));
      const alert = await screen.findByRole("alert");
      expect(alert).toHaveTextContent("The apply didn't start.");
      expect(alert).toHaveTextContent(reason);
      expect(alert).not.toHaveTextContent("interrupted");
    });

    it("tells a stale merge in the worker's words, not 'nothing was applied' after a retried apply", async () => {
      renderMergePage(
        mergeOf({
          status: "stale",
          error: "merge_stale: the target's schema changed since the merge was computed; the plan's transaction wasn't applied (recompute the merge)",
        }),
      );
      expect(await screen.findByText(/The target's schema changed since the merge was computed; the plan's transaction wasn't applied/)).toBeInTheDocument();
      expect(screen.queryByText(/nothing was applied/i)).toBeNull();
    });

    it("says a stale merge's plan wasn't applied when the worker left no text", async () => {
      renderMergePage(mergeOf({ status: "stale" }));
      expect(
        await screen.findByText(/A schema changed since the merge was computed, so the plan's transaction wasn't applied/),
      ).toBeInTheDocument();
    });

    it("tells a stale merge's sentence once, with no error panel repeating it (minor 1)", async () => {
      renderMergePage(
        mergeOf({ status: "stale", error: "merge_stale: the target's schema changed since the merge was computed; recompute it" }),
      );
      expect(await screen.findByText("The target's schema changed since the merge was computed; recompute it")).toBeInTheDocument();
      expect(screen.queryAllByText(/the target's schema changed since the merge was computed/i)).toHaveLength(1);
      expect(screen.queryByText("The merge reported a problem.")).toBeNull();
    });

    it("leaves a failed rehearsal to the Rehearsal section, with no error panel (D3)", async () => {
      const step = planStep("s2", { sql: "ALTER TABLE t ALTER COLUMN email SET NOT NULL" });
      renderMergePage(
        mergeOf({
          plan: { steps: [step] },
          error: "the rehearsal failed at step s2: column \"email\" contains null values",
          rehearsal: { steps: [{ id: "s2", outcome: "failed", duration_ms: 3 }], failed_step: "s2", error: "column \"email\" contains null values", drift: [] },
        }),
      );
      const rehearsal = await screen.findByRole("region", { name: "Rehearsal" });
      expect(within(rehearsal).getByText(/The rehearsal failed at step s2/)).toBeInTheDocument();
      expect(screen.queryByText("The last compute couldn't finish the plan.")).toBeNull();
      // Postgres's text appears once, in the Rehearsal section.
      expect(screen.getAllByText(/contains null values/)).toHaveLength(1);
    });

    it.each([
      ["creating the rehearsal: storage refused the branch", "storage refused the branch"],
      ["branch_not_ready: branch main is resetting", "branch main is resetting"],
      ["creating the rehearsal: branch_not_ready: branch main isn't ready", "branch main isn't ready"],
    ])("says the rehearsal couldn't run, with the worker's reason (D3): %s", async (error, reason) => {
      renderMergePage(mergeOf({ error }));
      const alert = await screen.findByRole("alert");
      expect(alert).toHaveTextContent("The rehearsal couldn't run.");
      expect(alert).toHaveTextContent(reason);
      expect(alert).not.toHaveTextContent("The last compute couldn't finish the plan.");
    });

    it("says the rehearsal's result couldn't be read (D3)", async () => {
      renderMergePage(mergeOf({ error: "the rehearsal: reading the result: timed out" }));
      const alert = await screen.findByRole("alert");
      expect(alert).toHaveTextContent("The rehearsal ran, but its result couldn't be read. Rehearse again.");
      expect(alert).toHaveTextContent("reading the result: timed out");
      expect(alert).not.toHaveTextContent("The last compute couldn't finish the plan.");
    });

    it("shows the worker's reason when the branch point's schema is gone, and doesn't blame a restore or the window", async () => {
      renderMergePage(mergeOf({ error: "merge_base_unavailable: the stored base is from another model version", analysis: null, plan: null }));
      const warning = await screen.findByRole("region", { name: "The branch point's schema is gone" });
      expect(warning).toHaveTextContent("Why: the stored base is from another model version");
      expect(warning).not.toHaveTextContent("wasn't saved when");
    });

    it("says a cancelled merge applied nothing, and an abandoned one kept its transaction", async () => {
      const { unmount } = renderMergePage(mergeOf({ status: "cancelled" }));
      expect(await screen.findByText("Cancelled. Nothing was applied.")).toBeInTheDocument();
      unmount();
      renderMergePage(mergeOf({ status: "cancelled", result: { leftovers: [] } }));
      expect(await screen.findByText(/Abandoned half applied/)).toBeInTheDocument();
      expect(screen.queryByText("Cancelled. Nothing was applied.")).toBeNull();
    });
  });

  it("keeps unsaved edits in one section when another section is saved", async () => {
    const conflict = { id: "c1", kind: "edit", items: [], base: null, target: columnSnap("a", "int8"), source: columnSnap("a", "int4") };
    const mine = mergeOf({ analysis: { items: [], conflicts: [conflict], reported: [] }, plan: { steps: [planStep("g1")] } });
    const saved = mergeOf({ ...mine, version: 4, resolutions: [{ conflict: "c1", choice: "take_source" }] });
    renderMergePage(mine, {
      get: sequence(
        () => jsonResponse(detailOf(mine)),
        () => jsonResponse(detailOf(saved)),
      ),
      routes: { [`PATCH ${MERGE_URL}`]: () => jsonResponse(saved) },
    });
    await userEvent.type(await screen.findByLabelText("Step SQL"), "UPDATE t SET c = 0");
    await userEvent.click(screen.getByRole("radio", { name: "Take dev's version" }));
    await userEvent.click(screen.getByRole("button", { name: "Save choices" }));
    // The save went through and the page reloaded the merge (version 4)...
    await vi.waitFor(() => expect(screen.getByRole("button", { name: "Save choices" })).toBeEnabled());
    await vi.waitFor(() => expect(screen.getByTestId("merge-status")).toBeInTheDocument());
    // ...and the other section's unsaved SQL is still there.
    expect(screen.getByLabelText("Step SQL")).toHaveValue("UPDATE t SET c = 0");
  });

  it("keeps the unsaved step SQL when the Plan section saves its acknowledgements (minor 6)", async () => {
    const drop = planStep("s1", { items: ["column:1:2"], data_loss: true });
    const mine = mergeOf({ plan: { steps: [drop] } });
    const saved = mergeOf({ ...mine, version: 4, acknowledgements: ["column:1:2"] });
    const { fetch } = renderMergePage(mine, {
      get: sequence(
        () => jsonResponse(detailOf(mine)),
        () => jsonResponse(detailOf(saved)),
      ),
      routes: { [`PATCH ${MERGE_URL}`]: () => jsonResponse(saved) },
    });
    await userEvent.type(await screen.findByLabelText("Step SQL"), "UPDATE t SET c = 0");
    await userEvent.click(screen.getByRole("checkbox", { name: "I accept the data loss of s1" }));
    await userEvent.click(screen.getByRole("button", { name: "Save acknowledgements" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "PATCH", MERGE_URL)).toHaveLength(1));
    await vi.waitFor(() => expect(getCalls(fetch)).toBe(2));
    expect(screen.getByRole("checkbox", { name: "I accept the data loss of s1" })).toBeChecked();
    expect(screen.getByLabelText("Step SQL")).toHaveValue("UPDATE t SET c = 0");
  });

  it("shows acknowledgements someone else saved", async () => {
    const drop = planStep("s1", { items: ["column:1:2"], data_loss: true });
    const mine = mergeOf({ plan: { steps: [drop] } });
    const theirs = mergeOf({ ...mine, version: 4, acknowledgements: ["column:1:2"] });
    vi.useFakeTimers({ shouldAdvanceTime: true });
    renderMergePage(mine, {
      get: sequence(
        () => jsonResponse(detailOf({ ...mine, status: "computing" })),
        () => jsonResponse(detailOf(theirs)),
      ),
    });
    const box = await screen.findByRole("checkbox", { name: "I accept the data loss of s1" });
    expect(box).not.toBeChecked();
    await act(() => vi.advanceTimersByTimeAsync(POLL_MS));
    await vi.waitFor(() => expect(screen.getByRole("checkbox", { name: "I accept the data loss of s1" })).toBeChecked());
  });
});
