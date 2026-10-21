import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, FULL, ROWS, bodies, editorRoutes, filterByTextKey, obj, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { renderWithQuery } from "@/test/render";
import { TableEditor } from "@/components/table-editor/table-editor";
import { invoices } from "@/lib/table-editor/test-tables";

/**
 * RT1 gf (user rulings, 2026-10-21, third Playwright check).
 *
 * Item 1: the footer range ignores struck-through (deleted live) rows, like the total. After 3 live inserts, Show and
 *   3 live deletes the footer read "Rows 1-33 of 30"; it must read "Rows 1-30 of 30" (here the same shape at a small size).
 *   `to` is `offset + rows on screen that are not struck`.
 * Item 3: in Studio a `numeric` value shows exactly as Postgres's text: scale kept (`2.50`, `5.00`), no float rounding
 *   (`12345678901234567890.12`), NaN and the infinities as Postgres prints them. Found by reading: the console route and
 *   the data API both put Postgres's own literal on the wire (`"price":2.50`; NaN and the infinities as quoted strings;
 *   engine_live.rs numeric_wire_keeps_postgres_text pins that), so nothing server-side changes. The scale is lost in
 *   the browser: `parseExact` (packages/management-client/src/fetch.ts) keeps a short number as a double, and 2.50 reads
 *   back as 2.5. The fix is Studio's (the client's parse and/or reading the cell from the page's `$o` text); the public
 *   data API's JSON is not touched. The fake console below answers like the real one: the raw numeric literal in the row
 *   JSON and the `text_of` aliases as Postgres prints them.
 */

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

/* ---------- item 1: the footer range ---------- */

function server(data: Data): FakeHandler {
  return async (req) => {
    const { tree } = await req.clone().json();
    return jsonResponse(tree.op === "select" ? filterByTextKey(tree.where, data) : data);
  };
}
const est = (row_estimate: number | null) => [obj("public", "invoices", { row_estimate, total_bytes: 12582912 })];
const struckCount = () => document.querySelectorAll('[role="row"][data-deleted="true"]').length;
const newRow = (id: string) => ({ id, customer_id: "c-9", status: "open", amount_cents: 1, memo: null });

async function mountAndInsert(row_estimate: number | null, inserted: string[]) {
  const rt = new FakeRealtime();
  const data: Data = ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ realtime: rt, objects: est(row_estimate), routes: { [`POST ${Q}`]: server(data) } });
  await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
  await screen.findByText(/^Rows 1–3 of /);
  data.push(...inserted.map(newRow));
  rt.rows(1, [{ op: "insert", keys: inserted.map(key) }]);
  await s.user.click(await screen.findByRole("button", { name: "Show" }));
  await screen.findByText(new RegExp(`^Rows 1–${3 + inserted.length} of `));
  return { rt, data, ...s };
}
const strike = async (rt: FakeRealtime, ids: string[], seq = 2) => {
  rt.rows(seq, ids.map((id) => ({ op: "delete" as const, keys: [key(id)] })));
  await vi.waitFor(() => expect(struckCount()).toBe(ids.length));
};

describe("item 1: the footer range ignores struck rows", () => {
  it("insert_three_show_delete_those_three_reads_rows_1_to_3_of_3", async () => {
    const ids = ["z-1", "z-2", "z-3"];
    const { rt } = await mountAndInsert(null, ids);
    expect(screen.getByText("Rows 1–6 of 6")).toBeInTheDocument();
    await strike(rt, ids);
    expect(screen.queryByText(/^Rows 1–6/)).toBeNull();
    expect(screen.getByText("Rows 1–3 of 3")).toBeInTheDocument();
  });

  it("with_an_estimate_above_the_live_rows_the_range_still_ends_at_the_live_rows", async () => {
    const ids = ["z-1", "z-2", "z-3"];
    const { rt } = await mountAndInsert(30, ids);
    expect(screen.getByText("Rows 1–6 of ≈ 30")).toBeInTheDocument();
    await strike(rt, ids);
    expect(screen.getByText("Rows 1–3 of ≈ 30")).toBeInTheDocument();
  });

  it("one_delete_lowers_the_range_by_one_and_each_further_delete_again", async () => {
    const { rt } = await mountAndInsert(null, ["z-1", "z-2"]);
    expect(screen.getByText("Rows 1–5 of 5")).toBeInTheDocument();
    await strike(rt, ["z-1"]);
    expect(screen.getByText("Rows 1–4 of 4")).toBeInTheDocument();
    rt.rows(3, [{ op: "delete", keys: [key("z-2")] }]);
    await vi.waitFor(() => expect(struckCount()).toBe(2));
    expect(screen.getByText("Rows 1–3 of 3")).toBeInTheDocument();
  });

  it("striking_an_original_row_lowers_it_too", async () => {
    const rt = new FakeRealtime();
    const data: Data = ROWS.map((r) => ({ ...r }));
    renderEditor({ realtime: rt, objects: est(null), routes: { [`POST ${Q}`]: server(data) } });
    await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
    await screen.findByText("Rows 1–3 of 3");
    await strike(rt, ["c2d1-3"], 1);
    expect(screen.getByText("Rows 1–2 of 2")).toBeInTheDocument();
  });
});

/* ---------- item 3: numeric values in Studio ---------- */

const base = invoices().columns;
const num = (attnum: number, name: string, data_type: string) => ({
  attnum, name, data_type, type_oid: 1700, not_null: false, default: null, comment: null, collation: null, generated: null, identity: null, grants: [],
});
const TABLE = invoices({ columns: [...base, num(6, "price", "numeric(12,2)"), num(7, "wide", "numeric")] } as Partial<Table>);

/** A numeric the server writes as Postgres's own literal in the row JSON. */
interface Lit { lit: string }
const lit = (s: string): Lit => ({ lit: s });
type NRow = Record<string, string | number | null | Lit>;
const text = (v: string | number | null | Lit): string | null => (v === null ? null : typeof v === "object" ? v.lit : String(v));

const NROWS: NRow[] = [
  { id: "r1", customer_id: "c-1", status: "paid", amount_cents: 12900, memo: null, price: lit("2.50"), wide: lit("12345678901234567890.12") },
  { id: "r2", customer_id: "c-2", status: "open", amount_cents: 45000, memo: "Q3 plan", price: lit("5.00"), wide: lit("0.10") },
  // NaN and the infinities are quoted strings in Postgres's JSON
  { id: "r3", customer_id: "c-1", status: "void", amount_cents: 8900, memo: "dup", price: "NaN", wide: "-Infinity" },
  { id: "r4", customer_id: "c-1", status: "void", amount_cents: 7, memo: "inf", price: "Infinity", wide: null },
];

/** The console as the real one answers: row JSON with raw numeric literals, plus the `text_of` aliases as Postgres prints them. */
function numericConsole(data: NRow[]): FakeHandler {
  const wire = (rows: NRow[], tree: { select?: unknown }) => {
    const items = Array.isArray(tree.select) ? (tree.select as unknown[]).filter((i): i is { text_of: string; as: string } => typeof i === "object" && i !== null && "text_of" in i) : [];
    const out = rows.map((r) => {
      const row: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(r)) row[k] = typeof v === "object" && v !== null ? `@@NUM(${v.lit})@@` : v;
      for (const i of items) row[i.as] = text(r[i.text_of] ?? null);
      return row;
    });
    const body = JSON.stringify(out).replace(/"@@NUM\(([^)]*)\)@@"/g, "$1");
    return new Response(body, { status: 200, headers: { "Content-Type": "application/json" } });
  };
  return async (req) => {
    const { tree } = await req.clone().json();
    if (tree.op === "select") return wire(filterByTextKey(tree.where, data as Data) as NRow[], tree);
    if (tree.op === "update") {
      const id = /"col":"id","eq":"([^"]+)"/.exec(JSON.stringify(tree.where))?.[1];
      const hit = data.filter((r) => r.id === id);
      for (const r of hit) for (const [k, v] of Object.entries(tree.set as Record<string, string | number | null>)) r[k] = typeof v === "string" && /^-?\d+(\.\d+)?$/.test(v) && (k === "price" || k === "wide") ? lit(v) : v;
      return wire(hit, tree);
    }
    return wire(data, tree);
  };
}
function mountNumeric(data: NRow[] = NROWS.map((r) => ({ ...r }))) {
  const routes = editorRoutes({ table: TABLE });
  routes[`POST ${Q}`] = numericConsole(data);
  const f = fakeFetch(routes);
  const view = renderWithQuery(<TableEditor params={FULL} navigate={vi.fn()} />, { fetch: f });
  return { f, data, user: userEvent.setup(), ...view };
}
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (name: string) => within(await grid()).findByRole("gridcell", { name });

describe("item 3: numeric values show as Postgres prints them", () => {
  it("the_scale_of_a_numeric_12_2_is_kept_in_the_grid", async () => {
    mountNumeric();
    expect(await cell("2.50")).toBeInTheDocument();
    expect(await cell("5.00")).toBeInTheDocument();
    expect(within(await grid()).queryByRole("gridcell", { name: "2.5" })).toBeNull();
    expect(within(await grid()).queryByRole("gridcell", { name: "5" })).toBeNull();
  });

  it("a_scale_only_zero_is_kept_in_an_unconstrained_numeric", async () => {
    mountNumeric();
    expect(await cell("0.10")).toBeInTheDocument();
    expect(within(await grid()).queryByRole("gridcell", { name: "0.1" })).toBeNull();
  });

  it("a_twenty_digit_numeric_is_not_rounded_through_a_float", async () => {
    mountNumeric();
    expect(await cell("12345678901234567890.12")).toBeInTheDocument();
  });

  it("nan_and_the_infinities_show_as_postgres_prints_them", async () => {
    mountNumeric();
    expect(await cell("NaN")).toBeInTheDocument();
    expect(await cell("-Infinity")).toBeInTheDocument();
    expect(await cell("Infinity")).toBeInTheDocument();
  });

  it("editing_a_numeric_cell_starts_from_its_exact_text", async () => {
    const { user } = mountNumeric();
    await user.dblClick(await cell("2.50"));
    expect(await screen.findByRole("textbox", { name: "price" })).toHaveValue("2.50");
    await user.keyboard("{Escape}");
    await user.dblClick(await cell("12345678901234567890.12"));
    expect(await screen.findByRole("textbox", { name: "wide" })).toHaveValue("12345678901234567890.12");
  });

  it("the_row_panel_starts_from_the_exact_text_too", async () => {
    const { user } = mountNumeric();
    await cell("2.50");
    await user.click(await screen.findByRole("button", { name: "Open row 1" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    await waitFor(() => expect(within(panel).getByLabelText("price")).toHaveValue("2.50"));
    expect(within(panel).getByLabelText("wide")).toHaveValue("12345678901234567890.12");
  });

  it("saving_a_numeric_sends_its_text_checks_the_original_text_and_shows_the_text_back", async () => {
    const { user, f } = mountNumeric();
    await user.dblClick(await cell("2.50"));
    const input = await screen.findByRole("textbox", { name: "price" });
    await user.clear(input);
    await user.type(input, "3.10{Enter}");
    expect(await cell("3.10")).toBeInTheDocument();
    const update = (await bodies(f, "POST", Q)).find((b) => b.tree.op === "update");
    expect(update.tree.set).toEqual({ price: "3.10" });
    // the conflict check compares the original by its text, scale included
    expect(JSON.stringify(update.tree.where)).toContain('"col":"price","eq":"2.50","text":true');
  });

  it("an_infinity_starts_an_edit_as_its_text", async () => {
    const { user } = mountNumeric();
    await user.dblClick(await cell("-Infinity"));
    expect(await screen.findByRole("textbox", { name: "wide" })).toHaveValue("-Infinity");
  });
});
