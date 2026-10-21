import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { stubLayout } from "@/test/table-editor-fixtures";
import { Grid, type GridProps } from "./grid";

/**
 * RT1 second Playwright pass, ruling 1 (user, 2026-10-21): the cell editor's error is painted under the next row.
 *
 * Cause (read from the code): each body row is `absolute` with a `transform`, so each row is its own stacking context;
 * the editor's `z-30` only orders it inside its own row, and the next row (later in the DOM) paints over whatever
 * hangs out of the edited row: the error `<p role="alert">`, and just as much the tall editors (JSON, the select).
 *
 * Pin chosen (the mechanism is the row, not a portal): the row holding the open editor is raised.
 * - That row (`[role="row"]`) has `data-editing="true"` and an inline `style.zIndex` that is an integer from 1 to 19
 *   (the sticky header groups are `z-20` and `z-30`, so a raised row never covers them). No other row has `data-editing`
 *   or an inline z-index. Closing the editor (Escape, a save) removes both, so the row goes back to its place.
 * - The error stays inside the editor, inside that row (so it is above every other row); the same goes for any
 *   `[data-editor]` content.
 * - Width: the error's inline `style.maxWidth` is a `px` or `rem` length, no wider than the room left between the edited
 *   cell's left edge and the grid viewport's right edge (the `[data-grid-scroll]` element), but at least 120px; with
 *   room to spare it is 320px (20rem, as `max-w-80` is today). The box stays left-aligned on the cell and wraps.
 *   The test stubs the edited cell's `getBoundingClientRect().left`, its `offsetLeft` and the scroll element's
 *   `getBoundingClientRect().right` and `clientWidth` consistently, so measuring with either works.
 *
 * What jsdom cannot catch: that the browser really paints the raised row over its neighbours (stacking contexts,
 * `transform`, sticky positioning), the real measured width, and a grid scrolled sideways after the error showed. The
 * Playwright pass checks those.
 */

type Row = Record<string, unknown>;
const ROWS: Row[] = [
  { id: "r1", at: "2026-10-01 10:00:00+00", memo: "m1" },
  { id: "r2", at: "2026-10-02 10:00:00+00", memo: "m2" },
  { id: "r3", at: "2026-10-03 10:00:00+00", memo: "m3" },
];
const KEYS = ['["r1"]', '["r2"]', '["r3"]'];
const columns = [
  { name: "id", type: "text" },
  { name: "at", type: "timestamptz" },
  { name: "memo", type: "text" },
].map(({ name, type }) => ({
  name,
  desc: { type, nullable: true, has_default: false, generated: false, array: false, enum: null },
  labels: null,
  readOnly: false,
}));
const LONG = "Enter a date and time with its offset, like 2026-10-01 12:00:00+00. A bare date or a time alone is not enough here.";

function setup(over: Partial<GridProps> = {}) {
  const props = {
    label: "public.t", columns, rows: ROWS, firstRowNumber: 1, keys: KEYS,
    isSelected: () => false, pageSelection: "none", onToggleRow: () => {}, onTogglePage: () => {}, sort: null, onSort: vi.fn(),
    canEdit: () => true, onSave: async () => LONG, onCopy: () => {}, widths: {}, onWidths: () => {}, onPresence: vi.fn(),
    ...over,
  } as unknown as GridProps;
  return { props, user: userEvent.setup(), ...render(<Grid {...props} />) };
}
const grid = () => screen.getByRole("grid", { name: "public.t" });
const cell = (name: string) => within(grid()).getByRole("gridcell", { name });
const bodyRows = () => Array.from(grid().querySelectorAll<HTMLElement>('[role="row"][aria-rowindex]')).filter((r) => Number(r.getAttribute("aria-rowindex")) >= 2);
const rowOf = (el: Element) => el.closest<HTMLElement>('[role="row"]')!;
const z = (el: HTMLElement) => (el.style.zIndex === "" ? 0 : Number(el.style.zIndex));
/** Edits the `at` cell of row 2 with text that fails, and returns its alert. */
async function failEdit(user: ReturnType<typeof setup>["user"]) {
  await user.dblClick(cell("2026-10-02 10:00:00+00"));
  const input = await screen.findByRole("textbox", { name: "at" });
  await user.clear(input);
  await user.type(input, "not a date{Enter}");
  return screen.findByRole("alert");
}
const length = (v: string) => (v.endsWith("rem") ? parseFloat(v) * 16 : v.endsWith("px") ? parseFloat(v) : NaN);

beforeEach(() => stubLayout());
afterEach(() => vi.restoreAllMocks());

describe("ruling 1: the row of the open editor is raised", () => {
  it("the_editing_row_is_marked_and_raised_below_the_sticky_headers_and_no_other_row_is", async () => {
    const { user } = setup();
    expect(bodyRows().map((r) => r.getAttribute("data-editing"))).toEqual([null, null, null]);
    await user.dblClick(cell("2026-10-02 10:00:00+00"));
    const editor = await waitFor(() => {
      const el = grid().querySelector<HTMLElement>("[data-editor]");
      expect(el).not.toBeNull();
      return el!;
    });
    const editing = rowOf(editor);
    expect(editing).toHaveAttribute("data-editing", "true");
    expect(z(editing)).toBeGreaterThanOrEqual(1);
    expect(z(editing)).toBeLessThanOrEqual(19);
    expect(Number.isInteger(z(editing))).toBe(true);
    for (const r of bodyRows().filter((r) => r !== editing)) {
      expect(r).not.toHaveAttribute("data-editing");
      expect(r.style.zIndex).toBe("");
    }
  });

  it("the_error_is_inside_the_raised_row_so_it_is_above_the_rows_after_it", async () => {
    const { user } = setup();
    const alert = await failEdit(user);
    expect(alert).toHaveTextContent("Enter a date and time");
    const row = rowOf(alert);
    expect(row).toHaveAttribute("data-editing", "true");
    expect(z(row)).toBeGreaterThanOrEqual(1);
    expect(z(row)).toBeLessThanOrEqual(19);
    expect(z(row)).toBeGreaterThan(Math.max(0, ...bodyRows().filter((r) => r !== row).map(z)));
    // a later row exists and is not raised: the one that painted over the error
    expect(bodyRows().indexOf(row)).toBeLessThan(bodyRows().length - 1);
  });

  it("the_editing_row_goes_back_to_its_place_when_the_editor_closes", async () => {
    const { user } = setup();
    await failEdit(user);
    await user.keyboard("{Escape}");
    await waitFor(() => expect(grid().querySelector("[data-editor]")).toBeNull());
    for (const r of bodyRows()) {
      expect(r).not.toHaveAttribute("data-editing");
      expect(r.style.zIndex).toBe("");
    }
  });

  it("an_editor_on_the_last_row_and_on_the_first_row_is_raised_too", async () => {
    const { user } = setup({ onSave: async () => null });
    for (const [index, text] of [[0, "2026-10-01 10:00:00+00"], [2, "2026-10-03 10:00:00+00"]] as const) {
      await user.dblClick(cell(text));
      await screen.findByRole("textbox", { name: "at" });
      expect(bodyRows().map((r) => r.getAttribute("data-editing"))).toEqual(bodyRows().map((_, i) => (i === index ? "true" : null)));
      await user.keyboard("{Escape}");
      await waitFor(() => expect(grid().querySelector("[data-editor]")).toBeNull());
    }
  });

  it("moving_the_editor_to_another_row_moves_the_raise", async () => {
    const { user } = setup({ onSave: async () => null });
    await user.dblClick(cell("2026-10-01 10:00:00+00"));
    await screen.findByRole("textbox", { name: "at" });
    await user.dblClick(cell("m3"));
    await screen.findByRole("textbox", { name: "memo" });
    await waitFor(() => expect(bodyRows().map((r) => r.getAttribute("data-editing"))).toEqual([null, null, "true"]));
    expect(bodyRows().map((r) => r.style.zIndex !== "")).toEqual([false, false, true]);
  });
});

describe("ruling 1: the error stays within the grid's visible width", () => {
  const rect = (left: number, right: number) => ({ x: left, y: 0, top: 0, left, right, bottom: 800, width: right - left, height: 800, toJSON: () => ({}) }) as DOMRect;
  /** The grid viewport is 0..800 wide; the cell with the editor starts at `cellLeft`. */
  function geometry(cellLeft: number) {
    const hasEditor = (el: Element) => el.hasAttribute("data-editor") || el.querySelector(":scope > [data-editor]") !== null;
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
      if (this.hasAttribute("data-grid-scroll")) return rect(0, 800);
      if (hasEditor(this)) return rect(cellLeft, cellLeft + 300);
      return rect(0, 1200);
    });
    vi.spyOn(HTMLElement.prototype, "offsetLeft", "get").mockImplementation(function (this: HTMLElement) {
      return hasEditor(this) && this.getAttribute("role") === "gridcell" ? cellLeft : 0;
    });
    vi.spyOn(Element.prototype, "clientWidth", "get").mockImplementation(function (this: Element) {
      return this.hasAttribute("data-grid-scroll") ? 800 : 0;
    });
  }

  it("near_the_right_edge_the_error_is_no_wider_than_the_room_left_and_wraps", async () => {
    geometry(600);
    const { user } = setup();
    const alert = await failEdit(user);
    await waitFor(() => expect(alert.style.maxWidth).not.toBe(""));
    const w = length(alert.style.maxWidth);
    expect(w).toBeLessThanOrEqual(200);
    expect(w).toBeGreaterThanOrEqual(120);
    expect(alert).toHaveTextContent(LONG);
  });

  it("with_room_to_spare_the_error_keeps_its_320px", async () => {
    geometry(0);
    const { user } = setup();
    const alert = await failEdit(user);
    await waitFor(() => expect(alert.style.maxWidth).not.toBe(""));
    expect(length(alert.style.maxWidth)).toBe(320);
  });
});
