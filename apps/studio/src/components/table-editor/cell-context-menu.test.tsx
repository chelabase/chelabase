import { screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, FULL, ROWS, bodies, filterByTextKey, invoices, meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/**
 * The body-cell context menu and the active cell in the table editor (RT1 browser feedback, 2026-10-20).
 *
 * Pins for the coder (all names are exact):
 * - the menu is `@base-ui/react`'s ContextMenu (`menu` / `menuitem` / `separator` roles, no new dependency);
 * - item labels: "Copy cell", "Copy row (TSV)", "Copy row (JSON)" (checked rows that include the clicked row:
 *   "Copy 2 rows (TSV)", "Copy 2 rows (JSON)"), "Edit cell", "Open row", "Filter by this value", "Set to NULL",
 *   "Delete row" ("Delete 2 rows" likewise); items that don't apply are absent, and separators sit only between
 *   groups that are present: Copy | Edit cell | Open row | Filter | Set to NULL | Delete;
 * - the active body cell has `data-active="true"` (see grid-active-cell.test.tsx);
 * - copy notices are shown wherever the copy came from (no checked rows needed): "Copied 1 row",
 *   "Copied 1 row as JSON", "The clipboard isn't available here.";
 * - "Filter by this value" adds `{ col, op: "eq", value: editText(value) }`, or `{ col, op: "is", value: "null" }` for NULL,
 *   after the filters already applied (AND), through the filter bar's own state, and is absent when `buildFilterTree`
 *   refuses that row;
 * - "Delete row" opens the existing confirmation ("Delete 1 row?" / button "Delete 1 row") and deletes through the
 *   existing keyed batch, for the clicked row alone unless it is among the checked rows;
 * - a right-click on the cell being edited opens no cell menu (the field keeps its own); a right-click on another cell
 *   closes the open editor (an unchanged one saves nothing) and opens the menu on the clicked cell;
 * - the menu's own pointer presses, and the row panel opening, never clear the active cell; Escape in the menu closes
 *   only the menu (the keydown must not reach the grid's Escape handler through the React portal tree).
 */

const Q = `${B}/console/query`;
const key = (id: string) => JSON.stringify([id]);
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");
const activeCells = () => Array.from(document.querySelectorAll('[data-active="true"]'));

type Data = Record<string, unknown>[];
function matches(row: Record<string, unknown>, w: any): boolean {
  if (!w) return true;
  if (w.and) return w.and.every((p: unknown) => matches(row, p));
  if ("is" in w) return (row[w.col] ?? null) === w.is;
  if ("eq" in w) return row[w.col] !== null && row[w.col] !== undefined && String(row[w.col]) === w.eq;
  return true;
}
/** A console answering selects from `data` and applying updates whose `where` still matches (a stale original matches nothing). */
function server(data: Data): FakeHandler {
  return async (req) => {
    const { tree } = await req.clone().json();
    if (tree.op === "select") return jsonResponse(filterByTextKey(tree.where, data));
    if (tree.op === "update") {
      const hit = data.filter((r) => matches(r, tree.where));
      for (const r of hit) Object.assign(r, tree.set);
      return jsonResponse(hit);
    }
    if (tree.op === "batch") return jsonResponse(tree.ops.map((o: any) => data.filter((r) => matches(r, o.where)).map((r) => ({ id: r.id }))));
    return jsonResponse(data);
  };
}

type Setup = Parameters<typeof renderEditor>[0];
function mount(o: Setup & { data?: Data } = {}) {
  const data: Data = o.data ?? ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ ...o, routes: { [`POST ${Q}`]: server(data), ...o.routes } });
  return { data, ...s };
}
type User = ReturnType<typeof mount>["user"];
const rightClick = (user: User, el: Element) => user.pointer({ keys: "[MouseRight]", target: el });
const menu = () => screen.findByRole("menu");
const names = (m: HTMLElement) =>
  Array.from(m.querySelectorAll('[role="menuitem"],[role="separator"]')).map((e) => (e.getAttribute("role") === "separator" ? "-" : (e.textContent ?? "").trim()));
const item = async (name: string) => within(await menu()).getByRole("menuitem", { name });
const pick = async (user: User, name: string) => user.click(await item(name));
const tsv = (rows: Record<string, unknown>[]) =>
  ["id\tcustomer_id\tstatus\tamount_cents\tmemo", ...rows.map((r) => [r.id, r.customer_id, r.status, r.amount_cents, r.memo ?? ""].join("\t"))].join("\n");
const copied = () => vi.spyOn(navigator.clipboard, "writeText");

const FULL_MENU = ["Copy cell", "Copy row (TSV)", "Copy row (JSON)", "-", "Edit cell", "-", "Open row", "-", "Filter by this value", "-", "Set to NULL", "-", "Delete row"];

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
});
afterEach(() => vi.restoreAllMocks());

describe("opening the menu", () => {
  it("a_right_click_makes_the_cell_active_and_opens_the_menu_with_every_group_in_order", async () => {
    const { user } = mount();
    const c = await cell("Q3 plan"); // taken before the menu opens: it hides the page from assistive tech
    await rightClick(user, c);
    expect(c).toHaveAttribute("data-active", "true");
    expect(names(await menu())).toEqual(FULL_MENU);
    expect(activeCells()).toHaveLength(1);
  });

  it("not_null_and_key_columns_hide_set_to_null_and_a_key_column_hides_edit_too", async () => {
    const { user } = mount();
    await rightClick(user, await cell("45000"));
    expect(names(await menu())).toEqual(["Copy cell", "Copy row (TSV)", "Copy row (JSON)", "-", "Edit cell", "-", "Open row", "-", "Filter by this value", "-", "Delete row"]);
    await user.keyboard("{Escape}");
    await rightClick(user, await cell("3f9a-1"));
    expect(names(await menu())).toEqual(["Copy cell", "Copy row (TSV)", "Copy row (JSON)", "-", "Open row", "-", "Filter by this value", "-", "Delete row"]);
  });

  it("shift_f10_and_the_context_menu_key_open_it_on_the_active_cell", async () => {
    const { user } = mount();
    const c = await cell("Q3 plan");
    await user.click(c);
    expect(c).toHaveAttribute("data-active", "true");
    await user.keyboard("{Shift>}{F10}{/Shift}");
    expect(names(await menu())).toEqual(FULL_MENU);
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(document.activeElement).toBe(c);
    await user.keyboard("{ContextMenu}");
    expect(names(await menu())).toEqual(FULL_MENU);
    const write = copied();
    await pick(user, "Copy cell");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith("Q3 plan"));
  });

  it("with_no_active_cell_the_keys_open_nothing", async () => {
    const { user } = mount();
    const c = await cell("Q3 plan");
    await user.click(c);
    expect(c).toHaveAttribute("data-active", "true");
    await user.keyboard("{Escape}");
    expect(activeCells()).toEqual([]);
    await user.keyboard("{Shift>}{F10}{/Shift}");
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("escape_closes_only_the_menu_and_focus_returns_to_the_cell_which_stays_active", async () => {
    const { user } = mount();
    const c = await cell("Q3 plan");
    await rightClick(user, c);
    await menu();
    expect(c).toHaveAttribute("data-active", "true");
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(c).toHaveAttribute("data-active", "true");
    expect(document.activeElement).toBe(c);
  });

  it("the_arrow_keys_move_through_the_items", async () => {
    const { user } = mount();
    await rightClick(user, await cell("Q3 plan"));
    const m = await menu();
    await user.keyboard("{ArrowDown}");
    expect(within(m).getByRole("menuitem", { name: "Copy cell" })).toHaveFocus();
    await user.keyboard("{ArrowDown}");
    expect(within(m).getByRole("menuitem", { name: "Copy row (TSV)" })).toHaveFocus();
  });

  it("an_outside_click_closes_the_menu_and_clears_the_selection", async () => {
    const { user } = mount();
    const c = await cell("Q3 plan");
    await rightClick(user, c);
    await menu();
    expect(c).toHaveAttribute("data-active", "true");
    await user.click(document.body);
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(activeCells()).toEqual([]);
  });

  it("a_right_click_on_another_cell_while_the_menu_is_open_retargets_it", async () => {
    const { user } = mount();
    const [first, second] = [await cell("45000"), await cell("Q3 plan")];
    await rightClick(user, first);
    await menu();
    await rightClick(user, second);
    await waitFor(() => expect(screen.getAllByRole("menu")).toHaveLength(1));
    expect(activeCells()).toEqual([second]);
    expect(names(await menu())).toEqual(FULL_MENU);
    const write = copied();
    await pick(user, "Copy cell");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith("Q3 plan"));
  });

  it("a_right_click_on_the_header_the_gutter_or_the_empty_area_opens_no_cell_menu", async () => {
    const { user } = mount();
    await rightClick(user, await cell("Q3 plan"));
    await menu(); // the menu does open on a body cell
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    const g = await grid();
    const targets = [
      within(g).getByRole("columnheader", { name: /memo/ }),
      screen.getByRole("checkbox", { name: "Select row 2" }).closest('[role="gridcell"]')!,
      document.querySelector("[data-grid-scroll]")!,
    ];
    for (const t of targets) {
      await rightClick(user, t);
      expect(screen.queryByRole("menu")).toBeNull();
    }
  });

  it("a_right_click_on_another_cell_while_editing_closes_the_unchanged_editor_without_a_save_and_opens_the_menu_there", async () => {
    const { user, f } = mount();
    const [edited, target] = [await cell("45000"), await cell("Q3 plan")];
    await user.dblClick(edited);
    await screen.findByRole("textbox", { name: "amount_cents" });
    await rightClick(user, target);
    expect(names(await menu())).toEqual(FULL_MENU);
    expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull();
    expect(target).toHaveAttribute("data-active", "true");
    expect(await writes(f)).toHaveLength(0);
  });

  it("a_right_click_on_the_cell_being_edited_leaves_the_field_its_own_menu", async () => {
    const { user } = mount();
    const c = await cell("Q3 plan");
    await rightClick(user, c); // the menu opens on a body cell (precondition)
    await menu();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    await user.dblClick(c);
    const input = await screen.findByRole("textbox", { name: "memo" });
    await rightClick(user, input);
    expect(screen.queryByRole("menu")).toBeNull();
    expect(screen.getByRole("textbox", { name: "memo" })).toBe(input);
  });

  it("neither_opening_the_menu_nor_using_it_publishes_no_selection_and_choosing_open_row_keeps_the_cell_active", async () => {
    const rt = new FakeRealtime();
    const { user } = mount({ realtime: rt });
    await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
    rt.joined();
    const c = await cell("Q3 plan");
    await rightClick(user, c);
    await menu();
    await vi.waitFor(() => expect(rt.table.presence.at(-1)).toEqual({ row: key("81c4-2"), column: "memo", editing: false }));
    // Focus moved into the menu; the 50 ms presence debounce has long passed.
    await new Promise((r) => setTimeout(r, 150));
    expect(rt.table.presence.at(-1)).toEqual({ row: key("81c4-2"), column: "memo", editing: false });
    await pick(user, "Open row");
    expect(await screen.findByRole("complementary", { name: "Row" })).toBeInTheDocument();
    expect(c).toHaveAttribute("data-active", "true");
    await new Promise((r) => setTimeout(r, 150));
    expect(rt.table.presence.at(-1)).toEqual({ row: key("81c4-2"), column: "memo", editing: false });
  });

  it("an_outside_click_publishes_no_selection_exactly_once", async () => {
    const rt = new FakeRealtime();
    const { user } = mount({ realtime: rt });
    await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
    rt.joined();
    const c = await cell("Q3 plan");
    await rightClick(user, c);
    await menu();
    expect(c).toHaveAttribute("data-active", "true");
    await vi.waitFor(() => expect(rt.table.presence.at(-1)?.row).toBe(key("81c4-2")));
    const before = rt.table.presence.length;
    await user.click(document.body);
    await vi.waitFor(() => expect(rt.table.presence.at(-1)).toEqual({ row: null, column: null, editing: false }));
    await new Promise((r) => setTimeout(r, 150));
    expect(rt.table.presence).toHaveLength(before + 1);
  });
});

describe("Copy", () => {
  it("copy_cell_writes_the_value_as_ctrl_c_does_and_a_null_cell_writes_an_empty_text", async () => {
    const { user } = mount();
    const c = await cell("45000");
    const write = copied();
    await rightClick(user, c);
    await pick(user, "Copy cell");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith("45000"));
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(document.activeElement).toBe(c);
    expect(c).toHaveAttribute("data-active", "true");
    await rightClick(user, await cell("NULL"));
    await pick(user, "Copy cell");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith(""));
  });

  it("copy_row_tsv_and_json_copy_the_clicked_row_and_say_so", async () => {
    const { user } = mount();
    const write = copied();
    await rightClick(user, await cell("Q3 plan"));
    await pick(user, "Copy row (TSV)");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith(tsv([ROWS[1]])));
    expect(await screen.findByText("Copied 1 row")).toBeInTheDocument();
    await rightClick(user, await cell("Q3 plan"));
    await pick(user, "Copy row (JSON)");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith(JSON.stringify([ROWS[1]], null, 2)));
    expect(await screen.findByText("Copied 1 row as JSON")).toBeInTheDocument();
  });

  it("checked_rows_that_include_the_clicked_row_are_what_copy_row_and_delete_row_act_on", async () => {
    const { user } = mount();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 1" }));
    await user.click(screen.getByRole("checkbox", { name: "Select row 2" }));
    const write = copied();
    await rightClick(user, await cell("Q3 plan"));
    expect(names(await menu())).toEqual(["Copy cell", "Copy 2 rows (TSV)", "Copy 2 rows (JSON)", "-", "Edit cell", "-", "Open row", "-", "Filter by this value", "-", "Set to NULL", "-", "Delete 2 rows"]);
    await pick(user, "Copy 2 rows (TSV)");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith(tsv([ROWS[0], ROWS[1]])));
    await rightClick(user, await cell("Q3 plan"));
    await pick(user, "Copy 2 rows (JSON)");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith(JSON.stringify([ROWS[0], ROWS[1]], null, 2)));
  });

  it("checked_rows_without_the_clicked_row_leave_the_menu_on_the_clicked_row_alone", async () => {
    const { user } = mount();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 1" }));
    const write = copied();
    await rightClick(user, await cell("Q3 plan"));
    const m = await menu();
    expect(names(m)).toContain("Copy row (TSV)");
    expect(names(m)).toContain("Delete row");
    await pick(user, "Copy row (TSV)");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith(tsv([ROWS[1]])));
  });

  it.each(["Copy cell", "Copy row (TSV)"])("%s with the clipboard refusing shows the existing notice", async (label) => {
    const { user } = mount();
    const write = copied();
    write.mockRejectedValue(new Error("denied"));
    await rightClick(user, await cell("Q3 plan"));
    await pick(user, label);
    expect(await screen.findByText("The clipboard isn't available here.")).toBeInTheDocument();
  });

  it("copy_row_without_a_clipboard_api_shows_the_existing_notice", async () => {
    const { user } = mount();
    const nav = window.navigator;
    const own = Object.getOwnPropertyDescriptor(nav, "clipboard");
    await rightClick(user, await cell("Q3 plan"));
    const entry = await item("Copy cell");
    Object.defineProperty(nav, "clipboard", { value: undefined, configurable: true });
    try {
      await user.click(entry);
      expect(await screen.findByText("The clipboard isn't available here.")).toBeInTheDocument();
    } finally {
      if (own) Object.defineProperty(nav, "clipboard", own);
    }
  });

  it("a_table_without_a_primary_key_copies_the_clicked_row_too", async () => {
    const { user } = mount({ table: invoices({ constraints: [] }) });
    const write = copied();
    await rightClick(user, await cell("Q3 plan"));
    await pick(user, "Copy row (TSV)");
    await waitFor(() => expect(write).toHaveBeenLastCalledWith(tsv([ROWS[1]])));
  });
});

describe("Edit cell and Open row", () => {
  it("edit_cell_opens_the_editor_like_a_double_click_and_saves_with_the_same_conflict_check", async () => {
    const { user, f } = mount();
    await rightClick(user, await cell("45000"));
    await pick(user, "Edit cell");
    const input = await screen.findByRole("textbox", { name: "amount_cents" });
    expect(input).toHaveValue("45000");
    await user.clear(input);
    await user.type(input, "46000{Enter}");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({
      v: 1, op: "update", table: "public.invoices", set: { amount_cents: 46000 },
      where: { and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "45000", text: true }] }, returning: "*",
    });
  });

  it("open_row_opens_the_side_panel_for_the_clicked_row_and_the_cell_stays_active", async () => {
    const { user } = mount();
    const c = await cell("Q3 plan");
    await rightClick(user, c);
    await pick(user, "Open row");
    const panel = await screen.findByRole("complementary", { name: "Row" });
    expect(within(panel).getByLabelText("memo")).toHaveValue("Q3 plan");
    expect(c).toHaveAttribute("data-active", "true");
  });

  it("a_table_without_a_primary_key_has_no_edit_open_set_null_or_delete", async () => {
    const { user } = mount({ table: invoices({ constraints: [] }) });
    await rightClick(user, await cell("Q3 plan"));
    expect(names(await menu())).toEqual(["Copy cell", "Copy row (TSV)", "Copy row (JSON)", "-", "Filter by this value"]);
  });

  it("a_viewer_gets_copy_open_row_and_filter_only", async () => {
    const { user } = mount({ me: meAs("viewer", ["reporter"]) });
    await rightClick(user, await cell("Q3 plan"));
    expect(names(await menu())).toEqual(["Copy cell", "Copy row (TSV)", "Copy row (JSON)", "-", "Open row", "-", "Filter by this value"]);
  });

  it("a_view_gets_copy_and_filter_only", async () => {
    const { user } = mount({ params: { ...FULL, table: "v_open" } });
    const c = await within(await screen.findByRole("grid", { name: "public.v_open" })).findByRole("gridcell", { name: "Q3 plan" });
    await rightClick(user, c);
    expect(names(await menu())).toEqual(["Copy cell", "Copy row (TSV)", "Copy row (JSON)", "-", "Filter by this value"]);
  });
});

describe("Filter by this value", () => {
  const openFilter = async (user: User) => user.click(screen.getByRole("button", { name: "Filter" }));

  it("adds_column_equals_value_to_the_filter_bar_and_reads_the_filtered_page", async () => {
    const { user, f } = mount();
    await rightClick(user, await cell("void"));
    await pick(user, "Filter by this value");
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.where).toEqual({ col: "status", eq: "void" }));
    await openFilter(user);
    expect(await screen.findByRole("button", { name: "Remove filter status eq void" })).toBeInTheDocument();
  });

  it("a_second_filter_is_added_after_the_applied_ones", async () => {
    const { user, f } = mount();
    await rightClick(user, await cell("void"));
    await pick(user, "Filter by this value");
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.where).toEqual({ col: "status", eq: "void" }));
    await rightClick(user, await cell("45000"));
    await pick(user, "Filter by this value");
    await vi.waitFor(async () =>
      expect((await selects(f)).at(-1).tree.where).toEqual({ and: [{ col: "status", eq: "void" }, { col: "amount_cents", eq: 45000 }] }),
    );
  });

  it("a_null_cell_filters_on_is_null", async () => {
    const { user, f } = mount();
    await rightClick(user, await cell("NULL"));
    await pick(user, "Filter by this value");
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.where).toEqual({ col: "memo", is: null }));
    await openFilter(user);
    expect(await screen.findByRole("button", { name: "Remove filter memo is null" })).toBeInTheDocument();
  });

  it("the_filter_changes_the_rows_under_the_cell_so_the_selection_clears", async () => {
    const { user } = mount();
    const c = await cell("void");
    await rightClick(user, c);
    expect(c).toHaveAttribute("data-active", "true");
    await pick(user, "Filter by this value");
    await vi.waitFor(() => expect(activeCells()).toEqual([]));
  });

  it("a_value_the_filter_bar_would_refuse_has_no_filter_item", async () => {
    const t = invoices();
    t.columns = [...t.columns, { ...t.columns[4], attnum: 6, name: "meta", data_type: "jsonb", type_oid: 3802 }];
    const lossy = { n: 12345678901234567890 }; // JSON.stringify gives 12345678901234567000: not the same number
    const { user } = mount({
      table: t as Table,
      data: [
        { ...ROWS[0], meta: lossy },
        { ...ROWS[1], meta: { a: 1 } },
      ],
    });
    await rightClick(user, await cell('{"a":1}'));
    expect(names(await menu())).toContain("Filter by this value");
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    await rightClick(user, await cell(JSON.stringify(lossy)));
    const m = await menu();
    expect(names(m)).toContain("Copy cell");
    expect(names(m)).not.toContain("Filter by this value");
  });
});

describe("Set to NULL", () => {
  it("goes_through_the_edit_save_path_with_the_conflict_check_originals", async () => {
    const { user, f, data } = mount();
    const memo = await cell("Q3 plan"); // row 2's memo (row 1's is already NULL)
    await rightClick(user, memo);
    await pick(user, "Set to NULL");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({
      v: 1, op: "update", table: "public.invoices", set: { memo: null },
      where: { and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "memo", eq: "Q3 plan", text: true }] }, returning: "*",
    });
    await vi.waitFor(() => expect(data[1].memo).toBeNull());
    await vi.waitFor(() => expect(memo).toHaveTextContent("NULL"));
  });

  it("a_row_changed_elsewhere_gives_the_conflict_dialog_and_changes_nothing", async () => {
    const { user, data } = mount();
    await cell("Q3 plan");
    data[1].memo = "changed elsewhere"; // this page still shows "Q3 plan"
    await rightClick(user, await cell("Q3 plan"));
    await pick(user, "Set to NULL");
    expect(await screen.findByRole("dialog", { name: "Changed by someone else" })).toBeInTheDocument();
    expect(data[1].memo).toBe("changed elsewhere");
  });

  it("a_read_only_role_has_no_set_to_null", async () => {
    const { user } = mount({ me: meAs("viewer", ["reporter"]) });
    await rightClick(user, await cell("Q3 plan"));
    expect(names(await menu())).not.toContain("Set to NULL");
  });
});

describe("Delete row", () => {
  it("asks_first_and_deletes_the_clicked_row_through_the_keyed_batch", async () => {
    const { user, f } = mount();
    await rightClick(user, await cell("Q3 plan"));
    await pick(user, "Delete row");
    const dialog = await screen.findByRole("dialog", { name: "Delete 1 row?" });
    expect(await writes(f)).toHaveLength(0);
    await user.click(within(dialog).getByRole("button", { name: "Delete 1 row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({
      v: 1, op: "batch", ops: [{ op: "delete", from: "public.invoices", where: { col: "id", eq: "81c4-2", parse: true }, returning: ["id"] }],
    });
  });

  it("cancelling_the_confirmation_deletes_nothing", async () => {
    const { user, f } = mount();
    await rightClick(user, await cell("Q3 plan"));
    await pick(user, "Delete row");
    const dialog = await screen.findByRole("dialog", { name: "Delete 1 row?" });
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(await writes(f)).toHaveLength(0);
  });

  it("checked_rows_that_include_the_clicked_row_are_deleted_together", async () => {
    const { user, f } = mount();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 1" }));
    await user.click(screen.getByRole("checkbox", { name: "Select row 3" }));
    await rightClick(user, await cell("void"));
    await pick(user, "Delete 2 rows");
    const dialog = await screen.findByRole("dialog", { name: "Delete 2 rows?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete 2 rows" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.ops.map((o: any) => o.where.eq)).toEqual(["3f9a-1", "c2d1-3"]);
  });

  it("checked_rows_without_the_clicked_row_are_left_alone", async () => {
    const { user, f } = mount();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 1" }));
    await rightClick(user, await cell("void"));
    await pick(user, "Delete row");
    const dialog = await screen.findByRole("dialog", { name: "Delete 1 row?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete 1 row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.ops.map((o: any) => o.where.eq)).toEqual(["c2d1-3"]);
  });
});

describe("the active cell in the table editor", () => {
  it("a_click_outlines_the_cell_and_escape_clears_it", async () => {
    const { user } = mount();
    const c = await cell("45000");
    await user.click(c);
    expect(c).toHaveAttribute("data-active", "true");
    await user.keyboard("{Escape}");
    expect(activeCells()).toEqual([]);
  });
});
