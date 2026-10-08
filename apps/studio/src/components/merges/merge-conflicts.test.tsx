import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { jsonResponse } from "@/test/fake-fetch";
import { columnSnap, mergeOf } from "@/test/merge-fixtures";
import { MERGE_URL, bodies, renderMergePage, setEditorText } from "@/test/merge-page-harness";

const conflicted = mergeOf({
  analysis: {
    items: [
      {
        id: "column:1:3",
        key: { kind: "column", schema: "public", table: "orders", name: "note" },
        source: { alter: { before: columnSnap("note"), after: columnSnap("note", "int4") } },
        target: { alter: { before: columnSnap("note"), after: columnSnap("note", "int8") } },
        outcome: { conflict: "c1" },
        data_loss: false,
        security: false,
      },
    ],
    conflicts: [
      { id: "c1", kind: "edit", items: ["column:1:3"], base: columnSnap("note"), target: columnSnap("note", "int8"), source: columnSnap("note", "int4") },
    ],
    reported: [],
  },
  plan: null,
});
const saved = () => jsonResponse(mergeOf({ ...conflicted, status: "computing", version: 4 }));

describe("MergeConflicts", () => {
  it("names each side's text block after the conflicting object", async () => {
    renderMergePage(conflicted);
    const conflict = await screen.findByRole("group", { name: /Changed differently on both sides: public\.orders\.note/ });
    expect(within(conflict).getByLabelText("Base (branch point) of public.orders.note")).toBeInTheDocument();
    expect(within(conflict).getByLabelText("main (target) of public.orders.note")).toBeInTheDocument();
    expect(within(conflict).getByLabelText("dev (source) of public.orders.note")).toBeInTheDocument();
  });

  it("conflict_choice_saves_with_version", async () => {
    const { fetch } = renderMergePage(conflicted, { routes: { [`PATCH ${MERGE_URL}`]: saved } });
    const conflict = await screen.findByRole("group", { name: /Changed differently on both sides: public\.orders\.note/ });
    // Base, target and source side by side.
    expect(within(conflict).getByRole("figure", { name: "main (target)" })).toHaveTextContent('"type": "int8"');
    expect(within(conflict).getByRole("figure", { name: "dev (source)" })).toHaveTextContent('"type": "int4"');
    await userEvent.click(within(conflict).getByRole("radio", { name: "Take dev's version" }));
    await userEvent.click(screen.getByRole("button", { name: "Save choices" }));
    await vi.waitFor(async () =>
      expect(await bodies(fetch, "PATCH", MERGE_URL)).toEqual([
        { version: 3, resolutions: [{ conflict: "c1", choice: "take_source" }] },
      ]),
    );
  });

  it("offers only the column order choices for a column order conflict", async () => {
    const order = mergeOf({
      ...conflicted,
      analysis: {
        ...(conflicted.analysis as object),
        conflicts: [{ id: "c2", kind: "column_order", items: ["column:1:3"], base: null, target: null, source: null }],
      },
    });
    renderMergePage(order);
    const conflict = await screen.findByRole("group", { name: /Both sides added columns/ });
    expect(within(conflict).getAllByRole("radio")).toHaveLength(2);
    expect(within(conflict).getByRole("radio", { name: "main's columns first, then dev's" })).toBeInTheDocument();
    expect(within(conflict).getByRole("radio", { name: "Write my own SQL" })).toBeInTheDocument();
  });

  it("own_sql_editor_saves_text", async () => {
    const { fetch } = renderMergePage(conflicted, { routes: { [`PATCH ${MERGE_URL}`]: saved } });
    const conflict = await screen.findByRole("group", { name: /^Changed differently on both sides: public\.orders\.note/ });
    await userEvent.click(within(conflict).getByRole("radio", { name: "Write my own SQL" }));
    // Blank own SQL isn't saved.
    await userEvent.click(screen.getByRole("button", { name: "Save choices" }));
    expect(await screen.findByText("Write the SQL for public.orders.note, or pick another choice.")).toBeInTheDocument();
    expect(await bodies(fetch, "PATCH", MERGE_URL)).toEqual([]);
    await setEditorText("Own SQL for public.orders.note", "ALTER TABLE orders ALTER note TYPE bigint");
    await userEvent.click(screen.getByRole("button", { name: "Save choices" }));
    await vi.waitFor(async () =>
      expect(await bodies(fetch, "PATCH", MERGE_URL)).toEqual([
        { version: 3, resolutions: [{ conflict: "c1", choice: { own_sql: "ALTER TABLE orders ALTER note TYPE bigint" } }] },
      ]),
    );
  });

  it("can't be changed while the merge isn't a draft or rehearsed", async () => {
    renderMergePage({ ...conflicted, status: "rehearsing" });
    const conflict = await screen.findByRole("group", { name: /^Changed differently on both sides: public\.orders\.note/ });
    expect(within(conflict).getByRole("radio", { name: "Keep main's version" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Save choices" })).toBeDisabled();
  });
});
