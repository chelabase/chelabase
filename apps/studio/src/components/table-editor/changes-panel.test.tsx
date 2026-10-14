import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, sequence } from "@/test/fake-fetch";
import { B, bodies, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";
import { DRAFTS_KEY } from "@/lib/table-editor/store";
import { newEdit, tableFingerprint } from "@/lib/table-editor/changeset";

const SC = `${B}/schema-changes`;
const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const draft = (base: string, ...ops: Parameters<typeof newEdit>[0][]) =>
  sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base, edits: ops.map((o) => newEdit(o)) } }));
const dropMemo = [{ drop_column: { table: T, column: "memo" } }];
const alterAmount = [{ alter_column: { table: T, column: "amount_cents", change: { data_type: { schema: "pg_catalog", name: "text" } } } }];
const okDry = (over: Record<string, unknown> = {}) => jsonResponse({ dry_run: true, committed: false, would_succeed: true, statements: [{ sql: "SQL 0", op_index: 0, executed: true, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null }], ...over });
const panel = () => screen.findByRole("complementary", { name: "Pending changes" });

describe("changes panel", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("drafts_survive_a_reload", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    renderEditor();
    expect(within(await panel()).getByRole("button", { name: "Drop column memo" })).toBeInTheDocument();
  });

  it("an_error_lands_on_its_edit_and_blocks_apply", async () => {
    draft(tableFingerprint(invoices()), dropMemo, alterAmount);
    const err = () => jsonResponse({ error: { code: "schema_change_failed", message: "cannot cast bigint", op_index: 1, sqlstate: "42804", detail: null, hint: "use USING" } }, 400);
    renderEditor({ routes: { [`POST ${SC}`]: err } });
    const p = await panel();
    expect(await within(p).findByText("Error: cannot cast bigint")).toBeInTheDocument();
    expect(within(p).getByText("use USING")).toBeInTheDocument();
    expect(within(p).getByRole("button", { name: "Apply 2 changes" })).toBeDisabled();
  });

  it("not_simulated_needs_an_acknowledgement", async () => {
    draft(tableFingerprint(invoices()), alterAmount);
    const dry = () => jsonResponse({ dry_run: true, committed: false, would_succeed: null, statements: [{ sql: "SQL 0", op_index: 0, executed: false, lock: "access_exclusive", notices: [], rewrites_table: true, skipped: "large_table_rewrite" }] });
    const { user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    const p = await panel();
    expect(await within(p).findByText("Rewrites public.invoices (12 MB): reads and writes wait while it runs")).toBeInTheDocument();
    expect(within(p).getByText("Not simulated: rewrites a large table")).toBeInTheDocument();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    expect(apply).toBeDisabled();
    await user.click(within(p).getByRole("checkbox", { name: "I've read the SQL" }));
    expect(apply).toBeEnabled();
  });

  it("dependents_changed_shows_the_new_list_and_unticks_cascade", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    const dry = sequence(
      () => okDry({ dependents: { hash: "h1", objects: [{ kind: "view", name: "v_open", schema: "public", description: "view public.v_open" }] } }),
      () => jsonResponse({ error: { code: "dependents_changed", message: "changed", dependents: { hash: "h2", objects: [{ kind: "view", name: "v_paid", schema: "public", description: "view public.v_paid" }] } } }, 409),
    );
    const { user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    const p = await panel();
    await user.click(await within(p).findByRole("checkbox", { name: "Drop dependents too (cascade)" }));
    await user.click(within(p).getByRole("button", { name: "Apply 1 change" }));
    expect(await within(p).findByText("view public.v_paid")).toBeInTheDocument();
    expect(within(p).getByRole("checkbox", { name: "Drop dependents too (cascade)" })).not.toBeChecked();
  });

  it("lock_timeout_offers_try_again", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    const dry = sequence(() => okDry(), () => jsonResponse({ error: { code: "lock_timeout", message: "lock timeout", op_index: 0 } }, 409), () => jsonResponse({ dry_run: false, committed: true, statements: [] }));
    const { f, user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    const p = await panel();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    await user.click(await within(p).findByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Applied 1 change to public.invoices");
    expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)).toHaveLength(2);
  });

  it("branch_protected_explains", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    const dry = sequence(() => okDry(), () => jsonResponse({ error: { code: "branch_protected", message: "branch is protected: schema changes need Admin" } }, 403));
    const { user } = renderEditor({ routes: { [`POST ${SC}`]: dry } });
    const p = await panel();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    expect(await within(p).findByText("branch is protected: schema changes need Admin")).toBeInTheDocument();
  });

  it("a_changed_table_blocks_apply_until_reviewed", async () => {
    draft(tableFingerprint(invoices({ comment: "older" })), dropMemo);
    const { user } = renderEditor();
    const p = await panel();
    expect(await within(p).findByText("This table changed since you started")).toBeInTheDocument();
    expect(within(p).getByRole("button", { name: "Apply 1 change" })).toBeDisabled();
    await user.click(within(p).getByRole("button", { name: "Review again" }));
    await vi.waitFor(() => expect(within(p).getByRole("button", { name: "Apply 1 change" })).toBeEnabled());
  });

  it("remove_and_discard", async () => {
    draft(tableFingerprint(invoices()), dropMemo, alterAmount);
    const { user } = renderEditor();
    const p = await panel();
    await user.click(within(p).getByRole("button", { name: "Remove Drop column memo" }));
    expect(within(p).getByRole("heading", { name: "Pending changes · 1" })).toBeInTheDocument();
    await user.click(within(p).getByRole("button", { name: "Discard" }));
    await user.click(within(await screen.findByRole("dialog", { name: "Discard 1 change?" })).getByRole("button", { name: "Discard" }));
    expect(screen.queryByRole("complementary", { name: "Pending changes" })).toBeNull();
    expect(JSON.parse(sessionStorage.getItem(DRAFTS_KEY)!)).toEqual({});
  });

  it("collapse_keeps_pending_changes", async () => {
    draft(tableFingerprint(invoices()), dropMemo);
    const { user } = renderEditor();
    await user.click(within(await panel()).getByRole("button", { name: "Collapse panel" }));
    expect(screen.getByRole("button", { name: "Expand panel" })).toHaveAccessibleDescription(/Pending changes · 1/);
    await user.click(screen.getByRole("button", { name: "Expand panel" }));
    expect(within(await panel()).getByRole("button", { name: "Drop column memo" })).toBeInTheDocument();
  });
});
