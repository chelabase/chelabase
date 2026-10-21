import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { stubLayout } from "@/test/table-editor-fixtures";
import { Grid, type GridProps } from "./grid";

/**
 * RT1 Playwright pass, fixes F1 and F2 (2026-10-21), the Grid's side.
 *
 * Pins for the coder (behaviour only; the mechanism is the coder's):
 * - F1: a pointer-down on ANOTHER body cell while an editor is open closes the editor (an unchanged value writes
 *   nothing: `onSave` is not called) and makes the pressed cell the active cell and the focused one. `onPresence`
 *   then publishes the pressed cell with `editing: false`; it never keeps publishing `{ key: old, editing: true }`.
 *   A pointer-down inside the open editor's own cell (its field) changes nothing.
 * - F2: with a CHANGED value the save goes through `onSave` as before (one call), and when it succeeds the pressed
 *   cell, not the edited one, ends up active and focused (the save's `focusCell(edited)` must not pull it back);
 *   presence ends on the pressed cell with `editing: false`.
 *   When the save FAILS the editor stays open on the old cell with its alert, the OLD cell is the active cell again
 *   (the press that started the save is undone), presence is `{ old row, old column, editing: true }` and focus is
 *   back in the editor's field so the user can retry.
 * - A double-click on another cell while editing ends with the editor open on the pressed cell, once the old editor
 *   is committed (changed value: through `onSave`, one call) or closed (unchanged). If the old save fails, the old
 *   editor stays (with its alert) and no editor opens on the pressed cell.
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
function setup(over: Partial<GridProps> = {}) {
  const { props, onPresence } = build(over);
  const view = render(
    <div>
      <button type="button">toolbar</button>
      <Grid {...props} />
    </div>,
  );
  return { ...view, props, onPresence, user: userEvent.setup() };
}
const grid = () => screen.getByRole("grid", { name: "public.t" });
const cell = (name: string) => within(grid()).getByRole("gridcell", { name });
const field = (name: string) => screen.queryByRole("textbox", { name });
const activeCells = () => Array.from(document.querySelectorAll('[data-active="true"]'));
const lastPresence = (fn: ReturnType<typeof vi.fn>) => fn.mock.calls.at(-1)?.[0];
const deferred = <T,>() => {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
};

beforeEach(() => stubLayout());

describe("F1: an unchanged editor and a press on another body cell", () => {
  it.each([
    ["another row, same column", "20", "30", { key: '["r3"]', column: "n", editing: false }],
    ["same row, another column", "20", "m2", { key: '["r2"]', column: "memo", editing: false }],
    ["another row and column", "20", "m3", { key: '["r3"]', column: "memo", editing: false }],
  ])("%s: the editor closes without a write and the pressed cell is active", async (_name, from, to, published) => {
    const onSave = vi.fn(async () => null);
    const { user, onPresence } = setup({ onSave });
    const edited = cell(from); // an open editor renames its cell: keep the element
    const pressed = cell(to);
    await user.dblClick(edited);
    await screen.findByRole("textbox", { name: "n" });
    await user.click(pressed);
    await waitFor(() => expect(field("n")).toBeNull());
    expect(onSave).not.toHaveBeenCalled();
    expect(activeCells()).toEqual([pressed]);
    expect(lastPresence(onPresence)).toEqual(published);
  });

  it("presence_never_keeps_publishing_the_old_cell_as_editing_after_the_press", async () => {
    const { user, onPresence } = setup();
    await user.dblClick(cell("20"));
    await screen.findByRole("textbox", { name: "n" });
    expect(lastPresence(onPresence)).toEqual({ key: '["r2"]', column: "n", editing: true });
    const before = onPresence.mock.calls.length;
    await user.click(cell("m3"));
    await waitFor(() => expect(lastPresence(onPresence)).toEqual({ key: '["r3"]', column: "memo", editing: false }));
    const after = onPresence.mock.calls.slice(before).map((c) => c[0]);
    expect(after.some((s) => s.editing === true)).toBe(false);
  });

  it("the_pressed_cell_is_focused_and_the_arrow_keys_go_on_from_it", async () => {
    const { user } = setup();
    await user.dblClick(cell("20"));
    await screen.findByRole("textbox", { name: "n" });
    const pressed = cell("m3");
    await user.click(pressed);
    await waitFor(() => expect(field("n")).toBeNull());
    expect(document.activeElement).toBe(pressed);
    await user.keyboard("{ArrowUp}");
    expect(document.activeElement).toBe(cell("m2"));
  });

  it("a_cell_that_cannot_be_edited_still_takes_the_press_and_closes_the_editor", async () => {
    const onSave = vi.fn(async () => null);
    const { user } = setup({ onSave, canEdit: (column: string) => column !== "memo" });
    await user.dblClick(cell("20"));
    await screen.findByRole("textbox", { name: "n" });
    const pressed = cell("m3");
    await user.click(pressed);
    await waitFor(() => expect(field("n")).toBeNull());
    expect(onSave).not.toHaveBeenCalled();
    expect(activeCells()).toEqual([pressed]);
    expect(field("memo")).toBeNull();
  });

  it("a_press_inside_the_open_editors_own_field_changes_nothing", async () => {
    const onSave = vi.fn(async () => null);
    const { user } = setup({ onSave });
    const edited = cell("20");
    await user.dblClick(edited);
    const input = await screen.findByRole("textbox", { name: "n" });
    await user.click(input);
    expect(field("n")).toBe(input);
    expect(onSave).not.toHaveBeenCalled();
    expect(activeCells()).toEqual([edited]);
  });
});

describe("F2: a changed value and a press on another body cell", () => {
  it("saves_once_then_the_pressed_cell_not_the_edited_one_is_active_and_focused", async () => {
    const save = deferred<string | null>();
    const onSave = vi.fn(() => save.promise);
    const { user, onPresence } = setup({ onSave });
    await user.dblClick(cell("20"));
    await user.type(await screen.findByRole("textbox", { name: "n" }), "9");
    const pressed = cell("m3");
    await user.click(pressed);
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(onSave).toHaveBeenCalledWith('["r2"]', expect.objectContaining({ name: "n" }), { null: false, text: "209" });
    save.resolve(null);
    await waitFor(() => expect(field("n")).toBeNull());
    expect(activeCells()).toEqual([pressed]);
    expect(document.activeElement).toBe(pressed);
    expect(lastPresence(onPresence)).toEqual({ key: '["r3"]', column: "memo", editing: false });
    expect(onSave).toHaveBeenCalledTimes(1);
  });

  it("an_instant_save_ends_the_same_way", async () => {
    const onSave = vi.fn(async () => null);
    const { user } = setup({ onSave });
    await user.dblClick(cell("20"));
    await user.type(await screen.findByRole("textbox", { name: "n" }), "9");
    const pressed = cell("30");
    await user.click(pressed);
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(field("n")).toBeNull());
    expect(activeCells()).toEqual([pressed]);
    expect(document.activeElement).toBe(pressed);
  });

  it("a_failed_save_keeps_the_editor_and_its_alert_and_the_old_cell_is_active_again", async () => {
    const onSave = vi.fn(async () => "Not a number");
    const { user, onPresence } = setup({ onSave });
    const edited = cell("20");
    await user.dblClick(edited);
    await user.type(await screen.findByRole("textbox", { name: "n" }), "x");
    await user.click(cell("m3"));
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(await screen.findByRole("alert")).toHaveTextContent("Not a number");
    expect(field("n")).toBeInTheDocument();
    expect(activeCells()).toEqual([edited]);
    expect(lastPresence(onPresence)).toEqual({ key: '["r2"]', column: "n", editing: true });
    expect(document.activeElement).toBe(field("n"));
  });
});

describe("a double-click on another cell while editing", () => {
  it("unchanged: the editor ends up open on the pressed cell, nothing is written", async () => {
    const onSave = vi.fn(async () => null);
    const { user, onPresence } = setup({ onSave });
    await user.dblClick(cell("20"));
    await screen.findByRole("textbox", { name: "n" });
    await user.dblClick(cell("m3"));
    const memo = await screen.findByRole("textbox", { name: "memo" });
    expect(memo).toHaveValue("m3");
    expect(field("n")).toBeNull();
    expect(onSave).not.toHaveBeenCalled();
    expect(lastPresence(onPresence)).toEqual({ key: '["r3"]', column: "memo", editing: true });
  });

  it("changed: the old value is saved once and the editor is open on the pressed cell afterwards", async () => {
    const save = deferred<string | null>();
    const onSave = vi.fn(() => save.promise);
    const { user, onPresence } = setup({ onSave });
    await user.dblClick(cell("20"));
    await user.type(await screen.findByRole("textbox", { name: "n" }), "9");
    await user.dblClick(cell("m3"));
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(onSave).toHaveBeenCalledWith('["r2"]', expect.objectContaining({ name: "n" }), { null: false, text: "209" });
    save.resolve(null);
    const memo = await screen.findByRole("textbox", { name: "memo" });
    expect(memo).toHaveValue("m3");
    await waitFor(() => expect(field("n")).toBeNull());
    // It stays: the finished save must not close the editor that opened meanwhile.
    await new Promise((r) => setTimeout(r, 30));
    expect(field("memo")).toBe(memo);
    expect(onSave).toHaveBeenCalledTimes(1);
    expect(lastPresence(onPresence)).toEqual({ key: '["r3"]', column: "memo", editing: true });
  });

  it("changed and the save fails: the old editor stays with its alert, none opens on the pressed cell", async () => {
    const onSave = vi.fn(async () => "Not a number");
    const { user } = setup({ onSave });
    const edited = cell("20");
    await user.dblClick(edited);
    await user.type(await screen.findByRole("textbox", { name: "n" }), "x");
    await user.dblClick(cell("m3"));
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(await screen.findByRole("alert")).toHaveTextContent("Not a number");
    await new Promise((r) => setTimeout(r, 30));
    expect(field("n")).toBeInTheDocument();
    expect(field("memo")).toBeNull();
    expect(activeCells()).toEqual([edited]);
  });
});
