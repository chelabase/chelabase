import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fakeFetch, jsonResponse, type FakeFetch } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { B, D, FULL, T, editorRoutes, meAs, stubLayout, type Level } from "@/test/table-editor-fixtures";
import type { EditorParams } from "@/lib/table-editor/context";
import { createTableEditorStore } from "@/lib/table-editor/store";
import { TableEditor } from "./table-editor";

/**
 * m4 (#86): the SQL drawer under the live table. SQL beside New table toggles it (the URL's `?sql=1`); a run that
 * changes the schema refreshes the open table, the object list and the types; Full editor goes to the SQL tab at the
 * same place with the buffer saved first; the row panel's unsaved draft is asked about first; Viewers get no SQL.
 */

const ndjson = (...events: unknown[]) => new Response(events.map((e) => JSON.stringify(e)).join("\n") + "\n", { status: 200, headers: { "Content-Type": "application/x-ndjson" } });
const ranTag = (tag: string) => () => ndjson({ t: "complete", statement: 0, tag, rows: 0, truncated: false }, { t: "done", outcome: "ok", duration_ms: 3 });

function mount(o: { level?: Level; sql?: boolean; run?: () => Response; params?: EditorParams } = {}) {
  const navigate = vi.fn();
  const openSqlEditor = vi.fn();
  const f: FakeFetch = fakeFetch(
    editorRoutes({
      me: meAs(o.level ?? "admin"),
      routes: {
        [`GET ${B}/sql/schema`]: () => jsonResponse({ schemas: [] }),
        [`GET ${D}/sql-queries`]: () => jsonResponse([]),
        [`POST ${B}/sql`]: o.run ?? ranTag("SELECT 0"),
      },
    }),
  );
  const ui = (sql: boolean) => (
    <TableEditor params={o.params ?? FULL} navigate={navigate} sql={sql} openSqlEditor={openSqlEditor} createSqlClient={() => Promise.reject(new Error("no worker"))} />
  );
  const result = renderWithQuery(ui(o.sql ?? false), { fetch: f });
  return { ...result, f, navigate, openSqlEditor, user: userEvent.setup(), setSql: (sql: boolean) => result.rerender(ui(sql)) };
}

async function setDoc(text: string) {
  const v = await waitFor(() => {
    const found = document.querySelector(".cm-editor");
    expect(found).not.toBeNull();
    return EditorView.findFromDOM(found as HTMLElement)!;
  });
  act(() => v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: text } }));
}
const gets = (f: FakeFetch, path: string) => f.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname === path).length;
const narrow = (matches: boolean) =>
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: matches && q.includes("40rem"), media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  narrow(false);
});
afterEach(() => vi.unstubAllGlobals());

describe("the SQL drawer", () => {
  it("sql_toggles_the_drawer_through_the_url_and_keeps_the_grid", async () => {
    const { navigate, user, setSql } = mount();
    await user.click(await screen.findByRole("button", { name: "SQL" }));
    expect(navigate).toHaveBeenLastCalledWith(FULL, { sql: true, replace: true });
    expect(screen.queryByRole("region", { name: "SQL" })).toBeNull();

    setSql(true);
    const drawer = await screen.findByRole("region", { name: "SQL" });
    expect(await within(drawer).findByRole("button", { name: "Run" })).toBeInTheDocument();
    expect(within(drawer).getByText("on main · app · as owner")).toBeInTheDocument();
    // The grid stays above it.
    expect(screen.getByRole("heading", { name: "public.invoices" })).toBeInTheDocument();
    expect(screen.getByRole("separator", { name: "Resize SQL" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "SQL" })).toHaveAttribute("aria-pressed", "true");

    await user.click(within(drawer).getByRole("button", { name: "Close SQL" }));
    expect(navigate).toHaveBeenLastCalledWith(FULL, { sql: false, replace: true });
  });

  it("below_40rem_sql_opens_the_full_editor_instead", async () => {
    narrow(true);
    const { navigate, openSqlEditor, user } = mount();
    await user.click(await screen.findByRole("button", { name: "SQL" }));
    expect(openSqlEditor).toHaveBeenCalledWith(FULL);
    expect(navigate).not.toHaveBeenCalledWith(FULL, expect.objectContaining({ sql: true }));
  });

  it("a_run_that_changes_the_schema_refreshes_the_table_objects_and_types", async () => {
    const { f, user } = mount({ sql: true, run: ranTag("ALTER TABLE") });
    const drawer = await screen.findByRole("region", { name: "SQL" });
    await screen.findByRole("heading", { name: "public.invoices" });
    await waitFor(() => expect(gets(f, T)).toBeGreaterThan(0));
    const before = { table: gets(f, T), objects: gets(f, `${B}/objects`), types: gets(f, `${B}/types`) };
    await setDoc("alter table invoices add column notes text");
    await user.click(await within(drawer).findByRole("button", { name: "Run" }));
    expect(await within(drawer).findByText(/ALTER TABLE/)).toBeInTheDocument();
    await waitFor(() => {
      expect(gets(f, T)).toBeGreaterThan(before.table);
      expect(gets(f, `${B}/objects`)).toBeGreaterThan(before.objects);
      expect(gets(f, `${B}/types`)).toBeGreaterThan(before.types);
    });
  });

  it("a_dml_only_run_refreshes_no_structure", async () => {
    const { f, user } = mount({ sql: true, run: ranTag("INSERT 0 1") });
    const drawer = await screen.findByRole("region", { name: "SQL" });
    await screen.findByRole("heading", { name: "public.invoices" });
    await waitFor(() => expect(gets(f, T)).toBeGreaterThan(0));
    const before = { table: gets(f, T), objects: gets(f, `${B}/objects`), types: gets(f, `${B}/types`) };
    await setDoc("insert into invoices default values");
    await user.click(await within(drawer).findByRole("button", { name: "Run" }));
    expect(await within(drawer).findByText(/INSERT 0 1/)).toBeInTheDocument();
    await act(() => new Promise((r) => setTimeout(r, 50)));
    expect({ table: gets(f, T), objects: gets(f, `${B}/objects`), types: gets(f, `${B}/types`) }).toEqual(before);
  });

  it("full_editor_saves_the_buffer_then_opens_the_sql_tab_at_the_same_place", async () => {
    const { openSqlEditor, user } = mount({ sql: true });
    const drawer = await screen.findByRole("region", { name: "SQL" });
    await within(drawer).findByRole("button", { name: "Run" });
    const seen: (string | null)[] = [];
    openSqlEditor.mockImplementation(() => seen.push(localStorage.getItem("chelabase.sql.buffer.u2.d1")));
    await setDoc("select 42");
    await user.click(within(drawer).getByRole("button", { name: "Full editor" }));
    expect(openSqlEditor).toHaveBeenCalledWith(FULL);
    // Saved before the SQL tab reads it (the debounced save hasn't run yet).
    expect(seen).toEqual(["select 42"]);
  });

  it("the_act_as_role_carries_to_the_other_page_through_session_storage", () => {
    const a = createTableEditorStore(sessionStorage);
    a.getState().setActAs("d1/b1", "reporter");
    a.getState().setActAs("d1/b2", null);
    const b = createTableEditorStore(sessionStorage);
    expect(b.getState().actAs).toEqual({ "d1/b1": "reporter", "d1/b2": null });
  });

  it("full_editor_with_an_unsaved_row_asks_first_and_keep_editing_stays", async () => {
    const { openSqlEditor, user } = mount({ sql: true });
    await user.click(await screen.findByRole("button", { name: "Open row 2" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    await user.type(within(panel).getByLabelText("amount_cents"), "1");
    const drawer = screen.getByRole("region", { name: "SQL" });
    await user.click(await within(drawer).findByRole("button", { name: "Full editor" }));
    const d = await screen.findByRole("dialog", { name: "Discard changes to row 2?" });
    await user.click(within(d).getByRole("button", { name: "Keep editing" }));
    expect(openSqlEditor).not.toHaveBeenCalled();
    expect(within(screen.getByRole("complementary", { name: "Row" })).getByLabelText("amount_cents")).toHaveValue("450001");

    await user.click(within(drawer).getByRole("button", { name: "Full editor" }));
    await user.click(within(await screen.findByRole("dialog", { name: "Discard changes to row 2?" })).getByRole("button", { name: "Discard" }));
    expect(openSqlEditor).toHaveBeenCalledWith(FULL);
  });

  it("a_viewer_gets_no_sql_button_and_no_drawer", async () => {
    mount({ level: "viewer", sql: true });
    await screen.findByRole("heading", { name: "public.invoices" });
    expect(screen.getByRole("button", { name: "New table" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "SQL" })).toBeNull();
    expect(screen.queryByRole("region", { name: "SQL" })).toBeNull();
  });
});
