import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, ROWS as TABLE_ROWS, bodies, filterByTextKey, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { Grid, type GridProps } from "./grid";

/**
 * RT1 browser feedback, round 2: the coder's edge cases beyond the RED set (grid-fz, rows-view-fz).
 * - an outside pointer-down with the other editors open (a textarea for arrays, CodeMirror for jsonb);
 * - two quick outside pointer-downs save once; a failed save is not retried by a second one with the same value;
 * - the viewer's colour changing while a cell is being edited;
 * - Enter keeps focusing the cell (only an outside pointer-down leaves it);
 * - through the table editor: a pointer-down in the row panel while editing commits and clears; the cell menu's
 *   "Open row" (and the gutter button) on a collapsed panel's row expands it with its draft kept.
 */

type Row = Record<string, unknown>;
const desc = (type: string, array = false) => ({ type, nullable: true, has_default: false, generated: false, array, enum: null });
const COLUMNS = [
  { name: "id", desc: desc("text"), labels: null, readOnly: false },
  { name: "n", desc: desc("text"), labels: null, readOnly: false },
  { name: "tags", desc: desc("text", true), labels: null, readOnly: false },
  { name: "doc", desc: desc("jsonb"), labels: null, readOnly: false },
];
const ROWS: Row[] = [
  { id: "r1", n: "10", tags: ["a"], doc: { k: 1 } },
  { id: "r2", n: "20", tags: ["b"], doc: { k: 2 } },
];
const KEYS = ['["r1"]', '["r2"]'];
const NOTHING = { key: null, column: null, editing: false };

function setup(over: Partial<GridProps> = {}) {
  const onPresence = vi.fn();
  const props = {
    label: "public.t", columns: COLUMNS, rows: ROWS, firstRowNumber: 1, keys: KEYS,
    isSelected: () => false, pageSelection: "none", onToggleRow: () => {}, onTogglePage: () => {}, sort: null, onSort: vi.fn(),
    canEdit: () => true, onSave: async () => null, onCopy: () => {}, widths: {}, onWidths: () => {}, onPresence,
    ...over,
  } as unknown as GridProps;
  const ui = (p: GridProps) => (
    <div>
      <button type="button">toolbar</button>
      <Grid {...p} />
      <p>below the grid</p>
    </div>
  );
  const view = render(ui(props));
  return { ...view, props, ui, onPresence, user: userEvent.setup() };
}
const grid = () => screen.getByRole("grid", { name: "public.t" });
const cell = (name: string) => within(grid()).getByRole("gridcell", { name });
const activeCells = () => Array.from(document.querySelectorAll('[data-active="true"]'));
const nulls = (fn: ReturnType<typeof vi.fn>) => fn.mock.calls.filter((c) => c[0].key === null).length;

describe("grid: the other editors and repeated presses", () => {
  beforeEach(() => stubLayout());

  it("an_array_textarea_editor_saves_its_changed_value_on_an_outside_press_and_clears", async () => {
    const onSave = vi.fn(async () => null);
    const { user, onPresence } = setup({ onSave });
    await user.dblClick(cell('["b"]'));
    const area = await screen.findByRole("textbox", { name: "tags" });
    expect(area.tagName).toBe("TEXTAREA");
    await user.clear(area);
    await user.type(area, '["c"]'.replace(/\[/g, "[["));
    const before = nulls(onPresence);
    await user.click(screen.getByText("below the grid"));
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(onSave).toHaveBeenCalledWith('["r2"]', expect.objectContaining({ name: "tags" }), { null: false, text: '["c"]' });
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "tags" })).toBeNull());
    expect(activeCells()).toEqual([]);
    await waitFor(() => expect(nulls(onPresence)).toBe(before + 1));
  });

  it("an_unchanged_codemirror_json_editor_closes_on_an_outside_press_without_a_save", async () => {
    const onSave = vi.fn(async () => null);
    const { user, onPresence } = setup({ onSave });
    await user.dblClick(cell('{"k":2}'));
    await screen.findByRole("textbox", { name: "doc" });
    expect(document.querySelector(".cm-editor")).not.toBeNull();
    await user.click(screen.getByRole("button", { name: "toolbar" }));
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "doc" })).toBeNull());
    expect(onSave).not.toHaveBeenCalled();
    expect(activeCells()).toEqual([]);
    expect(onPresence.mock.calls.at(-1)?.[0]).toEqual(NOTHING);
  });

  it("two_quick_outside_presses_save_once", async () => {
    let finish: (v: string | null) => void = () => {};
    const onSave = vi.fn(() => new Promise<string | null>((r) => (finish = r)));
    const { user, onPresence } = setup({ onSave });
    await user.dblClick(cell("20"));
    await user.type(await screen.findByRole("textbox", { name: "n" }), "9");
    const before = nulls(onPresence);
    await user.click(screen.getByText("below the grid"));
    await user.click(screen.getByRole("button", { name: "toolbar" }));
    expect(onSave).toHaveBeenCalledTimes(1);
    finish(null);
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "n" })).toBeNull());
    expect(onSave).toHaveBeenCalledTimes(1);
    expect(activeCells()).toEqual([]);
    await waitFor(() => expect(nulls(onPresence)).toBe(before + 1));
  });

  it("a_failed_save_is_not_retried_by_a_second_press_with_the_same_value_but_is_with_a_new_one", async () => {
    const onSave = vi.fn(async (_k: string, _c: unknown, v: { null: boolean; text?: string }) => (v.text === "20x" ? "Not a number" : null));
    const { user, onPresence } = setup({ onSave: onSave as unknown as GridProps["onSave"] });
    const c = cell("20");
    await user.dblClick(c);
    const input = await screen.findByRole("textbox", { name: "n" });
    await user.type(input, "x");
    await user.click(screen.getByText("below the grid"));
    expect(await screen.findByRole("alert")).toHaveTextContent("Not a number");
    await user.click(screen.getByText("below the grid"));
    expect(onSave).toHaveBeenCalledTimes(1);
    expect(c).toHaveAttribute("data-active", "true");
    const before = nulls(onPresence);
    await user.type(screen.getByRole("textbox", { name: "n" }), "{Backspace}1");
    await user.click(screen.getByText("below the grid"));
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "n" })).toBeNull());
    expect(onSave).toHaveBeenCalledTimes(2);
    expect(onSave).toHaveBeenLastCalledWith('["r2"]', expect.objectContaining({ name: "n" }), { null: false, text: "201" });
    expect(activeCells()).toEqual([]);
    await waitFor(() => expect(nulls(onPresence)).toBe(before + 1));
  });

  it("the_own_colour_changing_while_editing_restyles_the_cell_and_keeps_the_editor", async () => {
    const { user, props, ui, rerender } = setup({ activeColor: "var(--shell-light)" });
    const c = cell("20");
    await user.dblClick(c);
    const input = await screen.findByRole("textbox", { name: "n" });
    await user.type(input, "9");
    expect(c.style.outlineColor).toBe("var(--shell-light)");
    rerender(ui({ ...props, activeColor: "var(--coral)" }));
    expect(c.style.outlineColor).toBe("var(--coral)");
    expect(c.classList.contains("outline-sand")).toBe(false);
    expect(screen.getByRole("textbox", { name: "n" })).toHaveValue("209");
    expect(document.activeElement).toBe(screen.getByRole("textbox", { name: "n" }));
  });

  it("enter_still_saves_and_puts_focus_back_on_the_cell", async () => {
    const onSave = vi.fn(async () => null);
    const { user } = setup({ onSave });
    const c = cell("20");
    await user.dblClick(c);
    await user.type(await screen.findByRole("textbox", { name: "n" }), "9{Enter}");
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(document.activeElement).toBe(c));
    expect(c).toHaveAttribute("data-active", "true");
  });
});

const Q = `${B}/console/query`;
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");
function matches(row: Record<string, unknown>, w: any): boolean {
  if (!w) return true;
  if (w.and) return w.and.every((p: unknown) => matches(row, p));
  if ("is" in w) return (row[w.col] ?? null) === w.is;
  if ("eq" in w) return row[w.col] !== null && row[w.col] !== undefined && String(row[w.col]) === w.eq;
  return true;
}
function server(data: Row[]): FakeHandler {
  return async (req) => {
    const { tree } = await req.clone().json();
    if (tree.op === "select") return jsonResponse(filterByTextKey(tree.where, data));
    if (tree.op === "update") {
      const hit = data.filter((r) => matches(r, tree.where));
      for (const r of hit) Object.assign(r, tree.set);
      return jsonResponse(hit);
    }
    return jsonResponse(data);
  };
}
function mount() {
  const data: Row[] = TABLE_ROWS.map((r) => ({ ...r }));
  return renderEditor({ routes: { [`POST ${Q}`]: server(data) } });
}
const tableGrid = () => screen.findByRole("grid", { name: "public.invoices" });
const tableCell = async (text: string) => within(await tableGrid()).findByRole("gridcell", { name: text });
const openButton = (n: number) => screen.getByRole("button", { name: `Open row ${n}` });
const isOpenRow = async (text: string) => ((await tableCell(text)).closest('[role="row"]') as HTMLElement).classList.contains("bg-channel");

describe("table editor: the row panel while editing, and a collapsed panel's row", () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
    stubLayout();
  });
  afterEach(() => vi.restoreAllMocks());

  it("a_press_in_the_row_panel_while_editing_a_cell_commits_and_clears_it", async () => {
    const { f, user } = mount();
    await user.click(await screen.findByRole("button", { name: "Open row 1" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    const c = await tableCell("Q3 plan");
    await user.dblClick(c);
    const input = await within(await tableGrid()).findByRole("textbox", { name: "memo" });
    await user.type(input, "!");
    await user.click(within(panel).getByRole("heading"));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.set).toEqual({ memo: "Q3 plan!" });
    await vi.waitFor(() => expect(within(c).queryByRole("textbox")).toBeNull());
    expect(document.querySelectorAll('[data-active="true"]')).toHaveLength(0);
    expect(screen.getByRole("complementary", { name: "Row" })).toBeInTheDocument();
  });

  it.each([
    ["the cell menu's Open row", async (user: ReturnType<typeof userEvent.setup>) => {
      await user.pointer({ keys: "[MouseRight]", target: await tableCell("45000") });
      await user.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Open row" }));
    }],
    ["the gutter button", async (user: ReturnType<typeof userEvent.setup>) => user.click(openButton(2))],
  ])("%s on a collapsed panel's row expands it with its draft kept", async (_name, reopen) => {
    const { user } = mount();
    await user.click(await screen.findByRole("button", { name: "Open row 2" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    await user.type(within(panel).getByLabelText("memo"), " v2");
    await user.click(within(panel).getByRole("button", { name: "Collapse panel" }));
    expect(screen.queryByRole("complementary", { name: "Row" })).toBeNull();
    expect(openButton(2)).toHaveAttribute("aria-pressed", "false");
    await reopen(user);
    const again = await screen.findByRole("complementary", { name: "Row" });
    expect(within(again).getByLabelText("memo")).toHaveValue("Q3 plan v2");
    expect(openButton(2)).toHaveAttribute("aria-pressed", "true");
    expect(await isOpenRow("45000")).toBe(true);
  });
});
