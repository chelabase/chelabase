import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, FULL, OBJECTS, invoices, obj, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/**
 * RT1 second Playwright pass, ruling 4 (user, 2026-10-21, revised): a view's Structure tab shows real SQL types.
 *
 * The orchestrator's `GET …/tables/{schema}/{table}` now also answers for views, materialized views and foreign tables.
 * Response shape pinned: the generated client's `Table` (packages/management-client `getTable`, unchanged). Its
 * `constraints`, `indexes`, `policies`, `grants` and `options` are REQUIRED arrays there, so for these kinds they come
 * back as empty arrays (not absent); `columns[]` carry `name`, `data_type` (`format_type`, e.g. "numeric(12,2)") and
 * `not_null` as for a table. The fake below serves exactly that.
 *
 * Pins for the coder:
 * - Rows tab: still NO request to that path for a view, materialized view or foreign table (ColumnStrip stays off;
 *   rows-view-fs.test.tsx F3 pins stay at 0 there).
 * - Structure tab: exactly ONE GET to the object's table path (the one query, shared if it is asked twice).
 * - The Columns section is a `<table>` with one `<tr data-column="<name>">` per column, in `columns[]` order: the name as
 *   plain text (not a button), `data-testid="column-type"` holding the `data_type` as its whole text, and the text
 *   "not null" or "null" (as the editable table's Structure tab says it). Read-only: no "Add column" button, no
 *   button for a column name. The kind's note stays ("Views are read-only here.", "Materialized views are read-only here.",
 *   "Foreign tables are read-only here."). The old "Its definition isn't shown; the columns are the ones its rows have."
 *   sentence is gone while the columns come from the endpoint. No `role="alert"`.
 * - The endpoint failing (404, 5xx): today's names-only list (`<ul>` of `<li>` with the bare names, taken from the rows
 *   read) plus the note "Column types aren't available for this object." and no `role="alert"`.
 * - Not pinned: a partitioned table's Structure tab.
 */

const Q = `${B}/console/query`;
const column = (attnum: number, name: string, data_type: string, not_null: boolean) => ({
  attnum, name, data_type, type_oid: 25, not_null, default: null, comment: null, collation: null, generated: null, identity: null, grants: [],
});
const VIEW_COLUMNS = [
  column(1, "id", "uuid", true),
  column(2, "created_at", "timestamptz", false),
  column(3, "total", "numeric(12,2)", false),
  column(4, "memo", "text", false),
];
const VIEW_ROWS = [{ id: "3f9a-1", created_at: "2026-10-01 10:00:00+00", total: "12.90", memo: "m" }];
const definition = (name: string): Table =>
  invoices({ name, columns: VIEW_COLUMNS, constraints: [], indexes: [], policies: [], grants: [], options: [], rls_enabled: false } as unknown as Partial<Table>);

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

const path = (name: string) => `${B}/tables/public/${name}`;
async function open(kind: string, name: string, endpoint?: FakeHandler | null) {
  const objects = [...OBJECTS, obj("public", name, { kind, editable: false })];
  const routes: Record<string, FakeHandler> = { [`POST ${Q}`]: () => jsonResponse(VIEW_ROWS) };
  if (endpoint !== null) routes[`GET ${path(name)}`] = endpoint ?? (() => jsonResponse(definition(name)));
  const s = renderEditor({ params: { ...FULL, table: name }, objects, routes });
  await within(await screen.findByRole("grid", { name: `public.${name}` })).findByRole("gridcell", { name: "3f9a-1" });
  const gets = () => s.f.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname === path(name));
  return { ...s, gets };
}
async function structure(s: Awaited<ReturnType<typeof open>>) {
  await s.user.click(screen.getByRole("tab", { name: "Structure" }));
  return screen.findByRole("tabpanel");
}
const tr = (panel: HTMLElement, name: string) => panel.querySelector<HTMLElement>(`tr[data-column="${name}"]`);

describe("ruling 4: real column types on a view's Structure tab", () => {
  it.each([
    ["view", "v_open", "Views are read-only here."],
    ["materialized_view", "mv_totals", "Materialized views are read-only here."],
    ["foreign_table", "ft_remote", "Foreign tables are read-only here."],
  ])("a %s: one request, then each column with its SQL type and nullability, read-only", async (kind, name, note) => {
    const s = await open(kind, name);
    // the Rows tab asked nothing
    await vi.waitFor(() => expect(s.f.calls.some((r) => r.method === "GET" && new URL(r.url).pathname === `${B}/types`)).toBe(true));
    expect(s.gets()).toHaveLength(0);
    const panel = await structure(s);
    await vi.waitFor(() => expect(tr(panel, "total")).not.toBeNull());
    expect(s.gets()).toHaveLength(1);
    const types = Array.from(panel.querySelectorAll<HTMLElement>("tr[data-column]")).map((r) => [r.getAttribute("data-column"), r.querySelector('[data-testid="column-type"]')?.textContent]);
    expect(types).toEqual([["id", "uuid"], ["created_at", "timestamptz"], ["total", "numeric(12,2)"], ["memo", "text"]]);
    expect(tr(panel, "id")).toHaveTextContent("not null");
    expect(tr(panel, "total")).toHaveTextContent(/\bnull\b/);
    expect(tr(panel, "total")).not.toHaveTextContent("not null");
    expect(within(panel).getByText(note)).toBeInTheDocument();
    expect(within(panel).queryByText(/Its definition isn't shown/)).toBeNull();
    expect(within(panel).queryByRole("button", { name: "Add column" })).toBeNull();
    expect(within(panel).queryByRole("button", { name: "total" })).toBeNull();
    expect(within(panel).getByText("total")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("asks_once_even_when_the_tab_is_left_and_come_back_to", async () => {
    const s = await open("view", "v_open");
    await structure(s);
    await vi.waitFor(() => expect(s.gets()).toHaveLength(1));
    await s.user.click(screen.getByRole("tab", { name: "Rows" }));
    await s.user.click(screen.getByRole("tab", { name: "Structure" }));
    await screen.findByRole("tabpanel");
    expect(s.gets()).toHaveLength(1);
  });

  it.each([
    ["a 404", null],
    ["a 500", () => jsonResponse({ error: { code: "internal", message: "boom" } }, 500)],
  ])("%s falls back to the names read with the rows, a short note and no alert", async (_label, endpoint) => {
    const s = await open("view", "v_open", endpoint as FakeHandler | null);
    const panel = await structure(s);
    await vi.waitFor(() => expect(s.gets().length).toBeGreaterThan(0));
    expect(await within(panel).findByText("Column types aren't available for this object.")).toBeInTheDocument();
    expect(within(panel).getAllByRole("listitem").map((li) => li.textContent)).toEqual(["id", "created_at", "total", "memo"]);
    expect(within(panel).getByText(/Views are read-only here\./)).toBeInTheDocument();
    expect(screen.queryByRole("alert")).toBeNull();
    expect(panel.querySelector('[data-testid="column-type"]')).toBeNull();
  });
});
