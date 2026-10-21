import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { presenceColor } from "@/lib/realtime/presence";
import { member } from "@/test/fake-realtime";
import { AvatarStack, PresenceLayer, type PresenceGeometry } from "./presence-layer";

const key = (id: string) => JSON.stringify([id]);
const KEYS = [key("r1"), key("r2"), key("r3")];
const geometry: PresenceGeometry = {
  rowHeight: 32,
  offsetTop: 40,
  columnBox: (c) => ({ memo: { left: 200, width: 120 }, amount: { left: 80, width: 120 } })[c as "memo"] ?? null,
};

function reducedMotion(reduce: boolean) {
  vi.stubGlobal(
    "matchMedia",
    (query: string) => ({ matches: reduce && query.includes("prefers-reduced-motion"), media: query, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }),
  );
}

const ana = member({ id: "ana", sub: "u-ana", name: "Ana Lee", email: "ana@x.test", role: "billing_admin", color_seed: 3, state: { row: key("r2"), column: "memo", editing: false } });

const layer = (members = [ana], over: Partial<Parameters<typeof PresenceLayer>[0]> = {}) =>
  render(<PresenceLayer members={members} selfSub="u2" rowKeys={KEYS} geometry={geometry} {...over} />);

/**
 * The layer as Grid hosts it: inside the rows' group, beside the gridcells. jsdom has no layout, so
 * every element reports the same rect (the layer's origin is (RECT.left, RECT.top)); a pointer at
 * clientX = RECT.left + x, clientY = RECT.top + y is at (x, y) in the geometry's coordinates.
 */
const RECT = { left: 100, top: 50 };
const at = (x: number, y: number) => ({ clientX: RECT.left + x, clientY: RECT.top + y });
/** The middle of the row `i` (0-based) and the horizontal middle of the memo column. */
const MEMO_R2 = at(260, 40 + 32 + 16);
const AMOUNT_R1 = at(140, 40 + 16);

function grid(members = [ana], onCellClick = vi.fn(), onGridClick = vi.fn(), onGridDoubleClick = vi.fn()) {
  const view = render(
    <div role="grid" data-testid="grid" onClick={onGridClick} onDoubleClick={onGridDoubleClick}>
      <div role="rowgroup" data-testid="rows" style={{ position: "relative" }}>
        <PresenceLayer members={members} selfSub="u2" rowKeys={KEYS} geometry={geometry} />
        <div role="gridcell" data-testid="cell" onClick={onCellClick} onDoubleClick={onCellClick} tabIndex={-1} />
      </div>
    </div>,
  );
  return { ...view, onCellClick, onGridClick, onGridDoubleClick, cell: screen.getByTestId("cell"), rows: screen.getByTestId("rows") };
}

beforeEach(() => {
  reducedMotion(false);
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(
    () => ({ ...RECT, right: RECT.left + 1000, bottom: RECT.top + 1000, x: RECT.left, y: RECT.top, width: 1000, height: 1000, toJSON: () => ({}) }) as DOMRect,
  );
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("PresenceLayer", () => {
  it("draws_another_members_cell_box_where_the_geometry_says", () => {
    layer();
    const box = screen.getByTestId("presence-cell");
    expect(box).toHaveAttribute("data-member", "ana");
    expect(box).toHaveAttribute("data-column", "memo");
    expect(box).toHaveAttribute("data-row", key("r2"));
    expect(box).toHaveStyle({ top: `${40 + 32}px`, left: "200px", width: "120px", height: "32px" });
  });

  it("tints_their_row_in_their_colour", () => {
    layer();
    const tint = screen.getByTestId("presence-row");
    expect(tint).toHaveAttribute("data-row", key("r2"));
    expect(tint).toHaveAttribute("data-color", presenceColor(3));
    expect(screen.getByTestId("presence-cell")).toHaveAttribute("data-color", presenceColor(3));
  });

  it("each_seed_gets_its_own_hue_and_the_same_seed_the_same_hue", () => {
    const b = member({ id: "bo", sub: "u-bo", name: "Bo", color_seed: 4, state: { row: key("r1"), column: "amount", editing: false } });
    const c = member({ id: "cy", sub: "u-cy", name: "Cy", color_seed: 3 + 8, state: { row: key("r3"), column: "memo", editing: false } });
    layer([ana, b, c]);
    const colours = Object.fromEntries(screen.getAllByTestId("presence-cell").map((e) => [e.getAttribute("data-member"), e.getAttribute("data-color")]));
    expect(colours.ana).toBe(presenceColor(3));
    expect(colours.bo).toBe(presenceColor(4));
    expect(colours.bo).not.toBe(colours.ana);
    expect(colours.cy).toBe(colours.ana);
  });

  it("never_draws_the_viewers_own_selection_even_from_another_tab", () => {
    const mine = member({ id: "m1", sub: "u2", name: "Bo", state: { row: key("r1"), column: "memo", editing: true } });
    const mine2 = member({ id: "m2", sub: "u2", name: "Bo", state: { row: key("r3"), column: "memo", editing: false } });
    layer([mine, mine2]);
    expect(screen.queryByTestId("presence-cell")).toBeNull();
    expect(screen.queryByTestId("presence-row")).toBeNull();
  });

  it("a_member_with_no_selection_or_a_row_off_the_page_draws_nothing", () => {
    const idle = member({ id: "i", sub: "u-i", name: "Idle" });
    const away = member({ id: "a", sub: "u-a", name: "Away", state: { row: key("not-here"), column: "memo", editing: false } });
    layer([idle, away]);
    expect(screen.queryByTestId("presence-cell")).toBeNull();
    expect(screen.queryByTestId("presence-row")).toBeNull();
  });

  it("a_column_that_is_not_shown_still_tints_the_row_but_has_no_box", () => {
    const hidden = member({ id: "h", sub: "u-h", name: "H", state: { row: key("r1"), column: "elsewhere", editing: false } });
    layer([hidden]);
    expect(screen.getByTestId("presence-row")).toBeInTheDocument();
    expect(screen.queryByTestId("presence-cell")).toBeNull();
  });

  it("editing_fills_the_cell", () => {
    layer([{ ...ana, state: { ...ana.state, editing: true } }]);
    expect(screen.getByTestId("presence-cell")).toHaveAttribute("data-editing", "true");
  });

  it("selecting_without_editing_is_not_filled", () => {
    layer();
    expect(screen.getByTestId("presence-cell")).toHaveAttribute("data-editing", "false");
  });

  it("the_members_cell_box_takes_no_pointer_events", () => {
    layer();
    expect(screen.getByTestId("presence-cell")).toHaveStyle({ pointerEvents: "none" });
    expect(screen.getByTestId("presence-cell").style.pointerEvents).toBe("none");
  });

  it("a_real_pointer_cannot_target_the_box", async () => {
    // user-event refuses to click an element whose pointer-events is none: what a browser does.
    const { onCellClick } = grid();
    await expect(userEvent.setup().click(screen.getByTestId("presence-cell"))).rejects.toThrow(/pointer-events/);
    expect(onCellClick).not.toHaveBeenCalled();
  });

  it("a_click_on_the_box_is_neither_forwarded_nor_stopped", () => {
    // Where a browser would hit the box (pointer-events none makes it hit the cell instead), nothing
    // forwards the click to the gridcell and nothing stops it bubbling: the cell's handler only
    // ever runs for events aimed at the cell itself.
    const { cell, onCellClick, onGridClick, onGridDoubleClick } = grid();
    const box = screen.getByTestId("presence-cell");
    document.elementsFromPoint = () => [box, cell];
    try {
      fireEvent.click(box, MEMO_R2);
      fireEvent.doubleClick(box, MEMO_R2);
    } finally {
      // @ts-expect-error jsdom has no elementsFromPoint to restore
      delete document.elementsFromPoint;
    }
    expect(onCellClick).not.toHaveBeenCalled();
    // Each event bubbled once, untouched: the box stops nothing and synthesizes nothing.
    expect(onGridClick).toHaveBeenCalledTimes(1);
    expect(onGridDoubleClick).toHaveBeenCalledTimes(1);
  });

  it("a_click_aimed_at_the_cell_reaches_its_own_handler_once", async () => {
    const { cell, onCellClick } = grid();
    await userEvent.setup().pointer([{ target: cell, coords: MEMO_R2 }, { keys: "[MouseLeft]", target: cell, coords: MEMO_R2 }]);
    expect(onCellClick).toHaveBeenCalledTimes(1);
  });

  it("hovering_the_cell_where_a_member_sits_shows_name_email_and_role", async () => {
    const { cell } = grid();
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R2 });
    const tag = await screen.findByRole("tooltip");
    expect(tag).toHaveTextContent("Ana Lee");
    expect(tag).toHaveTextContent("ana@x.test");
    expect(tag).toHaveTextContent("billing_admin");
  });

  it("the_owner_is_labelled_as_such_in_the_tag", async () => {
    const { cell } = grid([{ ...ana, role: null }]);
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R2 });
    expect(await screen.findByRole("tooltip")).toHaveTextContent(/owner/i);
  });

  it("moving_to_another_cell_closes_the_tag", async () => {
    const { cell } = grid();
    const user = userEvent.setup();
    await user.pointer({ target: cell, coords: MEMO_R2 });
    await screen.findByRole("tooltip");
    await user.pointer({ target: cell, coords: AMOUNT_R1 });
    await waitFor(() => expect(screen.queryByRole("tooltip")).toBeNull());
  });

  it("leaving_the_grid_closes_the_tag", async () => {
    const { cell } = grid();
    const user = userEvent.setup();
    await user.pointer({ target: cell, coords: MEMO_R2 });
    await screen.findByRole("tooltip");
    await user.pointer({ target: document.body, coords: at(-50, -50) });
    await waitFor(() => expect(screen.queryByRole("tooltip")).toBeNull());
  });

  it("a_cell_nobody_is_on_opens_no_tag", async () => {
    const { cell } = grid();
    await userEvent.setup().pointer({ target: cell, coords: AMOUNT_R1 });
    expect(screen.queryByRole("tooltip")).toBeNull();
  });

  it("each_member_gets_their_own_tag_by_cell", async () => {
    const bo = member({ id: "bo", sub: "u-bo", name: "Bo Kim", email: "bo@x.test", color_seed: 4, state: { row: key("r1"), column: "amount", editing: false } });
    const { cell } = grid([ana, bo]);
    await userEvent.setup().pointer({ target: cell, coords: AMOUNT_R1 });
    const tag = await screen.findByRole("tooltip");
    expect(tag).toHaveTextContent("Bo Kim");
    expect(tag).not.toHaveTextContent("Ana Lee");
  });

  it("reduced_motion_is_reflected_on_the_layer", () => {
    reducedMotion(true);
    layer();
    expect(screen.getByTestId("presence-layer")).toHaveAttribute("data-reduced-motion", "true");
  });

  it("motion_is_on_by_default", () => {
    layer();
    expect(screen.getByTestId("presence-layer")).toHaveAttribute("data-reduced-motion", "false");
  });

  it("the_layer_does_not_swallow_clicks_meant_for_the_grid", () => {
    layer();
    expect(screen.getByTestId("presence-layer")).toHaveStyle({ pointerEvents: "none" });
  });
});

describe("AvatarStack", () => {
  const people = (n: number) => Array.from({ length: n }, (_, i) => member({ id: `m${i}`, sub: `s${i}`, name: `Person ${String.fromCharCode(65 + i)}`, email: `p${i}@x.test`, color_seed: i }));

  it("shows_initials_in_each_members_colour", () => {
    render(<AvatarStack members={people(2)} />);
    const avatars = screen.getAllByTestId("avatar");
    expect(avatars).toHaveLength(2);
    expect(avatars[0]).toHaveTextContent("PA");
    expect(avatars[0]).toHaveAttribute("data-color", presenceColor(0));
    expect(avatars[1]).toHaveAttribute("data-color", presenceColor(1));
  });

  it("five_members_show_five_and_no_overflow", () => {
    render(<AvatarStack members={people(5)} />);
    expect(screen.getAllByTestId("avatar")).toHaveLength(5);
    expect(screen.queryByText(/^\+\d/)).toBeNull();
  });

  it("more_than_five_show_five_then_plus_n", () => {
    render(<AvatarStack members={people(8)} />);
    expect(screen.getAllByTestId("avatar")).toHaveLength(5);
    expect(screen.getByText("+3")).toBeInTheDocument();
  });

  it("hovering_an_avatar_shows_the_same_tag", async () => {
    render(<AvatarStack members={[{ ...people(1)[0], role: "reporter" }]} />);
    await userEvent.setup().hover(screen.getByTestId("avatar"));
    const tag = await screen.findByRole("tooltip");
    expect(within(tag).getByText(/Person A/)).toBeInTheDocument();
    expect(tag).toHaveTextContent("p0@x.test");
    expect(tag).toHaveTextContent("reporter");
  });

  it("an_empty_table_shows_no_stack", () => {
    render(<AvatarStack members={[]} />);
    expect(screen.queryByTestId("avatar")).toBeNull();
  });
});
