import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, FULL, OBJECTS, ROWS, bodies, filterByTextKey, invoices, obj, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/**
 * RT1 Playwright pass, fixes F1-F4 (2026-10-21), through the table editor.
 *
 * Pins for the coder:
 * - F1/F2: see grid-fs.test.tsx; here the real save path (conflict check included) and the presence the other member
 *   gets (`{ row, column, editing }` through `TableSubscription.presence`, debounced 50 ms).
 * - F3: the cause is `ColumnStrip` (`above` of the Grid, column-strip.tsx): it calls `useTableStructure(target)` without
 *   `enabled`, so a view's rows tab requests `GET .../tables/<schema>/<view>` (404). Decision: Studio never requests that
 *   table-only endpoint for an object that is not an editable table (view, materialized view, foreign table, partitioned
 *   table), on the Rows tab or the Structure tab. Rows and their column headers still show; no `role="alert"` appears.
 *   (The endpoint is NOT made to serve views: reported, not decided.) What the Structure tab shows for a view is the
 *   coder's choice, within "no request, no alert".
 * - F4 (decision: show at least the visible count): the footer's total and the table bar's "≈ N rows" never show a number
 *   below the rows on screen. The estimate stays when it is the larger one (the existing "of ≈ 48,210" pins hold);
 *   otherwise the visible count is shown with the same "≈". After a live insert shown with "Show": estimate 3, 4 rows on
 *   screen reads "Rows 1–4 of ≈ 4" and "≈ 4 rows". A full page of 50 rows against an estimate of 30 reads
 *   "Rows 1–50 of ≈ 50" and "≈ 50 rows". (`COUNT_QUERY` is the selection bar's filtered count, not the footer's; it is
 *   not used here.)
 */

const Q = `${B}/console/query`;
const grid = (name = "public.invoices") => screen.findByRole("grid", { name });
const cell = async (text: string, name?: string) => within(await grid(name)).findByRole("gridcell", { name: text });
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");
const activeCells = () => Array.from(document.querySelectorAll('[data-active="true"]'));
const key = (id: string) => JSON.stringify([id]);

type Data = Record<string, unknown>[];
function matches(row: Record<string, unknown>, w: any): boolean {
  if (!w) return true;
  if (w.and) return w.and.every((p: unknown) => matches(row, p));
  if ("is" in w) return (row[w.col] ?? null) === w.is;
  if ("eq" in w) return row[w.col] !== null && row[w.col] !== undefined && String(row[w.col]) === w.eq;
  return true;
}
function server(data: Data): FakeHandler {
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
function mount(o: { realtime?: boolean; objects?: unknown[]; rows?: Data } = {}) {
  const rt = o.realtime ? new FakeRealtime() : undefined;
  const data: Data = o.rows ?? ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ realtime: rt, objects: o.objects, routes: { [`POST ${Q}`]: server(data) } });
  return { rt, data, ...s };
}
const live = async (rt: FakeRealtime) => {
  await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
  rt.joined();
};

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("F1: the other member sees where you went", () => {
  it("pressing_another_cell_with_an_unchanged_editor_open_publishes_that_cell_not_editing", async () => {
    const { rt, user } = mount({ realtime: true });
    await live(rt!);
    const edited = await cell("Q3 plan");
    await user.dblClick(edited);
    await screen.findByRole("textbox", { name: "memo" });
    await vi.waitFor(() => expect(rt!.table.presence.at(-1)).toEqual({ row: key("81c4-2"), column: "memo", editing: true }));
    await user.click(await cell("dup"));
    await vi.waitFor(() => expect(rt!.table.presence.at(-1)).toEqual({ row: key("c2d1-3"), column: "memo", editing: false }));
    expect(screen.queryByRole("textbox", { name: "memo" })).toBeNull();
    expect(activeCells()).toEqual([await cell("dup")]);
  });
});

describe("F2: a changed value and a press on another cell, through the real save path", () => {
  it("saves_with_the_conflict_check_once_and_the_pressed_cell_is_active_and_focused", async () => {
    const { f, rt, user } = mount({ realtime: true });
    await live(rt!);
    const edited = await cell("Q3 plan");
    const pressed = await cell("dup");
    await user.dblClick(edited);
    const input = await screen.findByRole("textbox", { name: "memo" });
    await user.clear(input);
    await user.type(input, "Q4 plan");
    await user.click(pressed);
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    const w = (await writes(f))[0].tree;
    expect(w.set).toEqual({ memo: "Q4 plan" });
    expect(w.where).toEqual({ and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "memo", eq: "Q3 plan", text: true }] });
    await vi.waitFor(() => expect(screen.queryByRole("textbox", { name: "memo" })).toBeNull());
    expect(activeCells()).toEqual([pressed]);
    expect(document.activeElement).toBe(pressed);
    await vi.waitFor(() => expect(rt!.table.presence.at(-1)).toEqual({ row: key("c2d1-3"), column: "memo", editing: false }));
    expect(await writes(f)).toHaveLength(1);
  });

  it("a_row_changed_meanwhile_opens_the_conflict_dialog_and_the_pressed_cell_is_still_active", async () => {
    const { data, f, user } = mount();
    const edited = await cell("Q3 plan");
    const pressed = await cell("dup");
    await user.dblClick(edited);
    const input = await screen.findByRole("textbox", { name: "memo" });
    await user.clear(input);
    await user.type(input, "Q4 plan");
    data[1].memo = "changed by Ana";
    await user.click(pressed);
    expect(await screen.findByRole("dialog", { name: "Changed by someone else" })).toBeInTheDocument();
    expect(await writes(f)).toHaveLength(1);
    expect(screen.queryByRole("textbox", { name: "memo" })).toBeNull();
    expect(activeCells()).toEqual([pressed]);
  });

  it("a_refused_save_keeps_the_editor_on_the_old_cell_and_that_cell_active", async () => {
    const data: Data = ROWS.map((r) => ({ ...r }));
    const refuse: FakeHandler = async (req) => {
      const { tree } = await req.clone().json();
      if (tree.op === "update") return jsonResponse({ error: { code: "bad_request", message: "value too long" } }, 400);
      return server(data)(req);
    };
    const { user } = renderEditor({ routes: { [`POST ${Q}`]: refuse } });
    const edited = await cell("Q3 plan");
    await user.dblClick(edited);
    const input = await screen.findByRole("textbox", { name: "memo" });
    await user.type(input, "!");
    await user.click(await cell("dup"));
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "memo" })).toBeInTheDocument();
    expect(activeCells()).toEqual([edited]);
  });
});

describe("F3: no table-only request for an object that is not an editable table", () => {
  const tableUrl = (name: string) => `${B}/tables/public/${name}`;
  const tableGets = (f: ReturnType<typeof mount>["f"], name: string) => f.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname === tableUrl(name));

  it.each([
    ["view", "v_open"],
    ["materialized_view", "mv_totals"],
    ["foreign_table", "ft_remote"],
    ["partitioned_table", "pt_events"],
  ])("a %s: rows and column headers show, no GET to its table path, no alert", async (kind, name) => {
    const objects = [...OBJECTS, obj("public", name, { kind, editable: false })];
    const rows: Data = ROWS.map((r) => ({ ...r }));
    const { f } = renderEditorFor(name, objects, rows);
    expect(await cell("Q3 plan", `public.${name}`)).toBeInTheDocument();
    for (const column of ["id", "memo"]) expect(within(await grid(`public.${name}`)).getByRole("columnheader", { name: column })).toBeInTheDocument();
    // everything the page asks for on open has been asked by now: the rows and the type catalog came back
    await vi.waitFor(() => expect(f.calls.some((r) => r.method === "GET" && new URL(r.url).pathname === `${B}/types`)).toBe(true));
    expect(tableGets(f, name)).toHaveLength(0);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  // gb ruling 4 (real view types): the Rows tab asks nothing, the Structure tab asks exactly once (the endpoint now answers for views).
  it("the_structure_tab_of_a_view_makes_exactly_one_request_to_the_table_endpoint", async () => {
    const rows: Data = ROWS.map((r) => ({ ...r }));
    const { f, user } = renderEditor({
      params: { ...FULL, table: "v_open" },
      objects: OBJECTS,
      routes: { [`POST ${Q}`]: server(rows), [`GET ${tableUrl("v_open")}`]: () => jsonResponse(invoices({ name: "v_open", constraints: [] } as never)) },
    });
    expect(await cell("Q3 plan", "public.v_open")).toBeInTheDocument();
    expect(tableGets(f, "v_open")).toHaveLength(0);
    await user.click(screen.getByRole("tab", { name: "Structure" }));
    await vi.waitFor(() => expect(tableGets(f, "v_open")).toHaveLength(1));
    await screen.findByRole("tabpanel");
    expect(tableGets(f, "v_open")).toHaveLength(1);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("an_editable_table_still_reads_its_table_definition", async () => {
    const { f } = mount();
    expect(await cell("Q3 plan")).toBeInTheDocument();
    await vi.waitFor(() => expect(tableGets(f, "invoices").length).toBeGreaterThan(0));
  });
});

function renderEditorFor(table: string, objects: unknown[], rows: Data) {
  return renderEditor({ params: { ...FULL, table }, objects, routes: { [`POST ${Q}`]: server(rows) } });
}

describe("F4: the counts never read below the rows on screen", () => {
  const small = (estimate: number) => [obj("public", "invoices", { row_estimate: estimate, total_bytes: 12582912 })];
  const header = () => document.querySelector('h2[title="public.invoices"]')?.parentElement?.textContent ?? "";

  it("after_a_live_insert_is_shown_the_footer_and_the_header_say_the_visible_count", async () => {
    const { rt, data, user } = mount({ realtime: true, objects: small(3) });
    // joined, but no `joined` answer yet: it would read the page again and pick the new row up without the chip
    await vi.waitFor(() => expect(rt!.active.length).toBeGreaterThan(0));
    expect(await screen.findByText("Rows 1–3 of ≈ 3")).toBeInTheDocument();
    expect(header()).toContain("≈ 3 rows");
    data.push({ id: "z-1", customer_id: "c-9", status: "open", amount_cents: 1, memo: "new" });
    rt!.rows(1, [{ op: "insert", keys: [key("z-1")] }]);
    await user.click(await screen.findByRole("button", { name: "Show" }));
    expect(await screen.findByText("Rows 1–4 of ≈ 4")).toBeInTheDocument();
    expect(header()).toContain("≈ 4 rows");
    expect(header()).not.toContain("≈ 3 rows");
  });

  it("a_full_page_over_the_estimate_reads_the_page_size_in_both", async () => {
    const many: Data = Array.from({ length: 50 }, (_, i) => ({ ...ROWS[0], id: `a-${i}` }));
    renderEditor({ objects: small(30), rows: many });
    expect(await screen.findByText("Rows 1–50 of ≈ 50")).toBeInTheDocument();
    expect(header()).toContain("≈ 50 rows");
  });

  it("an_estimate_above_the_rows_on_screen_is_left_alone", async () => {
    mount({ objects: small(48210) });
    expect(await screen.findByText("Rows 1–3 of ≈ 48,210")).toBeInTheDocument();
    expect(header()).toContain("≈ 48,210 rows");
  });
});
