import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Table } from "@chelabase/management-client";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, FULL, OBJECTS, PG_DATABASES, ROWS, T as TABLE_PATH, bodies, invoices, meAs, obj, pgDb, renderEditor, schemaChanges, stubLayout } from "@/test/table-editor-fixtures";
import { DRAFTS_KEY } from "@/lib/table-editor/store";
import { newEdit, tableFingerprint } from "@/lib/table-editor/changeset";
import { editorRouteParams } from "@/lib/table-editor/context";

// End of step 6: the browser pass's findings (browser-pass.md) I1, I2, M1 (user R15), M2, M3, M5, M6.
const Q = `${B}/console/query`;
const SC = `${B}/schema-changes`;
const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const draft = (...ops: Parameters<typeof newEdit>[0][]) =>
  sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base: tableFingerprint(invoices()), edits: ops.map((o) => newEdit(o)) } }));

describe("browser pass fixes", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("I1_the_column_form_preview_addresses_the_table_as_it_is_after_a_pending_rename", async () => {
    draft([{ rename_table: { table: T, to: "bills" } }]);
    const { f, user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Column actions for memo" }));
    await user.click(await screen.findByRole("menuitem", { name: "Comment" }));
    const form = await screen.findByRole("complementary", { name: /column/i });
    await user.type(within(form).getByLabelText("Comment"), "note");
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).some((b) => b.operations.some((o: object) => "alter_column" in o))).toBe(true), { timeout: 4000 });
    for (const b of await bodies(f, "POST", SC)) {
      for (const o of b.operations as Record<string, { table: unknown }>[]) {
        if ("alter_column" in o) expect(o.alter_column.table).toEqual({ schema: "public", name: "bills" });
      }
    }
  });

  it("I2_route_params_name_every_level_so_missing_ones_are_cleared", () => {
    expect(editorRouteParams({ projectId: "p1", databaseId: "d1", branchId: "b2" })).toEqual({
      projectId: "p1", databaseId: "d1", branchId: "b2", pgDatabase: undefined, schema: undefined, table: undefined,
    });
    expect(Object.keys(editorRouteParams({}))).toEqual(["projectId", "databaseId", "branchId", "pgDatabase", "schema", "table"]);
  });

  it("R18_admins_dont_act_as_anon_or_authenticated_without_a_named_grant", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByLabelText("Act as"));
    expect(await screen.findByRole("option", { name: "reporter" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "anon" })).toBeNull();
    expect(screen.queryByRole("option", { name: "authenticated" })).toBeNull();
  });

  it("R15_a_developer_acts_as_anon_only_when_the_grant_includes_it", async () => {
    const { user } = renderEditor({ me: meAs("developer", ["billing_admin", "anon"]) });
    await user.click(await screen.findByLabelText("Act as"));
    expect(await screen.findByRole("option", { name: "anon" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "authenticated" })).toBeNull();
  });

  it("R15_acting_as_anon_reads_rows_as_anon", async () => {
    const { f, user } = renderEditor({ me: meAs("developer", ["billing_admin", "anon"]) });
    await user.click(await screen.findByLabelText("Act as"));
    await user.click(await screen.findByRole("option", { name: "anon" }));
    await vi.waitFor(async () => expect((await bodies(f, "POST", Q)).some((b) => b.role === "anon")).toBe(true));
  });

  it("M2_the_system_postgres_database_isnt_offered", async () => {
    const system = { ...pgDb("postgres", false, "off"), origin: "system" };
    const { user } = renderEditor({ routes: { [`GET ${B}/databases`]: () => jsonResponse([...PG_DATABASES, system]) } });
    await user.click(await screen.findByLabelText("Postgres db"));
    expect(await screen.findByRole("option", { name: /^app/ })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: /^postgres/ })).toBeNull();
  });

  it("M3_a_filtered_delete_names_the_count", async () => {
    const answer: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.tree.op === "count") return jsonResponse({ count: 2 });
      if (b.tree.op === "delete") return jsonResponse([]);
      return jsonResponse(b.tree.where ? [ROWS[2], ROWS[0]] : ROWS);
    };
    const { user } = renderEditor({ routes: { [`POST ${Q}`]: answer } });
    await user.click(await screen.findByRole("button", { name: "Filter" }));
    await user.click(screen.getByLabelText("Filter column"));
    await user.click(await screen.findByRole("option", { name: "status" }));
    await user.click(screen.getByLabelText("Filter operator"));
    await user.click(await screen.findByRole("option", { name: "neq" }));
    await user.type(screen.getByLabelText("Filter value"), "open");
    await user.click(screen.getByRole("button", { name: "Apply filter" }));
    await screen.findByRole("gridcell", { name: "8900" });
    await user.click(screen.getByRole("checkbox", { name: "Select all rows on this page" }));
    await user.click(await screen.findByRole("button", { name: "Select all 2 matching rows" }));
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete 2 matching rows?" });
    expect(within(dialog).getByRole("button", { name: "Delete 2 matching rows" })).toBeInTheDocument();
  });

  it("M5_the_types_view_shows_the_applied_line", async () => {
    const { user } = renderEditor({ params: { ...FULL, table: undefined } });
    const list = await screen.findByRole("navigation", { name: "Tables" });
    await user.click(within(list).getByRole("button", { name: "Types" }));
    await user.click(await screen.findByRole("button", { name: "New enum" }));
    const d = await screen.findByRole("dialog", { name: "New enum" });
    await user.type(within(d).getByLabelText("Enum name"), "tier");
    await user.type(within(d).getByLabelText("Values"), "free");
    await user.click(within(d).getByRole("button", { name: "Add to changes" }));
    const p = await screen.findByRole("complementary", { name: "Pending changes" });
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    expect(await screen.findByText(/^Applied 1 change/)).toBeInTheDocument();
  });

  it("M6_no_rows_query_for_the_old_name_after_a_rename_is_applied", async () => {
    draft([{ rename_table: { table: T, to: "bills" } }]);
    let committed = false;
    const staleAfterCommit: unknown[] = [];
    const routes: Record<string, FakeHandler> = {
      [`POST ${SC}`]: async (req) => {
        if (!(await req.clone().json()).dry_run) committed = true;
        return schemaChanges(req);
      },
      [`POST ${Q}`]: async (req) => {
        const b = await req.clone().json();
        if (committed && (b.tree.from === "public.invoices" || b.tree.table === "public.invoices")) staleAfterCommit.push(b.tree);
        return jsonResponse(ROWS);
      },
      [`GET ${B}/objects`]: () => jsonResponse({ objects: committed ? [...OBJECTS.filter((o) => o.name !== "invoices"), obj("public", "bills")] : OBJECTS }),
      [`GET ${TABLE_PATH}`]: () => (committed ? jsonResponse({ error: { code: "not_found", message: "no such table" } }, 404) : jsonResponse(invoices())),
      [`GET ${B}/tables/public/bills`]: () => jsonResponse(invoices({ name: "bills" } as Partial<Table>)),
    };
    const { f, user, navigate } = renderEditor({ routes });
    const p = await screen.findByRole("complementary", { name: "Pending changes" });
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).some((b) => !b.dry_run)).toBe(true));
    await vi.waitFor(() => expect(navigate).toHaveBeenLastCalledWith({ ...FULL, table: "bills" }));
    expect(staleAfterCommit).toEqual([]);
  });
});
