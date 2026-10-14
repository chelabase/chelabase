import { render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, ROWS, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { Grid, type GridProps } from "./grid";

// Fix round 1 of T3 (review C1, I1, I2, M4, M6/D3).
const Q = `${B}/console/query`;
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select");
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");

describe("T3 fix round", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("C1_an_open_editor_closes_when_the_view_changes", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op !== "select") return jsonResponse([]);
      return jsonResponse(b.tree.where ? [ROWS[2], ROWS[0]] : ROWS);
    };
    const { f, user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    await user.dblClick(await cell("45000"));
    expect(await screen.findByRole("textbox", { name: "amount_cents" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(screen.getByLabelText("Filter column"));
    await user.click(await screen.findByRole("option", { name: "status" }));
    await user.click(screen.getByLabelText("Filter operator"));
    await user.click(await screen.findByRole("option", { name: "neq" }));
    await user.type(screen.getByLabelText("Filter value"), "open");
    await user.click(screen.getByRole("button", { name: "Apply filter" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.where).toBeTruthy());
    expect(await cell("8900")).toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull();
    expect(await writes(f)).toHaveLength(0);
  });

  it("C1_a_refetch_that_reorders_rows_still_saves_the_opened_row", async () => {
    let call = 0;
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op !== "select") return jsonResponse([{ ...ROWS[1], amount_cents: 46000 }]);
      call += 1;
      // The refetch moves the opened row (81c4-2) from index 1 to index 0.
      return jsonResponse(call === 1 ? ROWS : [ROWS[1], ROWS[2], ROWS[0]]);
    };
    const { f, user, queryClient } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    await user.dblClick(await cell("45000"));
    const input = await screen.findByRole("textbox", { name: "amount_cents" });
    await queryClient.invalidateQueries();
    await vi.waitFor(() => expect(call).toBeGreaterThan(1));
    await user.clear(input);
    await user.type(input, "46000{Enter}");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.where).toEqual({ col: "id", eq: "81c4-2" });
  });

  it("I2_rows_read_as_another_role_are_not_shown_while_the_new_role_loads", async () => {
    let release: () => void = () => {};
    const gate = new Promise<void>((r) => { release = r; });
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.role === "reporter") { await gate; return jsonResponse([ROWS[0]]); }
      return jsonResponse(ROWS);
    };
    const { user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    expect(await cell("45000")).toBeInTheDocument();
    await user.click(screen.getByLabelText("Act as"));
    await user.click(await screen.findByRole("option", { name: "reporter" }));
    await vi.waitFor(() => expect(screen.queryByRole("gridcell", { name: "45000" })).toBeNull());
    release();
    expect(await cell("12900")).toBeInTheDocument();
  });

  it("M4_the_rows_view_keeps_its_page_across_the_structure_tab", async () => {
    const many = Array.from({ length: 50 }, (_, i) => ({ ...ROWS[0], id: `r-${i}`, amount_cents: i }));
    const { f, user } = renderEditor({ rows: many });
    expect(await screen.findByText("Rows 1–50 of ≈ 48,210")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Next page" }));
    expect(await screen.findByText("Rows 51–100 of ≈ 48,210")).toBeInTheDocument();
    const before = (await selects(f)).length;
    await user.click(screen.getByRole("tab", { name: "Structure" }));
    await user.click(screen.getByRole("tab", { name: "Rows" }));
    expect(await screen.findByText("Rows 51–100 of ≈ 48,210")).toBeInTheDocument();
    expect((await selects(f)).slice(before).every((b) => b.tree.offset === 50)).toBe(true);
  });

  it("D3_json_cells_edit_in_codemirror_and_dates_in_a_date_input", async () => {
    const t = invoices();
    t.columns = [
      ...t.columns,
      { ...t.columns[4], attnum: 6, name: "meta", data_type: "jsonb", type_oid: 3802 },
      { ...t.columns[4], attnum: 7, name: "due_on", data_type: "date", type_oid: 1082 },
    ];
    const { user } = renderEditor({ table: t as Table, rows: [{ ...ROWS[0], meta: { a: 1 }, due_on: "2026-10-01" }] });
    await user.dblClick(await cell('{"a":1}'));
    const json = await screen.findByRole("textbox", { name: "meta" });
    expect(json.closest(".cm-editor")).not.toBeNull();
    await user.keyboard("{Escape}");
    await user.dblClick(await cell("2026-10-01"));
    const date = await screen.findByLabelText("due_on");
    expect(date).toHaveAttribute("type", "date");
    expect(date).toHaveValue("2026-10-01");
  });
});

describe("I1: the grid hosts a strip in its own scroll container", () => {
  it("renders_above_inside_the_scroll_container_outside_the_grid_role", () => {
    stubLayout();
    const columns = ["id", "memo"].map((name) => ({ name, desc: { type: "text", nullable: true, has_default: false, generated: false, array: false, enum: null }, labels: null, readOnly: false }));
    const props = {
      label: "public.t", columns, rows: [{ id: "a", memo: "x" }], firstRowNumber: 1, keys: ['["a"]'],
      isSelected: () => false, pageSelection: "none", onToggleRow: () => {}, onTogglePage: () => {}, sort: null, onSort: () => {},
      canEdit: () => false, onSave: async () => null, onCopy: () => {}, widths: {}, onWidths: () => {},
      above: (layout: { template: string; totalWidth: number; visibleCols: number[] }) => (
        <div data-testid="probe" data-template={layout.template} data-cols={layout.visibleCols.join(",")}>strip</div>
      ),
    } as unknown as GridProps;
    render(<Grid {...props} />);
    const probe = screen.getByTestId("probe");
    const g = screen.getByRole("grid", { name: "public.t" });
    const scroller = g.closest("[data-grid-scroll]");
    expect(scroller).not.toBeNull();
    expect(probe.closest("[data-grid-scroll]")).toBe(scroller);
    expect(g.contains(probe)).toBe(false);
    expect(probe.dataset.template).not.toBe("");
    expect(probe.dataset.cols).toBe("0,1");
  });
});
