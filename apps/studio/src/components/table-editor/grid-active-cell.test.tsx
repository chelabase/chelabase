import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { stubLayout } from "@/test/table-editor-fixtures";
import { Grid, type GridProps } from "./grid";

/**
 * The grid's persistent active cell (RT1 browser feedback, 2026-10-20): drawn for its owner with a 2px
 * inset sand outline, for mouse and keyboard selection alike, whatever holds focus.
 *
 * Pins for the coder:
 * - the active body cell (a data column of a body row) carries `data-active="true"`; no other cell has the attribute;
 * - and the unprefixed classes `outline-2`, `-outline-offset-2` and `outline-sand` (jsdom has no layout), which no
 *   other cell has (`focus-visible:outline-sand` in `cellBase` is a different token);
 * - nothing is active until a cell is clicked or reached by the keys (the roving tabindex position is not a selection);
 * - the presence publish is `onPresence({ key, column, editing })`; "nothing selected" is `{ key: null, column: null, editing: false }`.
 */

type Row = Record<string, unknown>;
const ROWS: Row[] = [
  { id: "r1", n: "10", memo: "m1" },
  { id: "r2", n: "20", memo: "m2" },
  { id: "r3", n: "30", memo: "m3" },
];
const KEYS = ['["r1"]', '["r2"]', '["r3"]'];
const columns = ["id", "n", "memo"].map((name) => ({
  name,
  desc: { type: "text", nullable: true, has_default: false, generated: false, array: false, enum: null },
  labels: null,
  readOnly: false,
}));
const NOTHING = { key: null, column: null, editing: false };

function build(over: Partial<GridProps> = {}) {
  const onPresence = vi.fn();
  const props = {
    label: "public.t", columns, rows: ROWS, firstRowNumber: 1, keys: KEYS,
    isSelected: () => false, pageSelection: "none", onToggleRow: () => {}, onTogglePage: () => {}, sort: null, onSort: vi.fn(),
    canEdit: () => true, onSave: async () => null, onCopy: () => {}, widths: {}, onWidths: () => {}, onPresence,
    ...over,
  } as unknown as GridProps;
  return { props, onPresence };
}
const ui = (props: GridProps) => (
  <div>
    <button type="button">toolbar</button>
    <Grid {...props} />
    <p>below the grid</p>
  </div>
);
function setup(over: Partial<GridProps> = {}) {
  const { props, onPresence } = build(over);
  const view = render(ui(props));
  return { ...view, props, onPresence, user: userEvent.setup() };
}
const cell = (name: string) => within(screen.getByRole("grid", { name: "public.t" })).getByRole("gridcell", { name });
const activeCells = () => Array.from(document.querySelectorAll('[data-active="true"]'));
const lastPresence = (fn: ReturnType<typeof vi.fn>) => fn.mock.calls.at(-1)?.[0];
const nothingPublished = (fn: ReturnType<typeof vi.fn>) => fn.mock.calls.filter((c) => c[0].key === null).length;

beforeEach(() => stubLayout());

describe("the active cell is drawn for its owner", () => {
  it("a_click_makes_the_cell_active_with_a_sand_outline_and_moves_with_the_next_click", async () => {
    const { user } = setup();
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    for (const c of ["outline-2", "-outline-offset-2", "outline-sand"]) expect(cell("20").classList.contains(c)).toBe(true);
    await user.click(cell("m3"));
    expect(cell("m3")).toHaveAttribute("data-active", "true");
    expect(activeCells()).toEqual([cell("m3")]);
    for (const c of ["outline-2", "-outline-offset-2", "outline-sand"]) expect(cell("20").classList.contains(c)).toBe(false);
  });

  it("nothing_is_active_before_a_selection", async () => {
    // The roving tabindex position (row 0, column 1) is not a selection.
    setup();
    await screen.findByRole("grid", { name: "public.t" });
    expect(activeCells()).toEqual([]);
    expect(cell("r1").classList.contains("outline-sand")).toBe(false);
  });

  it("it_does_not_depend_on_focus", async () => {
    const { user } = setup();
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    // Focus moves away without a pointer press (a menu, a panel, the browser chrome).
    screen.getByRole("button", { name: "toolbar" }).focus();
    expect(cell("20")).toHaveAttribute("data-active", "true");
    expect(activeCells()).toEqual([cell("20")]);
  });

  it("the_arrow_keys_move_the_outline_and_the_header_row_holds_none", async () => {
    const { user } = setup();
    await user.click(cell("10"));
    expect(cell("10")).toHaveAttribute("data-active", "true");
    await user.keyboard("{ArrowDown}");
    expect(cell("20")).toHaveAttribute("data-active", "true");
    await user.keyboard("{ArrowRight}");
    expect(cell("m2")).toHaveAttribute("data-active", "true");
    expect(activeCells()).toEqual([cell("m2")]);
    await user.keyboard("{ArrowUp}{ArrowUp}");
    // On the header row the selection is no body cell: nothing is outlined.
    expect(activeCells()).toEqual([]);
  });

  it("the_keyboard_model_is_unchanged_enter_edits_and_ctrl_c_copies_the_active_cell", async () => {
    const onCopy = vi.fn();
    const { user } = setup({ onCopy });
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    await user.keyboard("{Control>}c{/Control}");
    expect(onCopy).toHaveBeenCalledWith({ row: 1, column: "n" });
    await user.keyboard("{Enter}");
    expect(await screen.findByRole("textbox", { name: "n" })).toHaveValue("20");
  });
});

describe("clearing on a pointer-down outside the body cells", () => {
  it.each([
    ["the toolbar", () => screen.getByRole("button", { name: "toolbar" })],
    ["the header", () => within(screen.getByRole("grid", { name: "public.t" })).getByRole("columnheader", { name: /memo/ })],
    ["the empty grid area", () => document.querySelector("[data-grid-scroll]") as HTMLElement],
    ["anywhere else in Studio", () => screen.getByText("below the grid")],
  ])("%s clears the active cell and publishes no selection once", async (_name, target) => {
    const { user, onPresence } = setup();
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    expect(lastPresence(onPresence)).toEqual({ key: '["r2"]', column: "n", editing: false });
    const before = nothingPublished(onPresence);
    await user.click(target());
    expect(activeCells()).toEqual([]);
    expect(lastPresence(onPresence)).toEqual(NOTHING);
    expect(nothingPublished(onPresence)).toBe(before + 1);
  });

  it("a_pointer_down_on_another_body_cell_moves_the_selection_and_never_publishes_nothing", async () => {
    const { user, onPresence } = setup();
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    const before = nothingPublished(onPresence);
    await user.click(cell("m3"));
    expect(cell("m3")).toHaveAttribute("data-active", "true");
    expect(lastPresence(onPresence)).toEqual({ key: '["r3"]', column: "memo", editing: false });
    expect(nothingPublished(onPresence)).toBe(before);
  });

  it("presence_follows_the_active_cell_not_the_focus", async () => {
    const { user, onPresence } = setup();
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    const calls = onPresence.mock.calls.length;
    screen.getByRole("button", { name: "toolbar" }).focus();
    expect(onPresence.mock.calls.length).toBe(calls);
    expect(lastPresence(onPresence)).toEqual({ key: '["r2"]', column: "n", editing: false });
  });
});

describe("Escape", () => {
  it("clears_the_active_cell_when_nothing_is_being_edited_and_publishes_once", async () => {
    const { user, onPresence } = setup();
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    const before = nothingPublished(onPresence);
    await user.keyboard("{Escape}");
    expect(activeCells()).toEqual([]);
    expect(lastPresence(onPresence)).toEqual(NOTHING);
    expect(nothingPublished(onPresence)).toBe(before + 1);
    // Already clear: another Escape, then an outside press, publish nothing more.
    await user.keyboard("{Escape}");
    await user.click(screen.getByText("below the grid"));
    expect(nothingPublished(onPresence)).toBe(before + 1);
  });

  it("while_editing_the_first_escape_cancels_the_edit_keeps_the_cell_active_and_focused_the_second_clears", async () => {
    const onSave = vi.fn(async () => null);
    const { user, onPresence } = setup({ onSave });
    const c = cell("20"); // an open editor renames the cell: keep the element
    await user.dblClick(c);
    const input = await screen.findByRole("textbox", { name: "n" });
    expect(c).toHaveAttribute("data-active", "true");
    expect(lastPresence(onPresence)).toEqual({ key: '["r2"]', column: "n", editing: true });
    await user.type(input, "9");
    await user.keyboard("{Escape}");
    expect(c).toHaveAttribute("data-active", "true");
    expect(screen.queryByRole("textbox", { name: "n" })).toBeNull();
    expect(document.activeElement).toBe(c);
    expect(lastPresence(onPresence)).toEqual({ key: '["r2"]', column: "n", editing: false });
    expect(onSave).not.toHaveBeenCalled();
    await user.keyboard("{Escape}");
    expect(activeCells()).toEqual([]);
    expect(lastPresence(onPresence)).toEqual(NOTHING);
  });
});

describe("another view puts other rows under the cell", () => {
  // The editor is dropped by the same changes (resetKey, the page offset, the sort); the selection goes with it.
  it.each([
    ["the view key", { resetKey: "other" }],
    ["the page offset", { firstRowNumber: 51 }],
    ["the sort", { sort: { col: "n", dir: "asc" } }],
  ])("a change of %s clears the active cell and publishes no selection", async (_name, patch) => {
    const { props, onPresence, rerender, user } = setup({ resetKey: "v1" });
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    rerender(ui({ ...props, ...patch } as GridProps));
    expect(activeCells()).toEqual([]);
    expect(lastPresence(onPresence)).toEqual(NOTHING);
  });

  it("a_rerender_of_the_same_view_keeps_the_active_cell", async () => {
    const { props, rerender, user } = setup({ resetKey: "v1" });
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    rerender(ui({ ...props, rows: ROWS.map((r) => ({ ...r })) }));
    expect(cell("20")).toHaveAttribute("data-active", "true");
  });
});
