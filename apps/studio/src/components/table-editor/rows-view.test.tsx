import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, FULL, ROWS, bodies, invoices, meAs, obj, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const Q = `${B}/console/query`;
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");
// Reads are `select` and (T8) `count`; everything else is a write.
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");

describe("RowsView", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("reads_the_first_page_as_the_owner", async () => {
    const { f } = renderEditor();
    expect(await cell("45000")).toBeInTheDocument();
    const q = (await selects(f)).at(-1);
    expect(q.role).toBeNull();
    expect(q.tree).toMatchObject({ v: 1, op: "select", from: "public.invoices", limit: 50, offset: 0, orderBy: [{ col: "id", dir: "asc" }] });
    expect(screen.getByText("Rows 1–3 of ≈ 48,210")).toBeInTheDocument();
  });

  it("acts_as_the_chosen_role", async () => {
    const { f } = renderEditor({ me: meAs("developer", ["billing_admin"]) });
    await cell("45000");
    expect((await selects(f)).at(-1).role).toBe("billing_admin");
  });

  // A short first page is the exact count (finish fix 4); a full one without an estimate says "of more".
  it("estimate_unknown_and_a_full_page_reads_of_more", async () => {
    const many = Array.from({ length: 50 }, (_, i) => ({ ...ROWS[0], id: `r-${i}`, amount_cents: i }));
    renderEditor({ objects: [obj("public", "invoices", { row_estimate: null })], rows: many });
    expect(await screen.findByText("Rows 1–50 of more")).toBeInTheDocument();
    expect(screen.queryByText(/^1 \//)).toBeNull();
  });

  it("a_short_page_without_an_estimate_shows_its_count_in_the_bar_the_list_and_the_pager", async () => {
    renderEditor({ objects: [obj("public", "invoices", { row_estimate: null })] });
    expect(await screen.findByText("Rows 1–3 of 3")).toBeInTheDocument();
    expect(screen.getByText("1 / 1")).toBeInTheDocument();
    expect(screen.getByText(/^3 rows · /)).toBeInTheDocument();
    expect(within(screen.getByRole("navigation", { name: "Tables" })).getByRole("button", { name: "invoices" })).toHaveTextContent("3");
  });

  it("the_header_menu_sorts_and_clears", async () => {
    const { f, user } = renderEditor();
    const pick = async (item: string) => {
      await user.click(within(await grid()).getByRole("button", { name: "Column actions for amount_cents" }));
      await user.click(await screen.findByRole("menuitem", { name: item }));
    };
    await within(await grid()).findByRole("columnheader", { name: /amount_cents/ });
    await pick("Sort ascending");
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy).toEqual([{ col: "amount_cents", dir: "asc" }, { col: "id", dir: "asc" }]));
    await pick("Sort descending");
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy[0]).toEqual({ col: "amount_cents", dir: "desc" }));
    await pick("Clear sort");
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.orderBy).toEqual([{ col: "id", dir: "asc" }]));
  });

  it("paging_moves_the_offset", async () => {
    const many = Array.from({ length: 50 }, (_, i) => ({ ...ROWS[0], id: `r-${i}`, amount_cents: i }));
    const { f, user } = renderEditor({ rows: many });
    // Rows are virtualized: row 50 is never mounted in an 800px viewport, so wait for the footer.
    expect(await screen.findByText("Rows 1–50 of ≈ 48,210")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Next page" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.offset).toBe(50));
  });

  it("inline_edit_saves_one_update_by_key", async () => {
    const { f, user } = renderEditor();
    await user.dblClick(await cell("45000"));
    const input = await screen.findByRole("textbox", { name: "amount_cents" });
    await user.clear(input);
    await user.type(input, "46000{Enter}");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "update", table: "public.invoices", set: { amount_cents: 46000 }, where: { and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "45000", text: true }] }, returning: "*" });
  });

  it("escape_cancels_an_edit", async () => {
    const { f, user } = renderEditor();
    await user.dblClick(await cell("45000"));
    await user.type(await screen.findByRole("textbox", { name: "amount_cents" }), "9{Escape}");
    expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull();
    expect(await writes(f)).toHaveLength(0);
  });

  it("a_refused_edit_shows_its_error_at_the_cell", async () => {
    const refuse: FakeHandler = async (req) => {
      const b = await req.clone().json();
      return b.tree.op === "select" ? jsonResponse(ROWS) : jsonResponse({ error: { code: "permission_denied", message: "raw" } }, 403);
    };
    const { user } = renderEditor({ me: meAs("developer", ["billing_admin"]), routes: { [`POST ${Q}`]: refuse } });
    await user.dblClick(await cell("45000"));
    await user.type(await screen.findByRole("textbox", { name: "amount_cents" }), "1{Enter}");
    expect(await screen.findByText("billing_admin can't update this table.")).toBeInTheDocument();
  });

  it("keyboard_moves_and_enter_edits", async () => {
    const { user } = renderEditor();
    await user.click(await cell("12900"));
    await user.keyboard("{ArrowDown}");
    expect(document.activeElement).toHaveAccessibleName("45000");
    await user.keyboard("{ArrowRight}");
    expect(document.activeElement).toHaveAccessibleName("Q3 plan");
    await user.keyboard("{Enter}");
    expect(await screen.findByRole("textbox", { name: "memo" })).toHaveValue("Q3 plan");
  });

  it("no_primary_key_is_read_only", async () => {
    const t = invoices({ constraints: [] });
    const { f, user } = renderEditor({ table: t });
    expect(await screen.findByText("This table has no primary key, so its rows are read-only here.")).toBeInTheDocument();
    await user.dblClick(await cell("45000"));
    expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull();
    expect(await writes(f)).toHaveLength(0);
  });

  it("viewers_read_only", async () => {
    const { user } = renderEditor({ me: meAs("viewer", ["reporter"]) });
    await user.dblClick(await cell("45000"));
    expect(screen.queryByRole("textbox", { name: "amount_cents" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Insert row" })).toBeNull();
  });

  it("data_api_off_gates_rows_and_offers_enable", async () => {
    const enable = vi.fn(() => jsonResponse({ status: "accepted" }, 202));
    const { user } = renderEditor({
      me: meAs("developer", ["billing_admin"]),
      params: { ...FULL, pgDatabase: "reporting" },
      routes: { [`POST ${B}/pg-databases/reporting/data-api`]: enable },
    });
    expect(await screen.findByTestId("rows-gate")).toHaveTextContent("Rows need the data API on reporting");
    expect(screen.queryByRole("grid")).toBeNull();
    await user.click(screen.getByRole("button", { name: "Enable data API" }));
    await vi.waitFor(() => expect(enable).toHaveBeenCalled());
  });

  it("selection_page_then_all_matching", async () => {
    const { user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    expect(screen.getByText("3 selected")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Select all ≈ 48,210 matching rows" }));
    expect(screen.getByText("All ≈ 48,210 matching rows selected")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Clear selection" }));
    expect(screen.queryByText(/selected/)).toBeNull();
  });

  it("delete_selected_sends_one_batch_by_key", async () => {
    const { f, user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 1" }));
    await user.click(screen.getByRole("checkbox", { name: "Select row 3" }));
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete 2 rows?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete 2 rows" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "batch", ops: [
      { op: "delete", from: "public.invoices", where: { col: "id", eq: "3f9a-1", parse: true }, returning: ["id"] },
      { op: "delete", from: "public.invoices", where: { col: "id", eq: "c2d1-3", parse: true }, returning: ["id"] },
    ] });
  });

  it("delete_all_matching_without_a_filter_needs_the_table_name", async () => {
    const { f, user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    await user.click(screen.getByRole("button", { name: "Select all ≈ 48,210 matching rows" }));
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete all matching rows?" });
    const confirm = within(dialog).getByRole("button", { name: "Delete all rows" });
    expect(confirm).toBeDisabled();
    await user.type(within(dialog).getByLabelText("Type invoices to confirm"), "invoices");
    await user.click(confirm);
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "delete", from: "public.invoices", all: true });
  });

  it("delete_all_matching_with_a_filter_uses_it", async () => {
    const { f, user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(screen.getByLabelText("Filter column"));
    await user.click(await screen.findByRole("option", { name: "status" }));
    await user.click(screen.getByLabelText("Filter operator"));
    await user.click(await screen.findByRole("option", { name: "eq" }));
    await user.type(screen.getByLabelText("Filter value"), "void");
    await user.click(screen.getByRole("button", { name: "Apply filter" }));
    await vi.waitFor(async () => expect((await selects(f)).at(-1).tree.where).toEqual({ col: "status", eq: "void" }));
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    await user.click(screen.getByRole("button", { name: /^Select all .*matching rows$/ }));
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete all matching rows?" });
    expect(within(dialog).queryByLabelText("Type invoices to confirm")).toBeNull();
    await user.click(within(dialog).getByRole("button", { name: "Delete matching rows" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree).toEqual({ v: 1, op: "delete", from: "public.invoices", where: { col: "status", eq: "void" } });
  });

  it("copy_as_tsv_and_json", async () => {
    const { user } = renderEditor();
    await cell("45000");
    await user.click(screen.getByRole("checkbox", { name: "Select row 2" }));
    await user.click(screen.getByRole("button", { name: "Copy" }));
    expect(await navigator.clipboard.readText()).toBe("id\tcustomer_id\tstatus\tamount_cents\tmemo\n81c4-2\tc-2\topen\t45000\tQ3 plan");
    await user.click(screen.getByRole("button", { name: "Copy as JSON" }));
    expect(JSON.parse(await navigator.clipboard.readText())).toEqual([ROWS[1]]);
  });

  it("too_large_halves_the_page", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      return b.tree.limit > 25 ? jsonResponse({ error: { code: "too_large", message: "big" } }, 413) : jsonResponse(ROWS);
    };
    const { f } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    expect(await cell("45000")).toBeInTheDocument();
    expect((await selects(f)).map((b) => b.tree.limit)).toEqual([50, 25]);
  });

  it("long_values_are_cut_to_200_characters", async () => {
    renderEditor({ rows: [{ ...ROWS[0], memo: "x".repeat(500) }] });
    expect(await cell(`${"x".repeat(200)}…`)).toBeInTheDocument();
  });
});

