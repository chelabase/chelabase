import { render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeRealtime, member } from "@/test/fake-realtime";
import { jsonResponse } from "@/test/fake-fetch";
import { B, ROWS, filterByTextKey, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { PresenceLayer, maxRings, type PresenceGeometry } from "./presence-layer";

/*
 * The coder's edge tests for the nested presence boxes (RT1 ga): the `selfCell` prop (the grid's own active cell,
 * which leads the viewer's debounced presence entry), the ring cap's odd inputs, and the wiring in rows-view.
 */
const key = (id: string) => JSON.stringify([id]);
const KEYS = [key("r1"), key("r2"), key("r3")];
const geometry: PresenceGeometry = { rowHeight: 32, offsetTop: 40, columnBox: (c) => (c === "memo" ? { left: 200, width: 120 } : c === "amount" ? { left: 80, width: 120 } : null) };
const on = (row: string, id: string) => member({ id, sub: `u-${id}`, name: id, state: { row: key(row), column: "memo", editing: false } });
const me = (row = "r2", column = "memo") => member({ id: "me1", sub: "u2", name: "Me", state: { row: key(row), column, editing: false } });
const layer = (members: ReturnType<typeof member>[], selfCell?: { row: string; column: string } | null, g = geometry) => (
  <div style={{ position: "relative" }}>
    <PresenceLayer members={members} selfSub="u2" rowKeys={KEYS} geometry={g} selfCell={selfCell} />
  </div>
);
const insetOf = (id: string) => screen.getAllByTestId("presence-cell").find((b) => b.getAttribute("data-member") === id)?.getAttribute("data-inset");

beforeEach(() => {
  vi.stubGlobal("matchMedia", (query: string) => ({ matches: false, media: query, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("maxRings edges", () => {
  it("non_finite_and_negative_heights_still_give_one_ring", () => {
    expect(maxRings(Number.NaN)).toBe(1);
    expect(maxRings(Number.POSITIVE_INFINITY)).toBe(1);
    expect(maxRings(-40)).toBe(1);
    expect(maxRings(35.9)).toBe(3);
  });
});

describe("selfCell decides the viewer's ring when given", () => {
  it("the_grid_cell_wins_over_a_lagging_presence_entry_elsewhere", () => {
    render(layer([me("r1"), on("r2", "a")], { row: key("r2"), column: "memo" }));
    expect(insetOf("a")).toBe("2");
  });

  it("the_grid_cell_wins_over_a_lagging_presence_entry_on_the_cell", () => {
    render(layer([me("r2"), on("r2", "a")], { row: key("r1"), column: "memo" }));
    expect(insetOf("a")).toBe("0");
  });

  it("null_means_the_viewer_is_on_no_cell_even_with_a_stale_entry_there", () => {
    render(layer([me("r2"), on("r2", "a")], null));
    expect(insetOf("a")).toBe("0");
  });

  it("the_same_row_but_another_column_is_not_the_cell", () => {
    render(layer([on("r2", "a")], { row: key("r2"), column: "amount" }));
    expect(insetOf("a")).toBe("0");
  });

  it("it_takes_its_ring_from_the_cap_too", () => {
    render(layer([on("r2", "a"), on("r2", "b"), on("r2", "c")], { row: key("r2"), column: "memo" }));
    expect(screen.getAllByTestId("presence-cell").map((b) => b.getAttribute("data-member"))).toEqual(["a", "b"]);
  });

  it("at_one_ring_in_all_the_viewer_on_the_cell_leaves_no_box_but_the_row_tint", () => {
    render(layer([on("r2", "a")], { row: key("r2"), column: "memo" }, { ...geometry, rowHeight: 20 }));
    expect(screen.queryAllByTestId("presence-cell")).toHaveLength(0);
    expect(screen.getAllByTestId("presence-row")).toHaveLength(1);
  });

  it("ids_nest_in_plain_string_order_not_locale_order", () => {
    // "B" < "a" in code-unit order; a locale compare would put "a" first.
    render(layer([on("r2", "a"), on("r2", "B")]));
    expect(insetOf("B")).toBe("0");
    expect(insetOf("a")).toBe("2");
  });

  it("members_on_a_hidden_column_are_not_counted_on_any_cell", () => {
    const hidden = member({ id: "0", sub: "u-0", name: "zero", state: { row: key("r2"), column: "gone", editing: false } });
    render(layer([hidden, on("r2", "a")]));
    expect(insetOf("a")).toBe("0");
  });
});

describe("rows-view passes the grid's active cell", () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
    stubLayout();
  });

  it("clicking_onto_a_members_cell_moves_their_box_in_before_any_presence_echo", async () => {
    const rt = new FakeRealtime();
    const Q = `${B}/console/query`;
    const { user } = renderEditor({ realtime: rt, routes: { [`POST ${Q}`]: async (req) => {
      const { tree } = await req.clone().json();
      return jsonResponse(tree.op === "select" ? filterByTextKey(tree.where, ROWS) : ROWS);
    } } });
    await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
    const grid = await screen.findByRole("grid", { name: "public.invoices" });
    const target = await within(grid).findByRole("gridcell", { name: "45000" });
    rt.presenceState([member({ id: "ana", sub: "u-ana", name: "Ana", color_seed: 2, state: { row: key("81c4-2"), column: "amount_cents", editing: false } })]);
    expect(await screen.findByTestId("presence-cell")).toHaveAttribute("data-inset", "0");
    await user.click(target);
    // No presence entry of the viewer's own ever arrives here: the grid's active cell alone moves Ana's box in.
    await vi.waitFor(() => expect(screen.getByTestId("presence-cell")).toHaveAttribute("data-inset", "2"));
    await user.click(await within(grid).findByRole("gridcell", { name: "12900" }));
    await vi.waitFor(() => expect(screen.getByTestId("presence-cell")).toHaveAttribute("data-inset", "0"));
  });
});
