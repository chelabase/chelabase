import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse } from "@/test/fake-fetch";
import { B, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { DRAFTS_KEY } from "@/lib/table-editor/store";
import { newEdit, tableFingerprint } from "@/lib/table-editor/changeset";

const SC = `${B}/schema-changes`;
const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const dropMemo = [{ drop_column: { table: T, column: "memo" } }];
const panel = () => screen.findByRole("complementary", { name: "Pending changes" });

describe("changes panel (implementer's tests)", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("has_dependents_dry_run_reruns_with_cascade_once_ticked", async () => {
    sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base: tableFingerprint(invoices()), edits: [newEdit(dropMemo)] } }));
    const deps = { hash: "h1", objects: [{ kind: "view", name: "v_open", schema: "public", description: "view public.v_open" }] };
    // The API's answer to a drop with dependents and no cascade: 409, dry run or not.
    const handler = async (req: Request) => {
      const b = await req.clone().json();
      if (!b.cascade) return jsonResponse({ error: { code: "has_dependents", message: "other objects depend on it", dependents: deps } }, 409);
      if (b.dry_run) return jsonResponse({ dry_run: true, committed: false, would_succeed: true, statements: [{ sql: "ALTER TABLE public.invoices DROP COLUMN memo CASCADE", op_index: 0, executed: true, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null }], dependents: deps });
      return jsonResponse({ dry_run: false, committed: true, statements: [] });
    };
    const { f, user } = renderEditor({ routes: { [`POST ${SC}`]: handler } });
    const p = await panel();
    expect(await within(p).findByText("view public.v_open")).toBeInTheDocument();
    // A dependents answer isn't an error on the edit.
    expect(within(p).queryByText(/^Error:/)).toBeNull();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    expect(apply).toBeDisabled();
    await user.click(within(p).getByRole("checkbox", { name: "Drop dependents too (cascade)" }));
    await vi.waitFor(() => expect(apply).toBeEnabled(), { timeout: 2000 });
    const dryRuns = (await bodies(f, "POST", SC)).filter((b) => b.dry_run);
    expect(dryRuns.at(-1).cascade).toEqual({ dependents_hash: "h1" });
    await user.click(apply);
    expect(await screen.findByRole("status")).toHaveTextContent("Applied 1 change to public.invoices");
  });

  it("review_button_returns_to_the_changes_from_a_column_form", async () => {
    sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base: tableFingerprint(invoices()), edits: [newEdit(dropMemo)] } }));
    const { user } = renderEditor();
    await panel();
    await user.click(await screen.findByRole("button", { name: "Add column" }));
    expect(await screen.findByRole("complementary", { name: "New column" })).toBeInTheDocument();
    expect(screen.queryByRole("complementary", { name: "Pending changes" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Review 1 change" }));
    expect(within(await panel()).getByRole("button", { name: "Drop column memo" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Review 1 change" })).toBeNull();
  });

  it("form_errors_show_on_add_and_nothing_is_staged", async () => {
    const { user } = renderEditor();
    await user.click(await screen.findByRole("button", { name: "Add column" }));
    const form = await screen.findByRole("complementary", { name: "New column" });
    await user.click(within(form).getByRole("button", { name: "Add to changes" }));
    expect(within(form).getByText("Name the column")).toBeInTheDocument();
    expect(within(form).getByText("Pick a type")).toBeInTheDocument();
    expect(screen.queryByRole("complementary", { name: "Pending changes" })).toBeNull();
  });
});
