import { beforeEach, describe, expect, it } from "vitest";
import { newEdit, tableFingerprint } from "./changeset";
import { DRAFTS_KEY, createTableEditorStore } from "./store";
import { invoices } from "./test-tables";

const T = { schema: "public", name: "invoices" };
const K = "d1/b1/app/public.invoices";
const add = newEdit([{ add_column: { table: T, column: { name: "d", data_type: { schema: "pg_catalog", name: "int8" } } } }]);
const drop = newEdit([{ drop_column: { table: T, column: "d" } }]);

describe("table editor store", () => {
  beforeEach(() => sessionStorage.clear());
  it("adds_folds_and_drops_empty_changesets", () => {
    const s = createTableEditorStore(sessionStorage);
    s.getState().addEdit(K, invoices(), add);
    expect(s.getState().changesets[K]).toMatchObject({ key: K, base: tableFingerprint(invoices()) });
    expect(s.getState().changesets[K].edits).toHaveLength(1);
    s.getState().addEdit(K, invoices(), drop);
    expect(s.getState().changesets[K]).toBeUndefined();
  });
  it("persists_drafts_to_session_storage_and_loads_them", () => {
    const s = createTableEditorStore(sessionStorage);
    s.getState().addEdit(K, invoices(), add);
    expect(JSON.parse(sessionStorage.getItem(DRAFTS_KEY)!)[K].edits[0].label).toBe("Add column d int8");
    const again = createTableEditorStore(sessionStorage);
    expect(again.getState().changesets[K].edits[0].ops).toEqual(add.ops);
    again.getState().discard(K);
    expect(JSON.parse(sessionStorage.getItem(DRAFTS_KEY)!)).toEqual({});
  });
  it("remove_edit_and_rebase", () => {
    const s = createTableEditorStore(null);
    s.getState().addEdit(K, invoices(), add);
    s.getState().rebase(K, invoices({ comment: "new" }));
    expect(s.getState().changesets[K].base).toBe(tableFingerprint(invoices({ comment: "new" })));
    s.getState().removeEdit(K, s.getState().changesets[K].edits[0].id);
    expect(s.getState().changesets[K]).toBeUndefined();
  });
  it("broken_storage_is_ignored", () => {
    const broken = { getItem: () => { throw new Error("x"); }, setItem: () => { throw new Error("x"); }, removeItem: () => {} } as unknown as Storage;
    const s = createTableEditorStore(broken);
    expect(() => s.getState().addEdit(K, invoices(), add)).not.toThrow();
    expect(s.getState().changesets[K].edits).toHaveLength(1);
  });
  it("panel_act_as_and_selection", () => {
    const s = createTableEditorStore(null);
    s.getState().setCollapsed(true);
    s.getState().openPanel("column", { column: "memo" });
    expect(s.getState().panel).toEqual({ kind: "column", collapsed: false, column: "memo", row: null });
    s.getState().setActAs("d1/b1", "billing_admin");
    expect(s.getState().actAs["d1/b1"]).toBe("billing_admin");
    s.getState().select({ type: "view", view: "v" });
    s.getState().select({ type: "toggle", key: "[1]" });
    expect(s.getState().selection).toEqual({ view: "v", selection: { kind: "rows", keys: ["[1]"] } });
  });
});
