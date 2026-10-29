import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { B, T as TABLE_PATH, bodies, invoices, renderEditor, schemaChanges, stubLayout } from "@/test/table-editor-fixtures";
import { DRAFTS_KEY } from "@/lib/table-editor/store";
import { newEdit, tableFingerprint } from "@/lib/table-editor/changeset";

// Fix round 1 of T5 (review I1–I5). The invariant: Apply posts only after a fresh re-read shows the
// table unchanged, with exactly the operations, role and cascade of the latest settled dry run, and
// only after an acknowledgement given for that same dry run; nothing can be staged while Apply runs.
const SC = `${B}/schema-changes`;
const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const draft = (...ops: Parameters<typeof newEdit>[0][]) =>
  sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({ [K]: { key: K, base: tableFingerprint(invoices()), edits: ops.map((o) => newEdit(o)) } }));
const dropMemo = [{ drop_column: { table: T, column: "memo" } }];
const alterAmount = [{ alter_column: { table: T, column: "amount_cents", change: { data_type: { schema: "pg_catalog", name: "text" } } } }];
const panel = () => screen.findByRole("complementary", { name: "Pending changes" });
const ok = (over: Record<string, unknown> = {}) =>
  jsonResponse({ dry_run: true, committed: false, would_succeed: true, statements: [{ sql: "SQL 0", op_index: 0, executed: true, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null }], ...over });

/** Every real run equals the dry run that preceded it, with only `dry_run` flipped. */
async function expectRealMatchesLastDry(f: Parameters<typeof bodies>[0]) {
  const all = await bodies(f, "POST", SC);
  all.forEach((b, i) => {
    if (b.dry_run) return;
    const prev = all.slice(0, i).reverse().find((x) => x.dry_run);
    expect(prev, "a real run without a dry run before it").toBeTruthy();
    expect(b).toEqual({ ...prev, dry_run: false });
  });
}

describe("T5 fix round", () => {
  beforeEach(() => { localStorage.clear(); sessionStorage.clear(); stubLayout(); });

  it("I1_has_dependents_with_no_objects_asks_for_cascade_and_applies", async () => {
    draft(dropMemo);
    const sc: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (!b.cascade) return jsonResponse({ error: { code: "has_dependents", message: "drops depend on each other", op_index: 0, dependents: { hash: "h0", objects: [] } } }, 409);
      return b.dry_run ? ok() : jsonResponse({ dry_run: false, committed: true, statements: [] });
    };
    const { f, user } = renderEditor({ routes: { [`POST ${SC}`]: sc } });
    const p = await panel();
    expect(await within(p).findByText("Drops in this change depend on each other")).toBeInTheDocument();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    expect(apply).toBeDisabled();
    await user.click(within(p).getByRole("checkbox", { name: "Drop dependents too (cascade)" }));
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    expect(await screen.findByRole("status")).toHaveTextContent("Applied 1 change to public.invoices");
    expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)[0].cascade).toEqual({ dependents_hash: "h0" });
    await expectRealMatchesLastDry(f);
  });

  it("I2_nothing_can_be_staged_while_apply_runs", async () => {
    draft(alterAmount);
    let release: () => void = () => {};
    const gate = new Promise<void>((r) => { release = r; });
    const sc: FakeHandler = async (req) => {
      const b = await req.clone().json();
      if (b.dry_run) return schemaChanges(req);
      await gate;
      return jsonResponse({ dry_run: false, committed: true, statements: [] });
    };
    const { f, user } = renderEditor({ routes: { [`POST ${SC}`]: sc } });
    const p = await panel();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled());
    await user.click(apply);
    await vi.waitFor(async () => expect((await bodies(f, "POST", SC)).some((b) => !b.dry_run)).toBe(true));
    expect(screen.getByRole("button", { name: "Add column" })).toBeDisabled();
    // The column menu keeps its sort items only.
    await user.click(screen.getByRole("button", { name: "Column actions for memo" }));
    await screen.findByRole("menu");
    expect(screen.getAllByRole("menuitem").map((m) => m.textContent)).toEqual(["Sort ascending", "Sort descending"]);
    await user.keyboard("{Escape}");
    release();
    expect(await screen.findByRole("status")).toHaveTextContent("Applied 1 change to public.invoices");
    expect(screen.getByRole("button", { name: "Add column" })).toBeEnabled();
  });

  it("I3_the_acknowledgement_belongs_to_one_dry_run", async () => {
    draft(alterAmount);
    const sc: FakeHandler = async () =>
      jsonResponse({ dry_run: true, committed: false, would_succeed: null, statements: [{ sql: "SQL 0", op_index: 0, executed: false, lock: "access_exclusive", notices: [], rewrites_table: true, skipped: "large_table_rewrite" }] });
    const { user } = renderEditor({ routes: { [`POST ${SC}`]: sc } });
    const p = await panel();
    const ack = await within(p).findByRole("checkbox", { name: "I've read the SQL" });
    await user.click(ack);
    expect(within(p).getByRole("button", { name: "Apply 1 change" })).toBeEnabled();
    await user.click(screen.getByLabelText("Act as"));
    await user.click(await screen.findByRole("option", { name: "billing_admin" }));
    await vi.waitFor(() => expect(within(p).getByRole("checkbox", { name: "I've read the SQL" })).not.toBeChecked());
    expect(within(p).getByRole("button", { name: "Apply 1 change" })).toBeDisabled();
  });

  it("I4_apply_rereads_the_table_and_stops_when_it_changed", async () => {
    draft(alterAmount);
    let changed = false;
    const { f, user } = renderEditor({ routes: { [`GET ${TABLE_PATH}`]: () => jsonResponse(changed ? invoices({ comment: "renamed elsewhere" }) : invoices()) } });
    const p = await panel();
    const apply = within(p).getByRole("button", { name: "Apply 1 change" });
    await vi.waitFor(() => expect(apply).toBeEnabled());
    changed = true;
    await user.click(apply);
    expect(await within(p).findByText("This table changed since you started")).toBeInTheDocument();
    expect((await bodies(f, "POST", SC)).filter((b) => !b.dry_run)).toHaveLength(0);
  });

  it("I5_a_pending_drop_with_dependents_doesnt_break_the_column_preview", async () => {
    draft(dropMemo);
    const sc: FakeHandler = async (req) => {
      const b = await req.clone().json();
      const drops = (b.operations ?? []).some((o: Record<string, unknown>) => "drop_column" in o);
      if (drops && !b.cascade) return jsonResponse({ error: { code: "has_dependents", message: "v_open depends on memo", op_index: 0, dependents: { hash: "h1", objects: [{ kind: "view", name: "v_open", schema: "public", description: "view public.v_open" }] } } }, 409);
      return schemaChanges(req);
    };
    const { user } = renderEditor({ routes: { [`POST ${SC}`]: sc } });
    await panel();
    await user.click(await screen.findByRole("button", { name: "Add column" }));
    const form = await screen.findByRole("complementary", { name: "New column" });
    await user.type(within(form).getByLabelText("Name"), "note2");
    await user.click(within(form).getByLabelText("Type"));
    await user.click(await screen.findByRole("option", { name: "text" }));
    const preview = within(form).getByRole("region", { name: "SQL preview" });
    await vi.waitFor(() => expect(within(preview).queryAllByText(/^SQL \d+$/).length).toBeGreaterThan(0), { timeout: 3000 });
    expect(within(form).queryByText(/earlier pending change fails/i)).toBeNull();
  });
});
