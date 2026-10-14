import { act, screen, within } from "@testing-library/react";
import { focusManager } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, BRANCHES, D, FULL, PROJECTS, ROLES, ROWS, T, bodies, branch, invoices, meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { DRAFTS_KEY } from "@/lib/table-editor/store";
import { newEdit, tableFingerprint } from "@/lib/table-editor/changeset";

// Final fix round (FB): shell (T2), rows (T3), row panel (T4, E-M1, E-M10), count (T8), E-M2,
// E-M6, E-M7, E-M8 and T7's capability check for the editor.
const Q = `${B}/console/query`;
const SC = `${B}/schema-changes`;
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => !["select", "count"].includes(b.tree.op));
const counts = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "count");
const withColumns = (...extra: Record<string, unknown>[]) => {
  const t = invoices();
  t.columns = [...t.columns, ...extra.map((x, i) => ({ ...t.columns[4], attnum: 6 + i, ...x }))] as Table["columns"];
  return t;
};
const jsonTable = () => withColumns({ name: "meta", data_type: "jsonb", type_oid: 3802 });

async function filterNeqOpen(user: ReturnType<typeof renderEditor>["user"]) {
  await user.click(await screen.findByRole("button", { name: "Filter" }));
  await user.click(screen.getByLabelText("Filter column"));
  await user.click(await screen.findByRole("option", { name: "status" }));
  await user.click(screen.getByLabelText("Filter operator"));
  await user.click(await screen.findByRole("option", { name: "neq" }));
  await user.type(screen.getByLabelText("Filter value"), "open");
  await user.click(screen.getByRole("button", { name: "Apply filter" }));
}

describe("T2: the shell", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("M1_a_project_without_databases_says_so", async () => {
    const me = meAs("admin");
    me.tenant.projects = PROJECTS.map((p) => (p.id === "p1" ? { ...p, databases: [] } : p));
    renderEditor({ me, params: { projectId: "p1" } });
    expect(await screen.findByText("This project has no databases")).toBeInTheDocument();
    expect(screen.queryByText("Pick a table")).toBeNull();
  });

  it("M1_a_database_without_a_ready_branch_says_so", async () => {
    renderEditor({ params: { projectId: "p1", databaseId: "d1" }, routes: { [`GET ${D}/branches`]: () => jsonResponse([branch("b1", "main", null, { status: "creating" })]) } });
    expect(await screen.findByText("No ready branch")).toBeInTheDocument();
  });

  it("M2_the_structure_reason_shows_before_a_table_is_picked", async () => {
    renderEditor({ me: meAs("developer", ["billing_admin"]), params: { ...FULL, table: undefined } });
    expect(await screen.findByText("Pick a table")).toBeInTheDocument();
    expect(await screen.findByTestId("structure-reason")).toHaveTextContent("main is protected: schema changes need Admin");
  });

  it("M3_a_roles_error_keeps_the_table_and_says_so_near_act_as", async () => {
    renderEditor({ routes: { [`GET ${B}/roles`]: () => jsonResponse({ error: { code: "internal", message: "roles exploded" } }, 500) } });
    expect(await grid()).toBeInTheDocument();
    expect(await screen.findByText(/Couldn't load roles/)).toBeInTheDocument();
  });

  it("M8_the_count_line_separates_views_and_rows_describe_their_kind", async () => {
    renderEditor({ params: { ...FULL, table: undefined } });
    const list = await screen.findByRole("navigation", { name: "Tables" });
    expect(await within(list).findByText(/2 tables, 1 view/)).toBeInTheDocument();
    expect(within(list).getByRole("button", { name: "v_open" })).toHaveAccessibleDescription(/view/);
  });
});

describe("T3: rows", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("M1_the_header_checkbox_clears_an_all_matching_selection", async () => {
    const { user } = renderEditor();
    await cell("45000");
    const header = screen.getByRole("checkbox", { name: "Select all rows on this page" });
    await user.click(header);
    await user.click(await screen.findByRole("button", { name: "Select all ≈ 48,210 matching rows" }));
    expect(screen.getByText(/All ≈ ?48,210 matching rows selected/)).toBeInTheDocument();
    await user.click(header);
    expect(header).not.toBeChecked();
    expect(screen.queryByText(/matching rows selected/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Copy" })).toBeNull();
  });

  it("M2_ctrl_c_on_a_json_cell_copies_its_json", async () => {
    const { user } = renderEditor({ table: jsonTable(), rows: [{ ...ROWS[0], meta: { a: 1 } }] });
    await user.click(await cell('{"a":1}'));
    await user.keyboard("{Control>}c{/Control}");
    expect(await navigator.clipboard.readText()).toBe('{"a":1}');
  });

  it("M3_the_halved_page_size_recovers_when_the_filter_changes", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op !== "select") return jsonResponse([]);
      return !b.tree.where && b.tree.limit > 25 ? jsonResponse({ error: { code: "too_large", message: "big" } }, 413) : jsonResponse(ROWS);
    };
    const { f, user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    await cell("45000");
    const first = (await selects(f)).at(-1).tree.limit;
    expect(first).toBeLessThanOrEqual(25);
    await filterNeqOpen(user);
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.where).toBeTruthy());
    expect((await selects(f)).filter((b) => b.tree.where)[0].tree.limit).toBeGreaterThan(25);
  });

  it("M7_resizing_has_a_described_shortcut_and_a_pointer_way_without_dragging", async () => {
    const { user } = renderEditor();
    await cell("45000");
    const header = within(await grid()).getByRole("columnheader", { name: /memo/ });
    expect(header).toHaveAccessibleDescription(/Alt\+(Left|Right)/);
    const grip = screen.getByRole("separator", { name: "Resize memo" });
    const before = Number(grip.getAttribute("aria-valuenow"));
    await user.click(grip);
    await vi.waitFor(() => expect(Number(grip.getAttribute("aria-valuenow"))).toBeGreaterThan(before));
  });

  it("M8_a_key_delete_narrowed_to_fewer_rows_says_how_many_were_deleted", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op === "batch") return jsonResponse([[{ id: "3f9a-1" }], []]);
      return jsonResponse(ROWS);
    };
    const { user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 1" }));
    await user.click(screen.getByRole("checkbox", { name: "Select row 3" }));
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: /^Delete/ }));
    expect(await screen.findByText(/1 of 2 rows/)).toBeInTheDocument();
  });
});

describe("T4 / E-M1 / E-M10: the row panel", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });
  const open = async (user: ReturnType<typeof renderEditor>["user"], n = 2) => {
    await user.click(await screen.findByRole("button", { name: `Open row ${n}` }));
    return screen.findByRole("complementary", { name: "Row" });
  };

  it("T4_json_fields_edit_in_codemirror", async () => {
    const { user } = renderEditor({ table: jsonTable(), rows: ROWS.map((r) => ({ ...r, meta: { a: 1 } })) });
    const panel = await open(user);
    expect(within(panel).getByRole("textbox", { name: "meta" }).closest(".cm-editor")).not.toBeNull();
  });

  it("E-M1_an_untouched_insert_uses_the_insert_with_defaults_endpoint", async () => {
    const insert = vi.fn(() => jsonResponse({ row: { ...ROWS[0], id: "new-1" } }));
    const { f, user } = renderEditor({ routes: { [`POST ${T}/rows/default`]: insert } });
    await user.click(await screen.findByRole("button", { name: "Insert row" }));
    const panel = await screen.findByRole("complementary", { name: "New row" });
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(() => expect(insert).toHaveBeenCalledTimes(1));
    expect(await writes(f)).toHaveLength(0);
  });

  it("E-M10_set_to_now_fills_a_timestamp_field", async () => {
    const { user } = renderEditor({ table: withColumns({ name: "paid_at", data_type: "timestamp with time zone", type_oid: 1184 }), rows: ROWS.map((r) => ({ ...r, paid_at: null })) });
    const panel = await open(user);
    await user.click(within(panel).getByRole("button", { name: "Set paid_at to now" }));
    expect((within(panel).getByLabelText("paid_at") as HTMLInputElement).value).not.toBe("");
  });

  it("E-M10_open_goes_to_the_referenced_table", async () => {
    const { user, navigate } = renderEditor();
    const panel = await open(user);
    await user.click(within(panel).getByRole("button", { name: "Open customer_id" }));
    expect(navigate).toHaveBeenLastCalledWith({ ...FULL, schema: "public", table: "customers" });
  });

  it("E-M10_a_save_error_is_shown_and_the_draft_kept", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op === "update") return jsonResponse({ error: { code: "conflict", message: "duplicate key value violates unique constraint" } }, 409);
      return jsonResponse(ROWS);
    };
    const { user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    const panel = await open(user);
    const memo = within(panel).getByLabelText("memo");
    await user.type(memo, "!");
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    expect(await within(panel).findByText(/duplicate key value/)).toBeInTheDocument();
    expect(memo).toHaveValue("Q3 plan!");
  });

  it("T3_M8_a_panel_delete_that_deletes_nothing_says_so", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op === "delete") return jsonResponse([]);
      return jsonResponse(ROWS);
    };
    const { user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    const panel = await open(user, 3);
    await user.click(within(panel).getByRole("button", { name: "Delete row" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete this row?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete row" }));
    expect(await screen.findByText(/No row was deleted/)).toBeInTheDocument();
  });
});

describe("T8: the count", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });
  afterEach(() => { vi.useRealTimers(); focusManager.setFocused(undefined); });
  let n = 2;
  const answer: FakeHandler = async (req) => {
    const b = await req.clone().json();
    if (b.tree.op === "count") return jsonResponse({ count: n });
    if (b.tree.op === "insert") { n += 1; return jsonResponse([{ ...ROWS[0], id: "new-1" }]); }
    return jsonResponse(b.tree.where ? [ROWS[2], ROWS[0]] : ROWS);
  };

  it("M1_the_count_is_not_sent_again_on_window_focus", async () => {
    n = 2;
    const { f, user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    await filterNeqOpen(user);
    await screen.findByRole("gridcell", { name: "8900" });
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    expect(await screen.findByRole("button", { name: "Select all 2 matching rows" })).toBeInTheDocument();
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(Date.now() + 60_000);
    act(() => { focusManager.setFocused(false); focusManager.setFocused(true); });
    vi.useRealTimers();
    await new Promise((r) => setTimeout(r, 300));
    expect(await counts(f)).toHaveLength(1);
  });

  it("M2_a_row_insert_refreshes_the_count", async () => {
    n = 2;
    const { f, user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    await filterNeqOpen(user);
    await screen.findByRole("gridcell", { name: "8900" });
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    expect(await screen.findByRole("button", { name: "Select all 2 matching rows" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Insert row" }));
    const panel = await screen.findByRole("complementary", { name: "New row" });
    await user.type(within(panel).getByLabelText("customer_id"), "c-9");
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await counts(f)).toHaveLength(2));
    expect(await screen.findByRole("button", { name: "Select all 3 matching rows" })).toBeInTheDocument();
  });
});

describe("E-M2: sort and filter after the table's columns change", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("a_sort_on_a_column_that_is_gone_is_dropped", async () => {
    let renamed = false;
    const table = () => {
      const t = invoices();
      if (renamed) t.columns = t.columns.map((c) => (c.name === "memo" ? { ...c, name: "note" } : c));
      return jsonResponse(t);
    };
    const { f, user, queryClient } = renderEditor({ routes: { [`GET ${T}`]: table } });
    await cell("45000");
    await user.click(within(await grid()).getByRole("columnheader", { name: /memo/ }));
    await vi.waitFor(async () => expect(JSON.stringify((await selects(f)).at(-1).tree)).toContain("memo"));
    renamed = true;
    const before = (await selects(f)).length;
    await act(async () => { await queryClient.invalidateQueries(); });
    await vi.waitFor(async () => expect((await selects(f)).length).toBeGreaterThan(before));
    await vi.waitFor(async () => expect(JSON.stringify((await selects(f)).at(-1).tree)).not.toContain("memo"));
    expect(screen.queryByText(/column "memo" does not exist/)).toBeNull();
  });
});

describe("E-M6: no dry run as the owner before the roles arrive", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("a_developer_s_dry_run_waits_for_the_roles", async () => {
    const K = "d1/b1/app/public.invoices";
    const op = { alter_column: { table: { schema: "public", name: "invoices" }, column: "memo", change: { not_null: true } } };
    sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base: tableFingerprint(invoices()), edits: [newEdit([op])] } }));
    let release: () => void = () => {};
    const gate = new Promise<void>((r) => { release = r; });
    const { f } = renderEditor({
      me: meAs("developer", ["billing_admin"]),
      routes: {
        [`GET ${D}/branches`]: () => jsonResponse(BRANCHES.map((b) => (b.id === "b1" ? { ...b, protected: false } : b))),
        [`GET ${B}/roles`]: async () => { await gate; return jsonResponse(ROLES); },
      },
    });
    await screen.findByRole("complementary", { name: "Pending changes" });
    await new Promise((r) => setTimeout(r, 900));
    expect((await bodies(f, "POST", SC)).filter((b) => b.role === null)).toHaveLength(0);
    release();
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).length).toBeGreaterThan(0), { timeout: 3000 });
    expect((await bodies(f, "POST", SC)).every((b) => b.role === "billing_admin")).toBe(true);
  });
});

describe("E-M7: the FK picker", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("debounces_the_search", async () => {
    const { f, user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Open row 2" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    await user.click(within(panel).getByRole("button", { name: "Pick customer_id" }));
    const dialog = await screen.findByRole("dialog", { name: "Pick from customers" });
    await user.type(within(dialog).getByLabelText("Search"), "lagoon");
    await new Promise((r) => setTimeout(r, 600));
    const picks = (await bodies(f, "POST", Q)).filter((b) => b.tree.from === "public.customers");
    expect(picks.length).toBeLessThanOrEqual(2);
    expect(JSON.stringify(picks.at(-1).tree)).toContain("lagoon");
  });
});

describe("E-M8: Types and New table on a schema switch", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  for (const withTable of [true, false]) {
    it(`leaves_the_types_view_${withTable ? "with" : "without"}_a_table`, async () => {
      const params = withTable ? FULL : { ...FULL, table: undefined };
      const { user, rerender, navigate } = renderEditor({ params });
      const list = await screen.findByRole("navigation", { name: "Tables" });
      await user.click(within(list).getByRole("button", { name: "Types" }));
      expect(await screen.findByRole("region", { name: "Types in public" })).toBeInTheDocument();
      await user.click(within(list).getByLabelText("Schema"));
      await user.click(await screen.findByRole("option", { name: "billing" }));
      const next = navigate.mock.calls.at(-1)![0];
      const { TableEditor } = await import("./table-editor");
      rerender(<TableEditor params={next} navigate={navigate} />);
      await vi.waitFor(() => expect(screen.queryByRole("region", { name: /^Types in/ })).toBeNull());
    });
  }
});

describe("T7: the editor's Insert row follows the capability", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("a_viewer_gets_no_insert_row_a_developer_does", async () => {
    const viewer = renderEditor({ me: meAs("viewer", ["reporter"]) });
    await within(await grid()).findByRole("gridcell", { name: "45000" });
    expect(screen.queryByRole("button", { name: "Insert row" })).toBeNull();
    viewer.unmount();
    renderEditor({ me: meAs("developer", ["billing_admin"]) });
    await within(await grid()).findByRole("gridcell", { name: "45000" });
    expect(screen.getByRole("button", { name: "Insert row" })).toBeInTheDocument();
  });
});
