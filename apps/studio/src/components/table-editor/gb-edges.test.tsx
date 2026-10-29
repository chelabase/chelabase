import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, FULL, OBJECTS, ROWS, filterByTextKey, invoices, obj, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { ERROR_MAX_WIDTH, ERROR_MIN_WIDTH, errorRoom } from "./cell-editor";

/** The coder's edge cases for the gb fixes (editor error width, a seq gap and the row panel, a view's cached definition). */

const Q = `${B}/console/query`;
const key = (id: string) => JSON.stringify([id]);
type Data = Record<string, unknown>[];

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("errorRoom: the editor error's max width", () => {
  const viewport = (right: number) => ({ right, width: right });
  it("is_the_full_width_with_room_to_spare_and_for_an_unmeasured_viewport", () => {
    expect(errorRoom(0, viewport(800))).toBe(ERROR_MAX_WIDTH);
    expect(errorRoom(0, { right: 0, width: 0 })).toBe(ERROR_MAX_WIDTH);
  });
  it("is_the_room_left_between_the_bounds", () => {
    // gf item 4: the room keeps an 8px gap from the viewport's right edge
    expect(errorRoom(600, viewport(800))).toBe(192);
    expect(errorRoom(480, viewport(800))).toBe(312);
    expect(errorRoom(472, viewport(800))).toBe(ERROR_MAX_WIDTH);
    expect(errorRoom(600.6, viewport(800))).toBe(191);
  });
  it("never_goes_below_the_minimum_even_for_a_cell_at_or_past_the_right_edge", () => {
    expect(errorRoom(750, viewport(800))).toBe(ERROR_MIN_WIDTH);
    expect(errorRoom(800, viewport(800))).toBe(ERROR_MIN_WIDTH);
    expect(errorRoom(1200, viewport(800))).toBe(ERROR_MIN_WIDTH);
  });
});

describe("a seq gap re-reads the open row panel too", () => {
  function server(data: Data): FakeHandler {
    return async (req) => {
      const { tree } = await req.clone().json();
      return jsonResponse(tree.op === "select" ? filterByTextKey(tree.where, data) : data);
    };
  }
  it("a_missed_change_reaches_the_panel_through_the_page_refetch", async () => {
    const rt = new FakeRealtime();
    const data: Data = ROWS.map((r) => ({ ...r }));
    const s = renderEditor({ realtime: rt, routes: { [`POST ${Q}`]: server(data) } });
    await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
    await s.user.click(await screen.findByRole("button", { name: "Open row 2" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    expect(within(panel).getByLabelText("memo")).not.toHaveValue("missed");
    Object.assign(data[1], { memo: "missed" });
    // seq 5 right after the join (0): a gap, so the page (and now the panel's row) is read again.
    rt.rows(5, [{ op: "update", keys: [key("81c4-2")] }]);
    await vi.waitFor(() => expect(within(screen.getByRole("complementary", { name: "Row" })).getByLabelText("memo")).toHaveValue("missed"), { timeout: 4000 });
  });
});

describe("a view's definition is cached apart from the table queries", () => {
  it("going_back_to_rows_after_the_structure_tab_asks_nothing_more_and_shows_the_rows", async () => {
    const name = "mv_totals";
    const path = `${B}/tables/public/${name}`;
    const objects = [...OBJECTS, obj("public", name, { kind: "materialized_view", editable: false })];
    const definition = invoices({ name, constraints: [], indexes: [], policies: [], grants: [], options: [] } as never);
    const s = renderEditor({
      params: { ...FULL, table: name },
      objects,
      routes: { [`POST ${Q}`]: () => jsonResponse(ROWS.map((r) => ({ ...r }))), [`GET ${path}`]: () => jsonResponse(definition) },
    });
    const gets = () => s.f.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname === path);
    const grid = await screen.findByRole("grid", { name: `public.${name}` });
    await within(grid).findByRole("gridcell", { name: "Q3 plan" });
    await s.user.click(screen.getByRole("tab", { name: "Structure" }));
    const panel = await screen.findByRole("tabpanel");
    await vi.waitFor(() => expect(panel.querySelector("tr[data-column]")).not.toBeNull());
    await s.user.click(screen.getByRole("tab", { name: "Rows" }));
    const again = await screen.findByRole("grid", { name: `public.${name}` });
    expect(await within(again).findByRole("gridcell", { name: "Q3 plan" })).toBeInTheDocument();
    // still read-only: no column chips or Add column from a cached definition
    expect(screen.queryByRole("button", { name: "Add column" })).toBeNull();
    expect(gets()).toHaveLength(1);
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
