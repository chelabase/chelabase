import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, ROWS, bodies, filterByTextKey, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { Grid, type GridProps } from "./grid";

/** The coder's edge cases for the cell menu and the active cell (rulings 1–4 of 2026-10-20 and the seams around them). */

const Q = `${B}/console/query`;
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const activeCells = () => Array.from(document.querySelectorAll('[data-active="true"]'));
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");

type Data = Record<string, unknown>[];
function server(data: Data): FakeHandler {
  return async (req) => {
    const { tree } = await req.clone().json();
    if (tree.op === "select") return jsonResponse(filterByTextKey(tree.where, data));
    if (tree.op === "batch") return jsonResponse(tree.ops.map(() => [{ id: "x" }]));
    return jsonResponse(data);
  };
}
function mount(o: Parameters<typeof renderEditor>[0] & { data?: Data } = {}) {
  const data: Data = o.data ?? ROWS.map((r) => ({ ...r }));
  return { data, ...renderEditor({ ...o, routes: { [`POST ${Q}`]: server(data), ...o.routes } }) };
}
type User = ReturnType<typeof mount>["user"];
const rightClick = (user: User, el: Element) => user.pointer({ keys: "[MouseRight]", target: el });
const menu = () => screen.findByRole("menu");
const items = (m: HTMLElement) => within(m).queryAllByRole("menuitem").map((e) => (e.textContent ?? "").trim());

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
});
afterEach(() => vi.restoreAllMocks());

/** The invoices table with one more column of this type, and rows carrying a value for it. */
function withColumn(name: string, dataType: string, typeOid: number, values: unknown[]) {
  const t = invoices();
  t.columns = [...t.columns, { ...t.columns[4], attnum: 6, name, data_type: dataType, type_oid: typeOid }];
  return { table: t as Table, data: ROWS.map((r, i) => ({ ...r, [name]: values[i] })) };
}

describe("ruling 1: Filter by this value and json", () => {
  it("a_json_cell_has_no_filter_item_a_jsonb_cell_has_one", async () => {
    const json = mount(withColumn("doc", "json", 114, [{ a: 1 }, { b: 2 }, { c: 3 }]));
    await rightClick(json.user, await cell('{"a":1}'));
    const m = await menu();
    expect(items(m)).toContain("Copy cell");
    expect(items(m)).not.toContain("Filter by this value");
    json.unmount();

    const jsonb = mount(withColumn("doc", "jsonb", 3802, [{ a: 1 }, { b: 2 }, { c: 3 }]));
    await rightClick(jsonb.user, await cell('{"a":1}'));
    expect(items(await menu())).toContain("Filter by this value");
  });

  it("a_json_array_cell_has_no_filter_item_either", async () => {
    const { user } = mount(withColumn("docs", "json[]", 199, [[{ a: 1 }], [], []]));
    await rightClick(user, await cell('[{"a":1}]'));
    expect(items(await menu())).not.toContain("Filter by this value");
  });
});

describe("ruling 3: Set to NULL on a NULL cell", () => {
  it("is_hidden_while_the_other_editing_items_stay", async () => {
    const { user } = mount();
    // Row 1's memo is NULL.
    await rightClick(user, await cell("NULL"));
    const m = await menu();
    expect(items(m)).toContain("Edit cell");
    expect(items(m)).not.toContain("Set to NULL");
  });
});

describe("ruling 4: the cell being edited keeps the browser's menu", () => {
  it("the_contextmenu_event_on_the_field_is_not_prevented", async () => {
    const { user } = mount();
    await user.dblClick(await cell("Q3 plan"));
    const input = await screen.findByRole("textbox", { name: "memo" });
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2 });
    input.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(false);
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("a_right_click_on_the_header_is_not_prevented_either", async () => {
    mount();
    const header = within(await grid()).getByRole("columnheader", { name: /memo/ });
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2 });
    header.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(false);
  });

  it("a_right_click_on_the_gutter_is_not_prevented", async () => {
    mount();
    await cell("Q3 plan");
    const gutter = screen.getByRole("checkbox", { name: "Select row 2" }).closest('[role="gridcell"]')!;
    const event = new MouseEvent("contextmenu", { bubbles: true, cancelable: true, button: 2 });
    gutter.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(false);
    expect(screen.queryByRole("menu")).toBeNull();
  });
});

describe("the menu and focus", () => {
  it("typing_in_the_menu_never_starts_an_edit_of_the_cell", async () => {
    const { user } = mount();
    await rightClick(user, await cell("Q3 plan"));
    await menu();
    await user.keyboard("x");
    expect(screen.queryByRole("textbox", { name: "memo" })).toBeNull();
  });

  it("edit_cell_leaves_focus_in_the_editor", async () => {
    const { user } = mount();
    await rightClick(user, await cell("Q3 plan"));
    await user.click(within(await menu()).getByRole("menuitem", { name: "Edit cell" }));
    const input = await screen.findByRole("textbox", { name: "memo" });
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(document.activeElement).toBe(input);
  });

  it("delete_row_leaves_focus_in_its_dialog", async () => {
    const { user } = mount();
    await rightClick(user, await cell("Q3 plan"));
    await user.click(within(await menu()).getByRole("menuitem", { name: "Delete row" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete 1 row?" });
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
  });

  it("deleting_an_unchecked_row_leaves_the_checked_rows_checked", async () => {
    const { user, f } = mount();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 1" }));
    await rightClick(user, await cell("void"));
    await user.click(within(await menu()).getByRole("menuitem", { name: "Delete row" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete 1 row?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete 1 row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(screen.getByText("1 selected")).toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: "Select row 1" })).toBeChecked();
  });

  it("a_copy_notice_without_checked_rows_shows_and_the_selection_bar_stays_hidden", async () => {
    const { user } = mount();
    vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue(undefined);
    await rightClick(user, await cell("Q3 plan"));
    await user.click(within(await menu()).getByRole("menuitem", { name: "Copy row (JSON)" }));
    expect(await screen.findByText("Copied 1 row as JSON")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Clear selection" })).toBeNull();
    expect(activeCells()).toEqual([await cell("Q3 plan")]);
  });
});

describe("ruling 2: row selection is part of the grid", () => {
  type Row = Record<string, unknown>;
  const ROWS2: Row[] = [
    { id: "r1", n: "10" },
    { id: "r2", n: "20" },
  ];
  const columns = ["id", "n"].map((name) => ({
    name,
    desc: { type: "text", nullable: true, has_default: false, generated: false, array: false, enum: null },
    labels: null,
    readOnly: false,
  }));
  function setup() {
    const onPresence = vi.fn();
    const onToggleRow = vi.fn();
    const onOpenRow = vi.fn();
    const props = {
      label: "public.t", columns, rows: ROWS2, firstRowNumber: 1, keys: ['["r1"]', '["r2"]'],
      isSelected: () => false, pageSelection: "none", onToggleRow, onTogglePage: () => {}, sort: null, onSort: () => {},
      canEdit: () => true, onSave: async () => null, onCopy: () => {}, widths: {}, onWidths: () => {}, onPresence, onOpenRow,
    } as unknown as GridProps;
    render(<Grid {...props} />);
    return { onPresence, onToggleRow, onOpenRow, user: userEvent.setup() };
  }
  const gridCell = (name: string) => within(screen.getByRole("grid", { name: "public.t" })).getByRole("gridcell", { name });
  const nothing = (fn: ReturnType<typeof vi.fn>) => fn.mock.calls.filter((c) => c[0].key === null).length;

  it("a_click_on_a_row_checkbox_keeps_the_active_cell_and_publishes_nothing", async () => {
    const { user, onPresence, onToggleRow } = setup();
    await user.click(gridCell("20"));
    const before = nothing(onPresence);
    await user.click(screen.getByRole("checkbox", { name: "Select row 1" }));
    expect(onToggleRow).toHaveBeenCalledWith('["r1"]');
    expect(activeCells()).toEqual([gridCell("20")]);
    expect(nothing(onPresence)).toBe(before);
  });

  it("a_click_on_the_gutter_or_its_open_row_button_keeps_the_active_cell", async () => {
    const { user, onOpenRow } = setup();
    await user.click(gridCell("20"));
    await user.click(screen.getByText("1"));
    expect(activeCells()).toEqual([gridCell("20")]);
    await user.click(screen.getByRole("button", { name: "Open row 1" }));
    expect(onOpenRow).toHaveBeenCalledWith(0);
    expect(activeCells()).toEqual([gridCell("20")]);
  });

  it("the_arrow_keys_into_the_gutter_clear_it_like_the_header", async () => {
    const { user, onPresence } = setup();
    await user.click(gridCell("r2"));
    await user.keyboard("{ArrowLeft}");
    expect(activeCells()).toEqual([]);
    expect(onPresence.mock.calls.at(-1)?.[0]).toEqual({ key: null, column: null, editing: false });
    await user.keyboard("{ArrowRight}");
    expect(activeCells()).toEqual([gridCell("r2")]);
  });

  it("tabbing_into_the_grid_selects_nothing_until_a_key_moves", async () => {
    const { user } = setup();
    await user.tab();
    expect(document.activeElement).toBe(gridCell("r1"));
    expect(activeCells()).toEqual([]);
    await user.keyboard("{ArrowDown}");
    expect(activeCells()).toEqual([gridCell("r2")]);
  });
});
