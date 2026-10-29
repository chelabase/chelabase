import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { B, ROWS, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { WIDTHS_KEY } from "@/lib/table-editor/rows";

const Q = `${B}/console/query`;
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select");

describe("RowsView (more)", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("rows_are_virtualized_and_a_full_page_offers_the_next", async () => {
    const many = Array.from({ length: 50 }, (_, i) => ({ ...ROWS[0], id: `r-${i}`, amount_cents: i }));
    const { f, user } = renderEditor({ rows: many });
    expect(await screen.findByText("Rows 1–50 of ≈ 48,210")).toBeInTheDocument();
    const rendered = within(await grid()).getAllByRole("row").length - 1;
    expect(rendered).toBeGreaterThan(0);
    expect(rendered).toBeLessThan(50);
    await user.click(screen.getByRole("button", { name: "Next page" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.offset).toBe(50));
  });

  it("tab_saves_and_moves_to_the_next_cell", async () => {
    const { f, user } = renderEditor();
    await user.dblClick(await cell("45000"));
    const input = await screen.findByRole("textbox", { name: "amount_cents" });
    await user.clear(input);
    await user.type(input, "7{Tab}");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.set).toEqual({ amount_cents: 7 });
    await vi.waitFor(() => expect(document.activeElement).toHaveAttribute("data-cell", "1:5"));
  });

  it("an_unchanged_value_saves_nothing", async () => {
    const { f, user } = renderEditor();
    await user.dblClick(await cell("Q3 plan"));
    await user.type(await screen.findByRole("textbox", { name: "memo" }), "{Enter}");
    expect(screen.queryByRole("textbox", { name: "memo" })).toBeNull();
    expect(await writes(f)).toHaveLength(0);
  });

  it("an_invalid_number_is_refused_at_the_cell_without_a_request", async () => {
    const { f, user } = renderEditor();
    await user.dblClick(await cell("45000"));
    const input = await screen.findByRole("textbox", { name: "amount_cents" });
    await user.clear(input);
    await user.type(input, "lots{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent("amount_cents: enter a whole number.");
    expect(await writes(f)).toHaveLength(0);
  });

  it("set_null_writes_null_on_a_nullable_column", async () => {
    const { f, user } = renderEditor();
    await user.dblClick(await cell("Q3 plan"));
    await user.click(await screen.findByRole("button", { name: "Set NULL" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.set).toEqual({ memo: null });
  });

  it("enum_columns_edit_with_a_select_of_their_labels", async () => {
    const { user } = renderEditor();
    await user.dblClick(await cell("open"));
    const select = await screen.findByRole("combobox", { name: "status" });
    expect(within(select).getAllByRole("option").map((o) => o.textContent)).toEqual(["open", "paid", "void"]);
  });

  it("space_toggles_the_active_rows_selection", async () => {
    const { user } = renderEditor();
    await user.click(await cell("45000"));
    await user.keyboard(" ");
    expect(screen.getByText("1 selected")).toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: "Select row 2" })).toBeChecked();
  });

  it("typing_on_a_cell_starts_editing_with_that_key", async () => {
    const { user } = renderEditor();
    await user.click(await cell("Q3 plan"));
    await user.keyboard("Z");
    expect(await screen.findByRole("textbox", { name: "memo" })).toHaveValue("Z");
  });

  it("primary_key_cells_are_not_edited_in_place", async () => {
    const { user } = renderEditor();
    await user.dblClick(await cell("81c4-2"));
    expect(screen.queryByRole("textbox", { name: "id" })).toBeNull();
  });

  it("null_shows_as_NULL", async () => {
    renderEditor();
    expect(await cell("NULL")).toBeInTheDocument();
  });

  it("removing_a_filter_reads_without_it", async () => {
    const { f, user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(screen.getByLabelText("Filter column"));
    await user.click(await screen.findByRole("option", { name: "memo" }));
    await user.click(screen.getByLabelText("Filter operator"));
    await user.click(await screen.findByRole("option", { name: "ilike" }));
    await user.type(screen.getByLabelText("Filter value"), "%plan%{Enter}");
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.where).toEqual({ col: "memo", ilike: "%plan%" }));
    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(screen.getByRole("button", { name: "Remove filter memo ilike %plan%" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.where).toBeUndefined());
  });

  it("keyboard_resize_on_a_header_is_saved_per_table", async () => {
    const { user } = renderEditor();
    const header = await within(await grid()).findByRole("columnheader", { name: /memo/ });
    header.focus();
    await user.keyboard("{Alt>}{ArrowRight}{/Alt}");
    const saved = JSON.parse(localStorage.getItem(WIDTHS_KEY)!);
    expect(Object.values(saved)[0]).toEqual({ memo: 216 });
  });

  it("timestamps_edit_as_local_date_times_and_timestamptz_text_is_validated", async () => {
    const t = invoices();
    t.columns = [
      ...t.columns,
      { ...t.columns[4], attnum: 6, name: "seen", data_type: "timestamp without time zone", type_oid: 1114 },
      { ...t.columns[4], attnum: 7, name: "paid_at", data_type: "timestamp with time zone", type_oid: 1184 },
    ];
    const { f, user } = renderEditor({ table: t as Table, rows: [{ ...ROWS[0], seen: "2026-10-01T12:00:05.123456", paid_at: "2026-10-01T12:00:00+00:00" }] });
    await user.dblClick(await cell("2026-10-01T12:00:05.123456"));
    const local = await screen.findByLabelText("seen", { selector: "input" });
    expect(local).toHaveAttribute("type", "datetime-local");
    await user.keyboard("{Enter}");
    expect(await writes(f)).toHaveLength(0);
    await user.dblClick(await cell("2026-10-01T12:00:00+00:00"));
    const tz = await screen.findByRole("textbox", { name: "paid_at" });
    await user.clear(tz);
    await user.type(tz, "soon{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent("paid_at: enter a date and time with its offset");
    await user.clear(tz);
    await user.type(tz, "2026-10-02 08:00+02{Enter}");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.set).toEqual({ paid_at: "2026-10-02 08:00+02" });
  });

  it("a_view_reads_rows_without_editing", async () => {
    const { user } = renderEditor({ params: { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public", table: "v_open" } });
    expect(await screen.findByText("Views are read-only here.")).toBeInTheDocument();
    const g = await screen.findByRole("grid", { name: "public.v_open" });
    await user.dblClick(await within(g).findByRole("gridcell", { name: "45000" }));
    expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull();
  });
});
