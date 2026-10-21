import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { presenceColor } from "@/lib/realtime/presence";
import { member } from "@/test/fake-realtime";
import { PresenceLayer, maxRings, type PresenceGeometry } from "./presence-layer";

/*
 * RT1 second Playwright pass (2026-10-21), rulings 1 and 4.
 *
 * Pins for the coder (presence-layer.tsx):
 * - Stacked boxes. The members (not the viewer) on one cell are ordered by member `id`, ascending plain string order,
 *   whatever the order of `members`. The i-th (0-based) gets `data-ring={i}` and `data-inset={base + 2 * i}`, where
 *   base is 2 when the viewer (a member with `sub === selfSub`, however many sockets) is on that very cell and 0
 *   otherwise: the viewer's own active outline (grid.tsx, 2px, drawn on the gridcell) is the outermost ring, so
 *   nothing may paint over its outer 2px. The box element is the cell's box shrunk by its inset on every side
 *   (top/left grow by it, width/height shrink by twice it), with `box-shadow: inset 0 0 0 2px <colour>`.
 *   A lone member on a cell has data-ring "0" and data-inset "0" (the old box). The viewer's own box is never drawn.
 * - The editing fill (18% tint) stays on the editing member's own box: `data-editing="true"` and a background
 *   containing "18%".
 * - Ring cap. `maxRings(rowHeight) = max(1, floor((rowHeight - 20) / 4))`: 20px of the cell stays free of rings, and each
 *   ring takes 2px off every side, so one ring per 4px of height beyond that. The viewer's ring counts: with the
 *   viewer on the cell, `maxRings - 1` others are drawn. Members past the cap get no `presence-cell` box (their row
 *   tint stays), and the hover tag still lists everyone on the cell and still opens when the first of them has no box.
 * - Hover tag side. The tag over a cell of the first row shown (`rowKeys[0]`) opens below the cell; over any other row
 *   above it, as today. Pinned on `data-side` of the element holding the tooltip (the Positioner sets it from its `side`):
 *   `screen.getByRole("tooltip").closest("[data-side]")` is "bottom" or "top".
 */
const key = (id: string) => JSON.stringify([id]);
const KEYS = [key("r1"), key("r2"), key("r3")];
const geometry: PresenceGeometry = {
  rowHeight: 32,
  offsetTop: 40,
  columnBox: (c) => ({ memo: { left: 200, width: 120 }, amount: { left: 80, width: 120 } })[c as "memo"] ?? null,
};
const RECT = { left: 100, top: 50 };
const at = (x: number, y: number) => ({ clientX: RECT.left + x, clientY: RECT.top + y });
const MEMO_R1 = at(260, 40 + 16);
const MEMO_R2 = at(260, 40 + 32 + 16);
const MEMO_R3 = at(260, 40 + 64 + 16);

const on = (row: string, id: string, over: Partial<Parameters<typeof member>[0]> = {}) =>
  member({ id, sub: `u-${id}`, name: `Person ${id.toUpperCase()}`, color_seed: id.charCodeAt(0) % 8, state: { row: key(row), column: "memo", editing: false }, ...over });
const me = (id = "me1", row = "r2", column = "memo") => member({ id, sub: "u2", name: "Me", state: { row: key(row), column, editing: false } });

const ui = (members: ReturnType<typeof member>[], rowKeys = KEYS) => (
  <div role="grid">
    <div role="rowgroup" style={{ position: "relative" }}>
      <PresenceLayer members={members} selfSub="u2" rowKeys={rowKeys} geometry={geometry} />
      <div role="gridcell" data-testid="cell" tabIndex={-1} />
    </div>
  </div>
);
function grid(members: ReturnType<typeof member>[], rowKeys = KEYS) {
  const view = render(ui(members, rowKeys));
  return { ...view, cell: screen.getByTestId("cell"), again: (m: typeof members, r = rowKeys) => view.rerender(ui(m, r)) };
}
const boxes = () => screen.queryAllByTestId("presence-cell");
const boxOf = (id: string) => boxes().find((b) => b.getAttribute("data-member") === id)!;

beforeEach(() => {
  vi.stubGlobal("matchMedia", (query: string) => ({ matches: false, media: query, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(
    () => ({ ...RECT, right: RECT.left + 1000, bottom: RECT.top + 1000, x: RECT.left, y: RECT.top, width: 1000, height: 1000, toJSON: () => ({}) }) as DOMRect,
  );
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("maxRings", () => {
  it("keeps_20px_free_and_gives_a_ring_per_4px_with_a_floor_of_one", () => {
    expect(maxRings(32)).toBe(3);
    expect(maxRings(36)).toBe(4);
    expect(maxRings(60)).toBe(10);
    expect(maxRings(24)).toBe(1);
    expect(maxRings(20)).toBe(1);
    expect(maxRings(0)).toBe(1);
  });
});

describe("stacked boxes", () => {
  it("a_lone_member_is_ring_0_with_inset_0_and_the_old_box", () => {
    grid([on("r2", "a")]);
    const box = boxOf("a");
    expect(box).toHaveAttribute("data-ring", "0");
    expect(box).toHaveAttribute("data-inset", "0");
    expect(box).toHaveStyle({ top: "72px", left: "200px", width: "120px", height: "32px" });
  });

  it("members_on_one_cell_nest_by_id_each_2px_further_in", () => {
    grid([on("r2", "c"), on("r2", "a"), on("r2", "b")]);
    expect(boxes()).toHaveLength(3);
    const geo = (id: string, ring: number) => {
      const inset = 2 * ring;
      expect(boxOf(id), id).toHaveAttribute("data-ring", String(ring));
      expect(boxOf(id), id).toHaveAttribute("data-inset", String(inset));
      expect(boxOf(id), id).toHaveStyle({ top: `${72 + inset}px`, left: `${200 + inset}px`, width: `${120 - 2 * inset}px`, height: `${32 - 2 * inset}px` });
    };
    geo("a", 0);
    geo("b", 1);
    geo("c", 2);
  });

  it("every_colour_shows_one_2px_ring_each", () => {
    grid([on("r2", "a", { color_seed: 0 }), on("r2", "b", { color_seed: 1 }), on("r2", "c", { color_seed: 6 })]);
    for (const [id, seed] of [["a", 0], ["b", 1], ["c", 6]] as const) {
      expect(boxOf(id)).toHaveAttribute("data-color", presenceColor(seed));
      expect(boxOf(id).style.boxShadow).toContain("inset 0 0 0 2px");
    }
  });

  it("the_order_is_the_ids_not_the_join_order", () => {
    const a = on("r2", "a");
    const b = on("r2", "b");
    const { again } = grid([b, a]);
    expect(boxOf("a")).toHaveAttribute("data-ring", "0");
    expect(boxOf("b")).toHaveAttribute("data-ring", "1");
    again([a, b]);
    expect(boxOf("a")).toHaveAttribute("data-ring", "0");
    again([a, { ...b, color_seed: 5 }]);
    expect(boxOf("b")).toHaveAttribute("data-ring", "1");
  });

  it("a_member_leaving_closes_the_gap_and_a_new_low_id_takes_ring_0", () => {
    const { again } = grid([on("r2", "b"), on("r2", "c")]);
    expect(boxOf("b")).toHaveAttribute("data-ring", "0");
    expect(boxOf("c")).toHaveAttribute("data-ring", "1");
    again([on("r2", "a"), on("r2", "b"), on("r2", "c")]);
    expect(boxOf("a")).toHaveAttribute("data-ring", "0");
    expect(boxOf("c")).toHaveAttribute("data-ring", "2");
  });

  it("members_on_other_cells_do_not_nest_with_each_other", () => {
    const amount = on("r1", "a", { state: { row: key("r1"), column: "amount", editing: false } });
    grid([amount, on("r2", "b")]);
    expect(boxOf("a")).toHaveAttribute("data-inset", "0");
    expect(boxOf("b")).toHaveAttribute("data-inset", "0");
  });

  it("the_editing_fill_stays_on_the_editing_members_own_box", () => {
    grid([on("r2", "a"), on("r2", "b", { state: { row: key("r2"), column: "memo", editing: true } })]);
    expect(boxOf("b")).toHaveAttribute("data-ring", "1");
    expect(boxOf("b")).toHaveAttribute("data-editing", "true");
    expect(boxOf("b").style.background).toContain("18%");
    expect(boxOf("a")).toHaveAttribute("data-editing", "false");
    expect(boxOf("a").style.background).not.toContain("18%");
  });
});

describe("the viewer's own ring is the outermost", () => {
  it("others_start_2px_in_when_the_viewer_is_on_the_cell", () => {
    grid([me(), on("r2", "a"), on("r2", "b")]);
    expect(boxes()).toHaveLength(2);
    expect(boxOf("a")).toHaveAttribute("data-inset", "2");
    expect(boxOf("a")).toHaveStyle({ top: "74px", left: "202px", width: "116px", height: "28px" });
    expect(boxOf("b")).toHaveAttribute("data-inset", "4");
    expect(boxOf("a")).toHaveAttribute("data-ring", "0");
  });

  it("the_viewer_on_another_cell_or_row_changes_nothing", () => {
    const { again } = grid([me("me1", "r3"), on("r2", "a")]);
    expect(boxOf("a")).toHaveAttribute("data-inset", "0");
    again([me("me1", "r2", "amount"), on("r2", "a")]);
    expect(boxOf("a")).toHaveAttribute("data-inset", "0");
  });

  it("two_tabs_of_the_viewer_on_the_cell_reserve_one_ring_not_two", () => {
    grid([me("me1"), me("me2"), on("r2", "a")]);
    expect(boxOf("a")).toHaveAttribute("data-inset", "2");
  });

  it("the_viewers_own_box_is_still_never_drawn", () => {
    grid([me()]);
    expect(boxes()).toHaveLength(0);
  });

  it("the_viewer_leaving_the_cell_moves_the_others_back_out", () => {
    const { again } = grid([me(), on("r2", "a")]);
    expect(boxOf("a")).toHaveAttribute("data-inset", "2");
    again([me("me1", "r1"), on("r2", "a")]);
    expect(boxOf("a")).toHaveAttribute("data-inset", "0");
  });
});

describe("ring cap", () => {
  const crowd = (n: number) => Array.from({ length: n }, (_, i) => on("r2", String.fromCharCode(97 + i)));

  it("at_a_32px_row_three_rings_are_drawn_and_the_rest_are_not", () => {
    grid(crowd(5));
    expect(boxes().map((b) => b.getAttribute("data-member"))).toEqual(["a", "b", "c"]);
    expect(boxes().map((b) => b.getAttribute("data-ring"))).toEqual(["0", "1", "2"]);
  });

  it("the_viewers_ring_counts_towards_the_cap", () => {
    grid([me(), ...crowd(5)]);
    expect(boxes().map((b) => b.getAttribute("data-member"))).toEqual(["a", "b"]);
    expect(boxOf("b")).toHaveAttribute("data-inset", "4");
  });

  it("the_cap_holds_at_exactly_the_limit_and_one_below", () => {
    const { again } = grid(crowd(3));
    expect(boxes()).toHaveLength(3);
    again(crowd(2));
    expect(boxes()).toHaveLength(2);
  });

  it("members_past_the_cap_still_tint_their_row", () => {
    grid(crowd(5));
    expect(screen.getAllByTestId("presence-row")).toHaveLength(5);
  });

  it("the_tag_lists_everyone_on_the_cell_even_those_without_a_ring", async () => {
    const { cell } = grid(crowd(5).reverse());
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R2 });
    const tag = await screen.findByRole("tooltip");
    for (const n of ["A", "B", "C", "D", "E"]) expect(tag).toHaveTextContent(`Person ${n}`);
  });

  it("the_tag_opens_when_the_only_member_in_hover_order_has_no_ring", async () => {
    // The viewer plus a crowd of two at a row height that fits one ring in total: the viewer's.
    const small: PresenceGeometry = { ...geometry, rowHeight: 24 };
    render(
      <div role="rowgroup" style={{ position: "relative" }}>
        <PresenceLayer members={[me("me1", "r1"), on("r1", "a")]} selfSub="u2" rowKeys={[key("r1")]} geometry={small} />
        <div role="gridcell" data-testid="cell" tabIndex={-1} />
      </div>,
    );
    expect(boxes()).toHaveLength(0);
    await userEvent.setup().pointer({ target: screen.getByTestId("cell"), coords: at(260, 40 + 12) });
    expect(await screen.findByRole("tooltip")).toHaveTextContent("Person A");
  });
});

describe("hover tag side", () => {
  const side = async () => (await screen.findByRole("tooltip")).closest("[data-side]")?.getAttribute("data-side");

  it("over_a_first_row_cell_the_tag_opens_below", async () => {
    const { cell } = grid([on("r1", "a")]);
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R1 });
    expect(await side()).toBe("bottom");
  });

  it("over_the_second_and_third_rows_it_opens_above_as_before", async () => {
    const { cell } = grid([on("r2", "a"), on("r3", "b")]);
    const user = userEvent.setup();
    await user.pointer({ target: cell, coords: MEMO_R2 });
    expect(await side()).toBe("top");
    await user.pointer({ target: cell, coords: MEMO_R3 });
    await waitFor(async () => expect(await side()).toBe("top"));
  });

  it("the_first_row_is_the_first_one_shown_not_the_first_of_the_table", async () => {
    // Scrolled: r2 is now rowKeys[0]. The same member's tag flips below.
    const { cell, again } = grid([on("r2", "a")]);
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R2 });
    expect(await side()).toBe("top");
    again([on("r2", "a")], [key("r2"), key("r3")]);
    await waitFor(async () => expect(await side()).toBe("bottom"));
  });

  it("a_first_row_tag_still_lists_everyone_there", async () => {
    const { cell } = grid([on("r1", "a"), on("r1", "b")]);
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R1 });
    const tag = await screen.findByRole("tooltip");
    expect(tag).toHaveTextContent("Person A");
    expect(tag).toHaveTextContent("Person B");
  });
});
