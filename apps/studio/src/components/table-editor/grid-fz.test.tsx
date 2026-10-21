import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { stubLayout } from "@/test/table-editor-fixtures";
import { Grid, type GridProps } from "./grid";

/**
 * RT1 browser feedback, round 2 (2026-10-21), the Grid's side.
 *
 * Pins for the coder (exact names):
 * - new optional prop `activeColor?: string` on `GridProps`: a CSS colour (a `var(--x)` token as `presenceColor(seed)`
 *   returns it). When set, the active body cell carries it as an inline `outline-color` (style attribute) and does NOT
 *   carry the class `outline-sand`; it keeps `outline-2` and `-outline-offset-2`. When undefined (no presence colour
 *   yet), the cell keeps the class `outline-sand` and has no inline outline colour (the previous behaviour).
 * - the gutter's open-row button always has `aria-pressed`: "true" when its row's key equals `openKey`, "false" otherwise.
 *   Its accessible name stays `Open row N` (the codebase's toggle convention, see table-list.tsx).
 * - a pointer-down outside the grid's body cells (and outside the cell menu) while an editor is open commits the edit like
 *   the editor's blur does (saving a changed value through `onSave`, nothing for an unchanged one), and once that
 *   succeeds, closes the editor, clears the active cell (`onPresence` publishes `{ key: null, column: null, editing: false }`
 *   once) and does NOT move focus back into the grid. A failed save leaves the editor open (with its alert) and the
 *   cell active (nothing cleared, nothing null published).
 * - the gutter (checkbox, open-row button) is not "outside": it keeps the active cell, as before.
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
const SHELL = "var(--shell-light)";

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
const grid = () => screen.getByRole("grid", { name: "public.t" });
const cell = (name: string) => within(grid()).getByRole("gridcell", { name });
const activeCells = () => Array.from(document.querySelectorAll('[data-active="true"]'));
const lastPresence = (fn: ReturnType<typeof vi.fn>) => fn.mock.calls.at(-1)?.[0];
const nothingPublished = (fn: ReturnType<typeof vi.fn>) => fn.mock.calls.filter((c) => c[0].key === null).length;
const outlineColor = (el: Element) => (el as HTMLElement).style.outlineColor;

beforeEach(() => stubLayout());

describe("bug 1: the active cell is drawn in the viewer's own presence colour", () => {
  it("activeColor_is_the_inline_outline_colour_instead_of_the_sand_class", async () => {
    const { user } = setup({ activeColor: SHELL });
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    expect(outlineColor(cell("20"))).toBe(SHELL);
    for (const c of ["outline-2", "-outline-offset-2"]) expect(cell("20").classList.contains(c)).toBe(true);
    expect(cell("20").classList.contains("outline-sand")).toBe(false);
  });

  it("only_the_active_cell_carries_the_colour_and_it_moves_with_the_selection", async () => {
    const { user } = setup({ activeColor: SHELL });
    await user.click(cell("20"));
    await user.click(cell("m3"));
    expect(outlineColor(cell("m3"))).toBe(SHELL);
    expect(outlineColor(cell("20"))).toBe("");
    expect(cell("20").classList.contains("outline-2")).toBe(false);
    expect(activeCells()).toEqual([cell("m3")]);
  });

  it("without_activeColor_the_cell_keeps_the_sand_class_and_no_inline_colour", async () => {
    const { user } = setup();
    await user.click(cell("20"));
    expect(cell("20")).toHaveAttribute("data-active", "true");
    expect(cell("20").classList.contains("outline-sand")).toBe(true);
    expect(outlineColor(cell("20"))).toBe("");
  });

  it("a_colour_arriving_or_leaving_while_a_cell_is_active_restyles_it", async () => {
    const { props, rerender, user } = setup();
    await user.click(cell("20"));
    expect(cell("20").classList.contains("outline-sand")).toBe(true);
    rerender(ui({ ...props, activeColor: SHELL }));
    expect(outlineColor(cell("20"))).toBe(SHELL);
    expect(cell("20").classList.contains("outline-sand")).toBe(false);
    rerender(ui({ ...props, activeColor: undefined }));
    expect(outlineColor(cell("20"))).toBe("");
    expect(cell("20").classList.contains("outline-sand")).toBe(true);
  });
});

describe("bug 2: the gutter's open-row button is a toggle", () => {
  const openButton = (n: number) => screen.getByRole("button", { name: `Open row ${n}` });

  it("aria_pressed_is_true_only_on_the_row_whose_key_is_openKey", () => {
    setup({ openKey: KEYS[1], onOpenRow: vi.fn() });
    expect(openButton(2)).toHaveAttribute("aria-pressed", "true");
    expect(openButton(1)).toHaveAttribute("aria-pressed", "false");
    expect(openButton(3)).toHaveAttribute("aria-pressed", "false");
  });

  it("with_no_openKey_every_button_is_not_pressed", () => {
    setup({ openKey: null, onOpenRow: vi.fn() });
    for (const n of [1, 2, 3]) expect(openButton(n)).toHaveAttribute("aria-pressed", "false");
  });

  it("the_open_row_styling_follows_openKey", () => {
    const { props, rerender } = setup({ openKey: KEYS[1], onOpenRow: vi.fn() });
    const row = (n: string) => cell(n).closest('[role="row"]') as HTMLElement;
    expect(row("20").classList.contains("bg-channel")).toBe(true);
    expect(row("10").classList.contains("bg-channel")).toBe(false);
    rerender(ui({ ...props, openKey: null }));
    expect(row("20").classList.contains("bg-channel")).toBe(false);
  });
});

describe("bug 4: a pointer-down outside the body cells while editing", () => {
  it.each([
    ["the toolbar", () => screen.getByRole("button", { name: "toolbar" })],
    ["a column header", () => within(grid()).getByRole("columnheader", { name: /memo/ })],
    ["anywhere else in Studio", () => screen.getByText("below the grid")],
  ])("%s: a changed value is saved, the editor closes and the active cell clears", async (_name, target) => {
    const onSave = vi.fn(async () => null);
    const { user, onPresence } = setup({ onSave });
    await user.dblClick(cell("20"));
    const input = await screen.findByRole("textbox", { name: "n" });
    await user.type(input, "9");
    expect(lastPresence(onPresence)).toEqual({ key: '["r2"]', column: "n", editing: true });
    const before = nothingPublished(onPresence);
    await user.click(target());
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(onSave).toHaveBeenCalledWith('["r2"]', expect.objectContaining({ name: "n" }), { null: false, text: "209" });
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "n" })).toBeNull());
    expect(activeCells()).toEqual([]);
    expect(lastPresence(onPresence)).toEqual(NOTHING);
    expect(nothingPublished(onPresence)).toBe(before + 1);
  });

  it("the_save_does_not_pull_focus_back_into_the_grid", async () => {
    const onSave = vi.fn(async () => null);
    const { user } = setup({ onSave });
    await user.dblClick(cell("20"));
    await user.type(await screen.findByRole("textbox", { name: "n" }), "9");
    const toolbar = screen.getByRole("button", { name: "toolbar" });
    await user.click(toolbar);
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "n" })).toBeNull());
    expect(document.activeElement).toBe(toolbar);
    expect(activeCells()).toEqual([]);
  });

  it("an_unchanged_value_closes_the_editor_without_a_save_and_clears_the_cell", async () => {
    const onSave = vi.fn(async () => null);
    const { user, onPresence } = setup({ onSave });
    await user.dblClick(cell("20"));
    await screen.findByRole("textbox", { name: "n" });
    const before = nothingPublished(onPresence);
    await user.click(screen.getByText("below the grid"));
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "n" })).toBeNull());
    expect(onSave).not.toHaveBeenCalled();
    expect(activeCells()).toEqual([]);
    expect(lastPresence(onPresence)).toEqual(NOTHING);
    expect(nothingPublished(onPresence)).toBe(before + 1);
  });

  it("a_failed_save_keeps_the_editor_open_and_the_cell_active", async () => {
    const onSave = vi.fn(async () => "Not a number");
    const { user, onPresence } = setup({ onSave });
    const c = cell("20"); // an open editor renames the cell: keep the element
    await user.dblClick(c);
    await user.type(await screen.findByRole("textbox", { name: "n" }), "x");
    const before = nothingPublished(onPresence);
    await user.click(screen.getByText("below the grid"));
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(await screen.findByRole("alert")).toHaveTextContent("Not a number");
    expect(screen.getByRole("textbox", { name: "n" })).toBeInTheDocument();
    expect(c).toHaveAttribute("data-active", "true");
    expect(activeCells()).toEqual([c]);
    expect(nothingPublished(onPresence)).toBe(before);
    expect(lastPresence(onPresence)).toEqual({ key: '["r2"]', column: "n", editing: true });
  });

  it("the_gutter_checkbox_is_not_outside_the_active_cell_stays", async () => {
    const onSave = vi.fn(async () => null);
    const onToggleRow = vi.fn();
    const { user, onPresence } = setup({ onSave, onToggleRow });
    await user.dblClick(cell("20"));
    await user.type(await screen.findByRole("textbox", { name: "n" }), "9");
    const before = nothingPublished(onPresence);
    await user.click(screen.getByRole("checkbox", { name: "Select row 3" }));
    expect(onToggleRow).toHaveBeenCalledWith('["r3"]');
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.queryByRole("textbox", { name: "n" })).toBeNull());
    expect(activeCells()).toHaveLength(1);
    expect(nothingPublished(onPresence)).toBe(before);
  });

  it("another_body_cell_moves_the_selection_and_never_publishes_nothing", async () => {
    const { user, onPresence } = setup();
    await user.dblClick(cell("20"));
    await screen.findByRole("textbox", { name: "n" });
    const before = nothingPublished(onPresence);
    await user.click(cell("m3"));
    expect(cell("m3")).toHaveAttribute("data-active", "true");
    expect(nothingPublished(onPresence)).toBe(before);
  });
});
