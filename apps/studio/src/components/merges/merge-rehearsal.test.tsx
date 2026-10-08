import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { jsonResponse } from "@/test/fake-fetch";
import { columnSnap, mergeOf, planStep } from "@/test/merge-fixtures";
import { bodies, renderMergePage } from "@/test/merge-page-harness";

const plan = { steps: [planStep("g1", { sql: "ALTER TABLE t ADD c int" }), planStep("nn", { sql: "ALTER TABLE t ALTER c SET NOT NULL" })] };

describe("MergeRehearsal", () => {
  it("rehearsal_failure_shows_step_and_error", async () => {
    renderMergePage(
      mergeOf({
        plan,
        rehearsal: {
          steps: [
            { id: "g1", duration_ms: 12, outcome: "ok" },
            { id: "nn", duration_ms: 3, outcome: "failed" },
          ],
          failed_step: "nn",
          error: 'ERROR:  column "c" of relation "t" contains null values',
          drift: [],
        },
      }),
    );
    const rehearsal = await screen.findByRole("region", { name: "Rehearsal" });
    const failure = within(rehearsal).getByRole("alert");
    expect(failure).toHaveTextContent("The rehearsal failed at step nn. Nothing changed on main.");
    expect(failure).toHaveTextContent("ALTER TABLE t ALTER c SET NOT NULL");
    expect(failure).toHaveTextContent('column "c" of relation "t" contains null values');
    const timings = within(rehearsal).getByRole("table", { name: "Step timings" });
    expect(within(timings).getAllByRole("row")[1]).toHaveTextContent("g1ok12 ms");
  });

  it("rehearses a draft whose conflicts are settled", async () => {
    const { fetch } = renderMergePage(mergeOf({ plan }), {
      routes: { "POST /v1/databases/db/merges/m1/rehearse": () => jsonResponse(mergeOf({ status: "rehearsing" }), 202) },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Rehearse" }));
    await vi.waitFor(() => expect(fetch.calls.some((r) => r.method === "POST" && r.url.endsWith("/m1/rehearse"))).toBe(true));
  });

  it("sends the version it saw", async () => {
    const { fetch } = renderMergePage(mergeOf({ plan, version: 8 }), {
      routes: { "POST /v1/databases/db/merges/m1/rehearse": () => jsonResponse(mergeOf({ status: "rehearsing" }), 202) },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Rehearse" }));
    await vi.waitFor(async () => expect(await bodies(fetch, "POST", "/v1/databases/db/merges/m1/rehearse")).toEqual([{ version: 8 }]));
  });

  it("says what an edit does to a rehearsed merge: choices and steps return it to draft, acknowledgements don't", async () => {
    renderMergePage(mergeOf({ status: "rehearsed", plan }));
    const rehearse = await screen.findByRole("button", { name: "Rehearse" });
    expect(rehearse).toBeDisabled();
    expect(rehearse).toHaveAccessibleDescription(
      "Already rehearsed. Changing the choices, steps or lock timeout returns it to draft, to rehearse again; acknowledgements and the after-merge choice don't.",
    );
  });

  it("can't rehearse with open conflicts, and says why", async () => {
    renderMergePage(
      mergeOf({ plan: null, analysis: { items: [], conflicts: [{ id: "c1", kind: "edit", items: [], base: null, target: null, source: null }], reported: [] } }),
    );
    const rehearse = await screen.findByRole("button", { name: "Rehearse" });
    expect(rehearse).toBeDisabled();
    expect(rehearse).toHaveAccessibleDescription("Choose how to settle 1 conflict, and save.");
  });

  it("shows what own SQL produced, and drift", async () => {
    renderMergePage(
      mergeOf({
        status: "rehearsed",
        plan,
        rehearsal: {
          steps: [],
          failed_step: null,
          error: null,
          drift: [
            {
              key: { kind: "column", schema: "public", table: "t", name: "note" },
              kind: "different",
              expected: null,
              actual: columnSnap("note", "int8"),
              produced: true,
            },
            {
              key: { kind: "index", schema: "public", table: "t", name: "t_c_idx" },
              kind: "unexpected",
              expected: null,
              actual: null,
              produced: false,
            },
          ],
        },
      }),
    );
    const rehearsal = await screen.findByRole("region", { name: "Rehearsal" });
    // A rehearsal that finished with differences is still rehearsed; it doesn't say it simply passed.
    expect(within(rehearsal).queryByText("The rehearsal passed.")).toBeNull();
    expect(rehearsal).toHaveTextContent("The rehearsal ran, but its result differs from the merged schema (below). Look the differences over before you apply.");
    const produced = within(rehearsal).getByRole("region", { name: "What your own SQL produced" });
    expect(produced).toHaveTextContent("public.t.note");
    expect(produced).toHaveTextContent('"type": "int8"');
    const drift = within(rehearsal).getByRole("region", { name: "Differences from the expected schema" });
    expect(drift).toHaveTextContent("public.t.t_c_idx: not expected");
  });

  it("says a rehearsal passed when it has no differences", async () => {
    renderMergePage(mergeOf({ status: "rehearsed", plan, rehearsal: { steps: [], failed_step: null, error: null, drift: [] } }));
    const rehearsal = await screen.findByRole("region", { name: "Rehearsal" });
    expect(within(rehearsal).getByText("The rehearsal passed.")).toBeInTheDocument();
  });

  it("labels the SQL blocks so a screen reader can tell them apart", async () => {
    renderMergePage(
      mergeOf({
        plan,
        rehearsal: { steps: [], failed_step: "nn", error: "boom", drift: [{ key: { kind: "column", schema: "public", table: "t", name: "note" }, kind: "different", expected: null, actual: columnSnap("note"), produced: true }] },
      }),
    );
    const rehearsal = await screen.findByRole("region", { name: "Rehearsal" });
    expect(within(rehearsal).getByLabelText("SQL of the failed step nn")).toBeInTheDocument();
    expect(within(rehearsal).getByLabelText("Postgres's error")).toBeInTheDocument();
    expect(within(rehearsal).getByLabelText("What your own SQL produced for public.t.note")).toBeInTheDocument();
  });
});
