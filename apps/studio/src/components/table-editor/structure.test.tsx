import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { B, bodies, invoices, meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const SC = `${B}/schema-changes`;

describe("column strip and structure", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("strip_lists_typed_columns", async () => {
    renderEditor();
    const strip = await screen.findByRole("list", { name: "Columns" });
    expect(within(strip).getByRole("button", { name: "amount_cents bigint" })).toBeInTheDocument();
    expect(within(strip).getByRole("button", { name: "status invoice_status" })).toBeInTheDocument();
    expect(within(strip).getByRole("button", { name: "Add column" })).toBeInTheDocument();
  });

  it("add_column_flows_into_the_changes_panel", async () => {
    const { f, user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Add column" }));
    const form = await screen.findByRole("complementary", { name: "New column" });
    await user.type(within(form).getByLabelText("Name"), "discount_cents");
    await user.click(within(form).getByLabelText("Type"));
    await user.click(await screen.findByRole("option", { name: "int8" }));
    await user.click(within(form).getByRole("radio", { name: "Value" }));
    await user.type(within(form).getByLabelText("Default value"), "0");
    await user.click(within(form).getByRole("checkbox", { name: "Not null" }));
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).length).toBeGreaterThan(0), { timeout: 2000 });
    expect(within(within(form).getByRole("region", { name: "SQL preview" })).getByText("SQL 0")).toBeInTheDocument();
    await user.click(within(form).getByRole("button", { name: "Add to changes" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(within(panel).getByRole("heading", { name: "Pending changes · 1" })).toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: "Add column discount_cents int8" })).toBeInTheDocument();
    const strip = screen.getByRole("list", { name: "Columns" });
    expect(within(strip).getByRole("button", { name: "discount_cents int8, new" })).toBeInTheDocument();
    const apply = within(panel).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled(), { timeout: 2000 });
    await user.click(apply);
    expect(await screen.findByRole("status")).toHaveTextContent("Applied 1 change to public.invoices");
    const real = (await bodies(f, "POST", SC)).filter((b) => !b.dry_run);
    expect(real).toEqual([{ database: null, role: null, dry_run: false, operations: [
      { add_column: { table: { schema: "public", name: "invoices" }, column: { name: "discount_cents", data_type: { schema: "pg_catalog", name: "int8", array: false }, not_null: true, default: "'0'", comment: null } } },
    ] }]);
    expect(screen.queryByRole("heading", { name: /Pending changes/ })).toBeNull();
  });

  it("edit_existing_column_from_its_chip", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "memo text" }));
    const form = await screen.findByRole("complementary", { name: "Edit column memo" });
    const name = within(form).getByLabelText("Name");
    await user.clear(name);
    await user.type(name, "note");
    await user.click(within(form).getByRole("button", { name: "Add to changes" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(within(panel).getByRole("button", { name: "Rename column memo → note" })).toBeInTheDocument();
    expect(within(screen.getByRole("list", { name: "Columns" })).getByRole("button", { name: "note text, changed" })).toBeInTheDocument();
  });

  it("drop_from_the_chip_menu_shows_dependents_and_needs_cascade", async () => {
    const dry = async (req: Request) => {
      const b = await req.clone().json();
      if (b.dry_run) return new Response(JSON.stringify({ dry_run: true, committed: false, would_succeed: true, statements: [{ sql: "ALTER TABLE public.invoices DROP COLUMN memo", op_index: 0, executed: true, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null }], dependents: { hash: "h1", objects: [{ kind: "view", name: "v_open", schema: "public", description: "view public.v_open" }] } }), { status: 200, headers: { "Content-Type": "application/json" } });
      return new Response(JSON.stringify({ dry_run: false, committed: true, statements: [] }), { status: 200, headers: { "Content-Type": "application/json" } });
    };
    const { f, user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    await user.click(await screen.findByRole("button", { name: "Column actions for memo" }));
    await user.click(await screen.findByRole("menuitem", { name: "Drop" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(await within(panel).findByText("view public.v_open")).toBeInTheDocument();
    const apply = within(panel).getByRole("button", { name: "Apply 1 change" });
    expect(apply).toBeDisabled();
    await user.click(within(panel).getByRole("checkbox", { name: "Drop dependents too (cascade)" }));
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)).toHaveLength(1));
    expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)[0].cascade).toEqual({ dependents_hash: "h1" });
  });

  it("structure_tab_sections_and_read_only_lists", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("tab", { name: "Structure" }));
    for (const h of ["Columns", "Constraints", "Indexes", "Policies", "Grants", "Table"]) {
      expect(screen.getByRole("heading", { name: h })).toBeInTheDocument();
    }
    expect(screen.getAllByText("Read-only here for now")).toHaveLength(3);
    expect(screen.getByText("invoices_customer_id_fkey")).toBeInTheDocument();
  });

  it("rls_off_banner_stages_turn_on", async () => {
    const { user } = renderEditor({ table: invoices({ rls_enabled: false }) });
    const banner = await screen.findByRole("alert");
    expect(banner).toHaveTextContent("RLS is off: any role with grants reads and writes every row");
    await user.click(within(banner).getByRole("button", { name: "Turn RLS on" }));
    const panel = await screen.findByRole("complementary", { name: "Pending changes" });
    expect(within(panel).getByRole("button", { name: "Turn RLS on" })).toBeInTheDocument();
  });

  it("rls_on_without_policies_note", async () => {
    renderEditor();
    expect(await screen.findByText("RLS is on with no policies: only the owner and roles that bypass RLS see rows.")).toBeInTheDocument();
  });

  it("protected_branch_disables_structure_edits_for_developers", async () => {
    renderEditor({ me: meAs("developer", ["billing_admin"]) });
    expect(await screen.findByRole("button", { name: "Add column" })).toBeDisabled();
    expect(screen.getByTestId("structure-reason")).toHaveTextContent("main is protected: schema changes need Admin");
  });
});
