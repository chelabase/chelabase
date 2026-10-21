import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { fakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, FULL, ROWS, bodies, editorRoutes, filterByTextKey, obj, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { renderWithQuery } from "@/test/render";
import { TableEditor } from "@/components/table-editor/table-editor";
import { invoices } from "@/lib/table-editor/test-tables";

/**
 * RT1 gf, coder's edge tests beside gf-rows.test.tsx: numeric cells delivered as their exact text (item 3) copy, filter
 * and key the same as numbers did; a footer whose rows on screen are all struck reads like an empty page (item 1).
 */

const Q = `${B}/console/query`;

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

const base = invoices().columns;
const num = (attnum: number, name: string, data_type: string) => ({
  attnum, name, data_type, type_oid: 1700, not_null: false, default: null, comment: null, collation: null, generated: null, identity: null, grants: [],
});
const TABLE = invoices({ columns: [...base, num(6, "price", "numeric(12,2)"), num(7, "wide", "numeric")] } as Partial<Table>);

/** Rows as the real console writes them: numeric literals raw in the JSON (`2.50`), NaN quoted, `text_of` aliases as text. */
const WIRE_ROWS = [
  { id: "r1", customer_id: "c-1", status: "paid", amount_cents: 12900, memo: null, price: "@2.50", wide: "@12345678901234567890.12" },
  { id: "r2", customer_id: "c-2", status: "open", amount_cents: 45000, memo: "Q3 plan", price: "NaN", wide: "@0.10" },
];
const textOf = (v: unknown) => (v === null ? null : String(v).replace(/^@/, ""));
function numericConsole(): FakeHandler {
  return async (req) => {
    const { tree } = await req.clone().json();
    const rows = tree.op === "select" ? (filterByTextKey(tree.where, WIRE_ROWS) as typeof WIRE_ROWS) : WIRE_ROWS;
    const items: { text_of: string; as: string }[] = Array.isArray(tree.select) ? tree.select.filter((i: unknown) => typeof i === "object" && i !== null) : [];
    const out = rows.map((r) => ({ ...r, ...Object.fromEntries(items.map((i) => [i.as, textOf(r[i.text_of as keyof typeof r])])) }));
    const body = JSON.stringify(out).replace(/"@([^"]*)"/g, "$1");
    return new Response(body, { status: 200, headers: { "Content-Type": "application/json" } });
  };
}
function mountNumeric() {
  const routes = editorRoutes({ table: TABLE });
  routes[`POST ${Q}`] = numericConsole();
  const f = fakeFetch(routes);
  const view = renderWithQuery(<TableEditor params={FULL} navigate={vi.fn()} />, { fetch: f });
  return { f, user: userEvent.setup(), ...view };
}
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (name: string) => within(await grid()).findByRole("gridcell", { name });
const rightClick = (user: ReturnType<typeof userEvent.setup>, el: Element) => user.pointer({ keys: "[MouseRight]", target: el });

describe("numeric cells as exact text behave like numbers did", () => {
  it("copy_row_json_writes_the_numeric_as_the_json_number_postgres_wrote", async () => {
    const { user } = mountNumeric();
    const write = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue(undefined);
    await rightClick(user, await cell("2.50"));
    await user.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Copy row (JSON)" }));
    await vi.waitFor(() => expect(write).toHaveBeenCalled());
    const text = write.mock.calls[0][0];
    expect(text).toContain('"price": 2.50');
    expect(text).toContain('"wide": 12345678901234567890.12');
    expect(text).toContain('"amount_cents": 12900');
  });

  it("copy_row_json_keeps_nan_a_string_as_postgres_quotes_it", async () => {
    const { user } = mountNumeric();
    const write = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue(undefined);
    await rightClick(user, await cell("0.10"));
    await user.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Copy row (JSON)" }));
    await vi.waitFor(() => expect(write).toHaveBeenCalled());
    const text = write.mock.calls[0][0];
    expect(text).toContain('"price": "NaN"');
    expect(text).toContain('"wide": 0.10');
  });

  it("copy_row_tsv_writes_the_exact_text", async () => {
    const { user } = mountNumeric();
    const write = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue(undefined);
    await rightClick(user, await cell("2.50"));
    await user.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Copy row (TSV)" }));
    await vi.waitFor(() => expect(write).toHaveBeenCalled());
    expect(write.mock.calls[0][0].split("\n")[1].split("\t").slice(-2)).toEqual(["2.50", "12345678901234567890.12"]);
  });

  it("filter_by_this_value_on_a_numeric_sends_its_exact_text", async () => {
    const { user, f } = mountNumeric();
    await rightClick(user, await cell("2.50"));
    await user.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Filter by this value" }));
    await vi.waitFor(async () => {
      const wheres = (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select").map((b) => JSON.stringify(b.tree.where ?? null));
      expect(wheres.some((w) => w.includes('"col":"price","eq":"2.50"'))).toBe(true);
    });
  });
});

describe("footer when every row on screen is struck", () => {
  it("reads_no_rows_rather_than_an_empty_range", async () => {
    const rt = new FakeRealtime();
    const data = ROWS.map((r) => ({ ...r }));
    renderEditor({
      realtime: rt,
      objects: [obj("public", "invoices", { row_estimate: null, total_bytes: 12582912 })],
      routes: { [`POST ${Q}`]: async (req) => new Response(JSON.stringify(filterByTextKey((await req.clone().json()).tree.where, data)), { status: 200 }) },
    });
    await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
    await screen.findByText("Rows 1–3 of 3");
    rt.rows(1, data.map((r) => ({ op: "delete" as const, keys: [JSON.stringify([r.id])] })));
    await vi.waitFor(() => expect(document.querySelectorAll('[role="row"][data-deleted="true"]').length).toBe(3));
    expect(screen.queryByText(/^Rows 1–0/)).toBeNull();
    expect(screen.getByText("No rows")).toBeInTheDocument();
  });
});
