import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { stubLayout } from "@/test/table-editor-fixtures";
import { ERROR_MAX_WIDTH, ERROR_MIN_WIDTH, errorRoom } from "./cell-editor";
import { Grid, type GridProps } from "./grid";

/**
 * RT1 gf, item 4 (user, 2026-10-21): in the last visible column the editor's error box touched the window's right
 * edge. Rule: the box keeps at least an 8px gap from the grid viewport's right edge.
 * - `errorRoom(left, viewport)` is `min(320, max(120, floor(viewport.right - left - 8)))`: the room subtracts 8px, the
 *   120 floor and the 320 ceiling are unchanged, an unmeasured viewport still gives 320.
 * - In the grid the error's inline `style.maxWidth` is that value.
 *
 * Changed existing tests (gb-edges.test.tsx, `errorRoom` block): 200 -> 192, 320 -> 312 (cell at 480 of 800), 199 -> 191.
 * grid-gb-editor-stacking.test.tsx still passes unchanged (its bounds are <= 200 and >= 120, and 320 with room to spare).
 */
const viewport = (right: number) => ({ right, width: right });

describe("errorRoom: an 8px gap from the viewport's right edge", () => {
  it("subtracts_8px_from_the_room_left", () => {
    expect(errorRoom(600, viewport(800))).toBe(192);
    expect(errorRoom(480, viewport(800))).toBe(312);
    expect(errorRoom(600.6, viewport(800))).toBe(191);
  });

  it("the_full_width_needs_328px_of_room_not_320", () => {
    expect(errorRoom(472, viewport(800))).toBe(ERROR_MAX_WIDTH);
    expect(errorRoom(473, viewport(800))).toBe(319);
    expect(errorRoom(0, viewport(800))).toBe(ERROR_MAX_WIDTH);
  });

  it("the_120_floor_still_holds_one_pixel_either_side", () => {
    expect(errorRoom(672, viewport(800))).toBe(ERROR_MIN_WIDTH);
    expect(errorRoom(671, viewport(800))).toBe(121);
    expect(errorRoom(673, viewport(800))).toBe(ERROR_MIN_WIDTH);
    expect(errorRoom(790, viewport(800))).toBe(ERROR_MIN_WIDTH);
    expect(errorRoom(1200, viewport(800))).toBe(ERROR_MIN_WIDTH);
  });

  it("an_unmeasured_viewport_still_gives_the_full_width", () => {
    expect(errorRoom(0, { right: 0, width: 0 })).toBe(ERROR_MAX_WIDTH);
    expect(errorRoom(500, { right: 0, width: 0 })).toBe(ERROR_MAX_WIDTH);
  });

  it("the_box_never_reaches_the_edge_whenever_the_floor_does_not_force_it", () => {
    // exhaustive over the cell's left edge in an 800px viewport: left + width <= 792 unless the 120 floor applies
    for (let left = 0; left <= 900; left++) {
      const w = errorRoom(left, viewport(800));
      expect(w).toBeGreaterThanOrEqual(ERROR_MIN_WIDTH);
      expect(w).toBeLessThanOrEqual(ERROR_MAX_WIDTH);
      if (w > ERROR_MIN_WIDTH) expect(left + w, `left ${left}`).toBeLessThanOrEqual(792);
    }
  });
});

const columns = [
  { name: "id", type: "text" },
  { name: "at", type: "timestamptz" },
].map(({ name, type }) => ({
  name,
  desc: { type, nullable: true, has_default: false, generated: false, array: false, enum: null },
  labels: null,
  readOnly: false,
}));
const LONG = "Enter a date and time with its offset, like 2026-10-01 12:00:00+00. A bare date or a time alone is not enough here.";
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
async function failEdit() {
  const props = {
    label: "public.t", columns, rows: [{ id: "r1", at: "2026-10-01 10:00:00+00" }], firstRowNumber: 1, keys: ['["r1"]'],
    isSelected: () => false, pageSelection: "none", onToggleRow: () => {}, onTogglePage: () => {}, sort: null, onSort: vi.fn(),
    canEdit: () => true, onSave: async () => LONG, onCopy: () => {}, widths: {}, onWidths: () => {}, onPresence: vi.fn(),
  } as unknown as GridProps;
  const user = userEvent.setup();
  render(<Grid {...props} />);
  await user.dblClick(within(screen.getByRole("grid", { name: "public.t" })).getByRole("gridcell", { name: "2026-10-01 10:00:00+00" }));
  const input = await screen.findByRole("textbox", { name: "at" });
  await user.clear(input);
  await user.type(input, "not a date{Enter}");
  const alert = await screen.findByRole("alert");
  await waitFor(() => expect(alert.style.maxWidth).not.toBe(""));
  return alert;
}

beforeEach(() => stubLayout());
afterEach(() => vi.restoreAllMocks());

describe("the grid's error box keeps its gap", () => {
  it("a_cell_near_the_right_edge_gets_the_room_minus_8px", async () => {
    geometry(600);
    expect((await failEdit()).style.maxWidth).toBe("192px");
  });

  it("a_cell_with_room_to_spare_keeps_320px", async () => {
    geometry(0);
    expect((await failEdit()).style.maxWidth).toBe("320px");
  });

  it("a_cell_in_the_last_column_keeps_the_120px_floor", async () => {
    geometry(750);
    expect((await failEdit()).style.maxWidth).toBe("120px");
  });
});
