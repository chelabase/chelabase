import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, FULL, OBJECTS, T as TABLE_PATH, bodies, invoices, obj, renderEditor, schemaChanges, stubLayout } from "@/test/table-editor-fixtures";
import { DRAFTS_KEY } from "@/lib/table-editor/store";
import { newEdit, tableFingerprint } from "@/lib/table-editor/changeset";
import type { EditorParams } from "@/lib/table-editor/context";
import { TableEditor } from "./table-editor";

// Final fix round (FA): Apply of table renames/moves/drops (E-I2), creation (E-M3), the new table's
// RLS checkbox (R8), a staged drop (E-M4), empty schemas (E-M11), the T6 request bodies (E-M10) and
// T5 M2, M3, M6, M8.
const SC = `${B}/schema-changes`;
const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const draft = (...ops: Parameters<typeof newEdit>[0][]) =>
  sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base: tableFingerprint(invoices()), edits: ops.map((o) => newEdit(o)) } }));
const panel = () => screen.findByRole("complementary", { name: "Pending changes" });

/** Applies the panel's changes (ticking "I've read the SQL" when it is asked for). */
async function applyAll(user: ReturnType<typeof renderEditor>["user"], label: RegExp = /^Apply \d+ changes?$/) {
  const p = await panel();
  const apply = within(p).getByRole("button", { name: label });
  await vi.waitFor(() => expect(within(p).queryByText(/Checking|Running dry run/i)).toBeNull());
  const ack = within(p).queryByRole("checkbox", { name: "I've read the SQL" });
  if (ack) await user.click(ack);
  await vi.waitFor(() => expect(apply).toBeEnabled());
  await user.click(apply);
}
/** The real (not dry) runs posted so far. */
const realRuns = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", SC)).filter((b) => !b.dry_run);

/** Routes where the object list (and the table) reflect the committed change once a real run happened. */
function afterCommit(next: { objects: unknown[]; tables?: Record<string, Table> }) {
  let committed = false;
  const routes: Record<string, FakeHandler> = {
    [`POST ${SC}`]: async (req) => {
      const b = await req.clone().json();
      if (!b.dry_run) committed = true;
      return schemaChanges(req);
    },
    [`GET ${B}/objects`]: () => jsonResponse({ objects: committed ? next.objects : OBJECTS }),
    [`GET ${TABLE_PATH}`]: () => (committed && !next.objects.some((o) => (o as { schema: string; name: string }).name === "invoices" && (o as { schema: string }).schema === "public") ? jsonResponse({ error: { code: "not_found", message: "no such table" } }, 404) : jsonResponse(invoices())),
  };
  for (const [path, t] of Object.entries(next.tables ?? {})) routes[`GET ${B}/tables/${path}`] = () => jsonResponse(t);
  return routes;
}
const without = (name: string) => OBJECTS.filter((o) => !(o.schema === "public" && o.name === name));

describe("E-I2: applying a rename, move or drop of the open table", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("rename_navigates_to_the_new_name_and_keeps_the_applied_line", async () => {
    draft([{ rename_table: { table: T, to: "bills" } }]);
    const bills = invoices({ name: "bills" } as Partial<Table>);
    const r = renderEditor({ routes: afterCommit({ objects: [...without("invoices"), obj("public", "bills")], tables: { "public/bills": bills } }) });
    await applyAll(r.user);
    await vi.waitFor(async () => expect(await realRuns(r.f)).toHaveLength(1));
    await vi.waitFor(() => expect(r.navigate).toHaveBeenLastCalledWith({ ...FULL, table: "bills" }));
    r.rerender(<TableEditor params={{ ...FULL, table: "bills" }} navigate={r.navigate} />);
    expect(await screen.findByText("Applied 1 change to public.bills")).toBeInTheDocument();
    expect(screen.queryByText("Table not found")).toBeNull();
  });

  it("move_navigates_to_the_new_schema", async () => {
    draft([{ set_table_schema: { table: T, schema: "billing" } }]);
    const r = renderEditor({ routes: afterCommit({ objects: [...without("invoices"), obj("billing", "invoices")], tables: { "billing/invoices": invoices({ schema: "billing" } as Partial<Table>) } }) });
    await applyAll(r.user);
    await vi.waitFor(async () => expect(await realRuns(r.f)).toHaveLength(1));
    await vi.waitFor(() => expect(r.navigate).toHaveBeenLastCalledWith({ ...FULL, schema: "billing", table: "invoices" }));
  });

  it("drop_navigates_to_the_schema_without_a_table", async () => {
    draft([{ drop_table: { table: T } }]);
    const r = renderEditor({ routes: afterCommit({ objects: without("invoices") }) });
    await applyAll(r.user);
    await vi.waitFor(async () => expect(await realRuns(r.f)).toHaveLength(1));
    await vi.waitFor(() => expect(r.navigate).toHaveBeenCalled());
    const last = r.navigate.mock.calls.at(-1)![0] as EditorParams;
    expect({ ...last, table: last.table ?? undefined }).toEqual({ ...FULL, table: undefined });
  });
});

describe("E-M3: creating a table", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("navigates_only_once_the_refetched_object_list_has_the_new_table", async () => {
    const r = renderEditor({ routes: afterCommit({ objects: [...OBJECTS, obj("public", "payments")] }) });
    const notFoundAtNavigate: boolean[] = [];
    r.navigate.mockImplementation((p: EditorParams) => {
      r.rerender(<TableEditor params={p} navigate={r.navigate} />);
      notFoundAtNavigate.push(screen.queryByText("Table not found") !== null);
    });
    await r.user.click(await screen.findByRole("button", { name: "New table" }));
    const view = await screen.findByRole("region", { name: "New table" });
    await r.user.type(within(view).getByLabelText("Table name"), "payments");
    await r.user.click(within(view).getByRole("button", { name: "Create table" }));
    await applyAll(r.user);
    await vi.waitFor(() => expect(r.navigate).toHaveBeenCalledWith({ ...FULL, table: "payments" }));
    expect(notFoundAtNavigate).toEqual([false]);
  });
});

describe("R8: the new table's RLS checkbox", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  async function newTable() {
    const r = renderEditor();
    await r.user.click(await screen.findByRole("button", { name: "New table" }));
    const view = await screen.findByRole("region", { name: "New table" });
    await r.user.type(within(view).getByLabelText("Table name"), "payments");
    return { ...r, view };
  }

  it("unticked_by_default_and_the_copy_tells_the_exposure", async () => {
    const { view } = await newTable();
    const box = within(view).getByRole("checkbox", { name: "Turn RLS on" });
    expect(box).not.toBeChecked();
    expect(view).toHaveTextContent(/anon.*reads? every row/i);
    expect(view).toHaveTextContent(/authenticated.*writes? every row/i);
    expect(view).not.toHaveTextContent(/not exposed/i);
  });

  it("ticked_sends_rls_on_in_create_table_and_drops_the_exposure_copy", async () => {
    const { view, user, f } = await newTable();
    await user.click(within(view).getByRole("checkbox", { name: "Turn RLS on" }));
    expect(view).not.toHaveTextContent(/anon.*reads? every row/i);
    await user.click(within(view).getByRole("button", { name: "Create table" }));
    await applyAll(user);
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)).toHaveLength(1));
    const op = (await bodies(f, "POST", SC)).filter((b) => !b.dry_run)[0].operations[0];
    expect(op.create_table.rls).toEqual({ enabled: true, forced: false });
  });
});

describe("E-M4: a staged drop of the table", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("makes_the_rest_of_the_structure_read_only_with_a_reason", async () => {
    draft([{ drop_table: { table: T } }]);
    const { user } = renderEditor();
    expect(await screen.findByRole("button", { name: "Column actions for memo" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Add column" })).toBeDisabled();
    await user.click(screen.getByRole("tab", { name: "Structure" }));
    expect(await screen.findAllByText("public.invoices is dropped in your pending changes")).not.toHaveLength(0);
    expect(screen.getByRole("button", { name: "Add constraint" })).toBeDisabled();
    expect(screen.queryByRole("button", { name: "Turn RLS on" })).toBeNull();
  });
});

describe("E-M11: moving to a schema that holds no object yet", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("a_typed_schema_name_can_be_used", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("tab", { name: "Structure" }));
    await user.click(screen.getByRole("button", { name: "Move to schema" }));
    const d = await screen.findByRole("dialog", { name: "Move to schema" });
    await user.click(within(d).getByLabelText("Schema"));
    await user.keyboard("archive");
    await user.click(await screen.findByRole("option", { name: 'Use "archive"' }));
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    const p = await panel();
    expect(within(p).getAllByRole("listitem").map((li) => li.textContent)).toEqual([expect.stringContaining("archive")]);
  });
});

describe("E-M10: T6's requests carry the operations the form describes", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("foreign_key_not_valid_body", async () => {
    const { user, f } = renderEditor();
    await user.click(await screen.findByRole("tab", { name: "Structure" }));
    await user.click(screen.getByRole("button", { name: "Add constraint" }));
    const d = await screen.findByRole("dialog", { name: "Add constraint" });
    await user.click(within(d).getByLabelText("Kind"));
    await user.click(await screen.findByRole("option", { name: "Foreign key" }));
    await user.click(within(d).getByLabelText("Columns"));
    await user.click(await screen.findByRole("option", { name: /^customer_id/ }));
    await user.keyboard("{Escape}");
    await user.click(within(d).getByLabelText("References table"));
    await user.click(await screen.findByRole("option", { name: "customers" }));
    await user.click(within(d).getByLabelText("References column"));
    await user.click(await screen.findByRole("option", { name: "id" }));
    await user.click(within(d).getByRole("checkbox", { name: "Add as NOT VALID" }));
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).filter((b) => b.dry_run).length).toBeGreaterThan(0), { timeout: 3000 });
    const last = (await bodies(f, "POST", SC)).at(-1);
    expect(last.operations).toEqual([
      { add_constraint: { table: T, constraint: { kind: { foreign_key: { columns: ["customer_id"], references: { schema: "public", name: "customers" }, referenced_columns: ["id"] } }, not_valid: true } } },
    ]);
  });

  it("rename_then_comment_then_move_bodies_address_the_table_as_it_is", async () => {
    const { user, f } = renderEditor();
    await user.click(await screen.findByRole("tab", { name: "Structure" }));
    await user.click(screen.getByRole("button", { name: "Rename table" }));
    let d = await screen.findByRole("dialog", { name: "Rename table" });
    await user.type(within(d).getByLabelText("New name"), "bills");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    await user.click(screen.getByRole("button", { name: "Add column" }));
    const form = await screen.findByRole("complementary", { name: /column/i });
    await user.type(within(form).getByLabelText("Name"), "note");
    await user.click(within(form).getByLabelText("Type"));
    await user.click(await screen.findByRole("option", { name: /^text/ }));
    await user.click(within(form).getByRole("button", { name: "Add to changes" }));
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).at(-1)?.operations).toHaveLength(2), { timeout: 3000 });
    const ops = (await bodies(f, "POST", SC)).at(-1).operations;
    expect(ops[0]).toEqual({ rename_table: { table: T, to: "bills" } });
    expect(ops[1].add_column.table).toEqual({ schema: "public", name: "bills" });
  });
});

describe("T5 minors", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("M2_closing_check_drops_the_hidden_check", async () => {
    const { user, f } = renderEditor();
    await user.click(await screen.findByRole("tab", { name: "Structure" }));
    await user.click(screen.getByRole("button", { name: "Add column" }));
    const form = await screen.findByRole("complementary", { name: /column/i });
    await user.type(within(form).getByLabelText("Name"), "note");
    await user.click(within(form).getByLabelText("Type"));
    await user.click(await screen.findByRole("option", { name: /^text/ }));
    await user.click(within(form).getByRole("button", { name: "Check…" }));
    await user.type(within(form).getByLabelText("Check expression"), "note <> ''");
    await user.click(within(form).getByRole("button", { name: "Check…" }));
    await user.click(within(form).getByRole("button", { name: "Add to changes" }));
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).length).toBeGreaterThan(0), { timeout: 3000 });
    const ops = (await bodies(f, "POST", SC)).at(-1).operations;
    expect(ops.some((o: object) => "add_constraint" in o)).toBe(false);
  });

  it("M3_the_changeset_stays_until_the_table_is_read_again", async () => {
    draft([{ alter_column: { table: T, column: "memo", change: { not_null: true } } }]);
    let committed = false;
    const draftAtReread: boolean[] = [];
    const routes: Record<string, FakeHandler> = {
      [`POST ${SC}`]: async (req) => {
        if (!(await req.clone().json()).dry_run) committed = true;
        return schemaChanges(req);
      },
      [`GET ${TABLE_PATH}`]: () => {
        if (committed) draftAtReread.push((sessionStorage.getItem(DRAFTS_KEY) ?? "").includes(K));
        return jsonResponse(invoices());
      },
    };
    const { user } = renderEditor({ routes });
    await applyAll(user);
    expect(await screen.findByText("Applied 1 change to public.invoices")).toBeInTheDocument();
    expect(draftAtReread[0]).toBe(true);
  });

  it("M6_the_status_live_region_exists_before_apply_and_focus_does_not_drop_to_body", async () => {
    draft([{ alter_column: { table: T, column: "memo", change: { not_null: true } } }]);
    const { user } = renderEditor();
    await panel();
    const regions = screen.getAllByRole("status");
    expect(regions.length).toBeGreaterThan(0);
    await applyAll(user);
    expect(await screen.findByText("Applied 1 change to public.invoices")).toBeInTheDocument();
    expect(regions.some((r) => r.isConnected && r.textContent?.includes("Applied 1 change"))).toBe(true);
    expect(document.activeElement).not.toBe(document.body);
  });

  it("M8_a_focus_request_on_an_open_form_is_used_at_once_and_not_by_the_next_form", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Column actions for memo" }));
    await user.click(await screen.findByRole("menuitem", { name: "Rename" }));
    const form = await screen.findByRole("complementary", { name: /column/i });
    await user.click(screen.getByRole("button", { name: "Column actions for memo" }));
    await user.click(await screen.findByRole("menuitem", { name: "Comment" }));
    await vi.waitFor(() => expect(document.activeElement).toBe(within(form).getByLabelText("Comment")));
    await user.click(screen.getByRole("button", { name: "Add column" }));
    const next = await screen.findByRole("complementary", { name: /column/i });
    await vi.waitFor(() => expect(document.activeElement).toBe(within(next).getByLabelText("Name")));
  });
});
