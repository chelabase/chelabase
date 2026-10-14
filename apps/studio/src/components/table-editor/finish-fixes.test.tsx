import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, OBJECTS, ROWS, bodies, invoices, obj, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { DRAFTS_KEY } from "@/lib/table-editor/store";
import { newEdit, tableFingerprint } from "@/lib/table-editor/changeset";

// End of step 3: the Impeccable finish review's fixes (finish-review.md items) and the fix round's
// leftovers (Apply refreshes the count).
const Q = `${B}/console/query`;
const SC = `${B}/schema-changes`;
const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const draft = (...ops: Parameters<typeof newEdit>[0][]) =>
  sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base: tableFingerprint(invoices()), edits: ops.map((o) => newEdit(o)) } }));
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");

describe("finish review fixes", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("2_a_pending_column_has_a_header_cell_in_the_grid", async () => {
    draft([{ add_column: { table: T, column: { name: "note", data_type: { schema: "pg_catalog", name: "text" } } } }]);
    renderEditor();
    const g = await grid();
    await within(g).findByRole("gridcell", { name: "45000" });
    expect(within(g).getByRole("columnheader", { name: /note/ })).toBeInTheDocument();
  });

  it("3_chips_show_short_type_names_and_keep_the_full_type_in_the_name", async () => {
    renderEditor();
    const strip = await screen.findByRole("list", { name: "Columns" });
    const chip = within(strip).getByRole("button", { name: "amount_cents bigint" });
    expect(chip).toHaveTextContent(/int8/);
    expect(chip).not.toHaveTextContent(/bigint/);
  });

  it("4_a_short_first_page_gives_the_exact_count", async () => {
    renderEditor({ objects: OBJECTS.map((o) => (o.name === "invoices" ? obj("public", "invoices", { row_estimate: null }) : o)) });
    await within(await grid()).findByRole("gridcell", { name: "45000" });
    expect(await screen.findByText("Rows 1–3 of 3")).toBeInTheDocument();
    expect(screen.queryByText(/of many/)).toBeNull();
  });

  it("5_an_identity_always_column_is_read_only_in_a_new_row", async () => {
    const t = invoices();
    t.columns = t.columns.map((c) => (c.name === "id" ? { ...c, identity: { generation: "always", sequence_oid: 1, sequence_name: "invoices_id_seq", params: {}, grants: [] } } : c)) as Table["columns"];
    const { user } = renderEditor({ table: t });
    await user.click(await screen.findByRole("button", { name: "Insert row" }));
    const panel = await screen.findByRole("complementary", { name: "New row" });
    expect(within(panel).getByLabelText("id")).toHaveAttribute("readonly");
  });

  it("7_a_branch_without_a_postgres_database_disables_the_list_tools", async () => {
    renderEditor({ params: { projectId: "p1", databaseId: "d1", branchId: "b1" }, routes: { [`GET ${B}/databases`]: () => jsonResponse([]) } });
    const list = await screen.findByRole("navigation", { name: "Tables" });
    expect(await within(list).findByText("No database")).toBeInTheDocument();
    expect(within(list).queryByText("Loading…")).toBeNull();
    expect(within(list).getByRole("button", { name: "Types" })).toBeDisabled();
    expect(within(list).getByRole("button", { name: "New table" })).toBeDisabled();
  });

  it("8_a_column_header_is_named_by_its_label_only", async () => {
    renderEditor();
    const g = await grid();
    expect(within(g).getByRole("columnheader", { name: "memo" })).toBeInTheDocument();
  });

  it("9_the_table_bar_has_a_sort_tool", async () => {
    const { f, user } = renderEditor();
    await within(await grid()).findByRole("gridcell", { name: "45000" });
    await user.click(screen.getByRole("button", { name: "Sort" }));
    await user.click(screen.getByLabelText("Sort column"));
    await user.click(await screen.findByRole("option", { name: "memo" }));
    await user.click(screen.getByLabelText("Sort direction"));
    await user.click(await screen.findByRole("option", { name: "Descending" }));
    await user.click(screen.getByRole("button", { name: "Apply sort" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy?.[0]).toEqual({ col: "memo", dir: "desc" }));
  });

  it("10_a_passed_dry_run_is_one_line_with_the_lock", async () => {
    draft([{ alter_column: { table: T, column: "memo", change: { not_null: true } } }]);
    renderEditor();
    const p = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(await within(p).findByText(/^Dry run passed · .*lock/i)).toBeInTheDocument();
  });

  it("14_the_row_panel_collapses_on_structure_and_returns_on_rows", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Open row 2" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    await user.type(within(panel).getByLabelText("memo"), "!");
    await user.click(screen.getByRole("tab", { name: "Structure" }));
    expect(screen.queryByRole("complementary", { name: "Row" })).toBeNull();
    await user.click(screen.getByRole("tab", { name: "Rows" }));
    expect(within(await screen.findByRole("complementary", { name: "Row" })).getByLabelText("memo")).toHaveValue("Q3 plan!");
  });

  it("leftover_apply_refreshes_the_selection_count", async () => {
    draft([{ alter_column: { table: T, column: "memo", change: { not_null: true } } }]);
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op === "count") return jsonResponse({ count: 2 });
      return jsonResponse(b.tree.where ? [ROWS[2], ROWS[0]] : ROWS);
    };
    const { f, user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    await user.click(await screen.findByRole("button", { name: "Filter" }));
    await user.click(screen.getByLabelText("Filter column"));
    await user.click(await screen.findByRole("option", { name: "status" }));
    await user.click(screen.getByLabelText("Filter operator"));
    await user.click(await screen.findByRole("option", { name: "neq" }));
    await user.type(screen.getByLabelText("Filter value"), "open");
    await user.click(screen.getByRole("button", { name: "Apply filter" }));
    await screen.findByRole("gridcell", { name: "8900" });
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    expect(await screen.findByRole("button", { name: "Select all 2 matching rows" })).toBeInTheDocument();
    const counts = async () => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "count").length;
    const before = await counts();
    const p = await screen.findByRole("complementary", { name: "Pending changes" });
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).some((b) => !b.dry_run)).toBe(true));
    await vi.waitFor(async () => expect(await counts()).toBeGreaterThan(before));
  });
});
