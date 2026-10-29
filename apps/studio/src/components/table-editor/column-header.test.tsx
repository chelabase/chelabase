import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { B, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { DRAFTS_KEY } from "@/lib/table-editor/store";
import { newEdit, tableFingerprint } from "@/lib/table-editor/changeset";
import { Grid, type GridProps } from "./grid";

/**
 * One column header row (2026-10-29): the column chip is the grid's header cell. A click (or Enter) opens the
 * column form; sorting lives in the chip's `⋯` menu ("Sort ascending", "Sort descending", "Clear sort" when that
 * column is sorted), first, before the structure items. Shift+F10 on a header opens that menu. Without structure
 * (a plain grid), the header still names the column, a click or Enter cycles the sort, and the menu has only sort.
 */

const Q = `${B}/console/query`;
const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const header = async (name: string) => within(await grid()).findByRole("columnheader", { name });
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");
const draft = (...ops: Parameters<typeof newEdit>[0][]) =>
  sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base: tableFingerprint(invoices()), edits: ops.map((o) => newEdit(o)) } }));
const items = () => screen.getAllByRole("menuitem").map((m) => m.textContent);

describe("one column header row", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("has_one_header_row_with_the_chips_in_it", async () => {
    renderEditor();
    const h = await header("amount_cents");
    expect(h).toHaveAccessibleDescription(/bigint/);
    expect(h).toHaveTextContent(/int8/);
    expect(screen.queryByRole("list", { name: "Columns" })).toBeNull();
    const g = await grid();
    expect(within(g).getAllByRole("row").filter((r) => r.getAttribute("aria-rowindex") === "1")).toHaveLength(1);
    expect(within(g).getByRole("button", { name: "Column actions for amount_cents" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add column" })).toBeInTheDocument();
  });

  it("a_click_on_the_header_opens_the_column_form", async () => {
    const { user } = renderEditor();
    await user.click(await header("memo"));
    expect(await screen.findByRole("complementary", { name: "Edit column memo" })).toBeInTheDocument();
  });

  it("the_menu_sorts_first_then_clears", async () => {
    const { f, user } = renderEditor();
    const h = await header("amount_cents");
    await user.click(within(h).getByRole("button", { name: "Column actions for amount_cents" }));
    await screen.findByRole("menu");
    expect(items().slice(0, 2)).toEqual(["Sort ascending", "Sort descending"]);
    expect(items()).not.toContain("Clear sort");
    expect(items()).toContain("Rename");
    await user.click(screen.getByRole("menuitem", { name: "Sort descending" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy[0]).toEqual({ col: "amount_cents", dir: "desc" }));
    expect(await header("amount_cents")).toHaveAttribute("aria-sort", "descending");
    // No form opened: the menu click didn't reach the header.
    expect(screen.queryByRole("complementary", { name: /Edit column/ })).toBeNull();
    await user.click(within(await header("amount_cents")).getByRole("button", { name: "Column actions for amount_cents" }));
    await screen.findByRole("menu");
    expect(items().slice(0, 3)).toEqual(["Sort ascending", "Sort descending", "Clear sort"]);
    await user.click(screen.getByRole("menuitem", { name: "Clear sort" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy).toEqual([{ col: "id", dir: "asc" }]));
    expect(await header("amount_cents")).toHaveAttribute("aria-sort", "none");
  });

  it("the_toolbar_sort_shows_on_the_header", async () => {
    const { user } = renderEditor();
    await within(await grid()).findByRole("gridcell", { name: "45000" });
    await user.click(screen.getByRole("button", { name: "Sort" }));
    await user.click(screen.getByLabelText("Sort column"));
    await user.click(await screen.findByRole("option", { name: "memo" }));
    await user.click(screen.getByRole("button", { name: "Apply sort" }));
    await vi.waitFor(async () => expect(await header("memo")).toHaveAttribute("aria-sort", "ascending"));
    await user.click(within(await header("memo")).getByRole("button", { name: "Column actions for memo" }));
    await screen.findByRole("menu");
    expect(items()).toContain("Clear sort");
  });

  it("enter_opens_the_form_and_shift_f10_opens_the_menu", async () => {
    const { user } = renderEditor();
    const h = await header("memo");
    h.focus();
    await user.keyboard("{Shift>}{F10}{/Shift}");
    expect(await screen.findByRole("menuitem", { name: "Sort ascending" })).toBeInTheDocument();
    await user.keyboard("{Escape}");
    await vi.waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(document.activeElement).toBe(h);
    await user.keyboard("{Enter}");
    expect(await screen.findByRole("complementary", { name: "Edit column memo" })).toBeInTheDocument();
  });

  it("a_read_only_structure_keeps_sort_in_the_menu", async () => {
    draft([{ drop_table: { table: T } }]);
    const { f, user } = renderEditor();
    const h = await header("memo");
    await user.click(within(h).getByRole("button", { name: "Column actions for memo" }));
    await screen.findByRole("menu");
    expect(items()).toEqual(["Sort ascending", "Sort descending"]);
    await user.click(screen.getByRole("menuitem", { name: "Sort ascending" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy[0]).toEqual({ col: "memo", dir: "asc" }));
    expect(screen.getByRole("button", { name: "Add column" })).toBeDisabled();
  });

  it("a_pending_column_header_is_its_chip_and_reachable_by_keys", async () => {
    draft([{ add_column: { table: T, column: { name: "note", data_type: { schema: "pg_catalog", name: "text" } } } }]);
    const { user } = renderEditor();
    const h = await header("note");
    expect(h).toHaveAccessibleDescription(/new/);
    const last = await header("memo");
    last.focus();
    await user.keyboard("{End}");
    expect(document.activeElement).toBe(h);
    await user.keyboard("{Enter}");
    expect(await screen.findByRole("complementary", { name: /Edit column note/ })).toBeInTheDocument();
  });
});

describe("a plain grid header (no structure)", () => {
  beforeEach(() => { stubLayout(); });
  const columns = ["id", "memo"].map((name) => ({
    name,
    desc: { type: "text", nullable: true, has_default: false, generated: false, array: false, enum: null },
    labels: null,
    readOnly: false,
  }));
  function setup(over: Partial<GridProps> = {}) {
    const onSort = vi.fn();
    const props = {
      label: "public.t", columns, rows: [{ id: "r1", memo: "m1" }], firstRowNumber: 1, keys: ['["r1"]'],
      isSelected: () => false, pageSelection: "none", onToggleRow: () => {}, onTogglePage: () => {}, sort: null, onSort,
      canEdit: () => true, onSave: async () => null, onCopy: () => {}, widths: {}, onWidths: () => {},
      ...over,
    } as unknown as GridProps;
    const view = render(<Grid {...props} />);
    return { ...view, onSort, user: userEvent.setup() };
  }
  const h = (name: string) => within(screen.getByRole("grid", { name: "public.t" })).getByRole("columnheader", { name });

  it("a_click_and_enter_cycle_the_sort", async () => {
    const { onSort, user, rerender } = setup();
    await user.click(h("memo"));
    expect(onSort).toHaveBeenLastCalledWith("memo", "asc");
    const props = { label: "public.t", columns, rows: [{ id: "r1", memo: "m1" }], firstRowNumber: 1, keys: ['["r1"]'], isSelected: () => false, pageSelection: "none", onToggleRow: () => {}, onTogglePage: () => {}, onSort, canEdit: () => true, onSave: async () => null, onCopy: () => {}, widths: {}, onWidths: () => {} } as unknown as GridProps;
    rerender(<Grid {...props} sort={{ col: "memo", dir: "asc" }} />);
    h("memo").focus();
    await user.keyboard("{Enter}");
    expect(onSort).toHaveBeenLastCalledWith("memo", "desc");
    rerender(<Grid {...props} sort={{ col: "memo", dir: "desc" }} />);
    await user.click(h("memo"));
    expect(onSort).toHaveBeenLastCalledWith("memo", null);
  });

  it("the_menu_has_only_sort", async () => {
    const { onSort, user } = setup({ sort: { col: "memo", dir: "asc" } });
    await user.click(within(h("memo")).getByRole("button", { name: "Column actions for memo" }));
    await screen.findByRole("menu");
    expect(items()).toEqual(["Sort ascending", "Sort descending", "Clear sort"]);
    await user.click(screen.getByRole("menuitem", { name: "Sort descending" }));
    expect(onSort).toHaveBeenLastCalledWith("memo", "desc");
    expect(onSort).toHaveBeenCalledTimes(1);
  });
});
