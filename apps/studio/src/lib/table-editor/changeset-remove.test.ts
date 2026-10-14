import { describe, expect, it } from "vitest";
import type { Operation } from "@chelabase/management-client";
import { newEdit, withoutEdit } from "./changeset";
import { createTableEditorStore } from "./store";
import { invoices } from "./test-tables";

// T2 review M5: removing an edit also removes the later edits that depend on it.
const T = { schema: "public", name: "invoices" };
const INT8 = { schema: "pg_catalog", name: "int8" };
const op = {
  add: (name: string): Operation => ({ add_column: { table: T, column: { name, data_type: INT8 } } }),
  unique: (c: string): Operation => ({ add_constraint: { table: T, constraint: { kind: { unique: { columns: [c] } } } } }),
  notNull: (c: string): Operation => ({ alter_column: { table: T, column: c, change: { not_null: true } } }),
  rename: (c: string, to: string): Operation => ({ rename_column: { table: T, column: c, to } }),
};

describe("withoutEdit", () => {
  it("drops_later_edits_that_need_the_removed_one", () => {
    const e1 = newEdit([op.add("d")]);
    const e2 = newEdit([op.unique("d")]);
    const e3 = newEdit([op.notNull("memo")]);
    expect(withoutEdit(invoices(), [e1, e2, e3], e1.id).map((x) => x.id)).toEqual([e3.id]);
    expect(withoutEdit(invoices(), [e1, e2, e3], e2.id).map((x) => x.id)).toEqual([e1.id, e3.id]);
  });
  it("an_alter_on_a_renamed_name_goes_with_the_rename", () => {
    const r = newEdit([op.rename("memo", "note")]);
    const a = newEdit([op.notNull("note")]);
    expect(withoutEdit(invoices(), [r, a], r.id)).toEqual([]);
  });
  it("unknown_id_changes_nothing", () => {
    const e1 = newEdit([op.notNull("memo")]);
    expect(withoutEdit(invoices(), [e1], "nope")).toEqual([e1]);
  });
  it("store_remove_edit_with_a_base_removes_dependents", () => {
    const s = createTableEditorStore(null);
    const K = "d1/b1/app/public.invoices";
    const e1 = newEdit([op.add("d")]);
    const e2 = newEdit([op.unique("d")]);
    s.getState().addEdit(K, invoices(), e1);
    s.getState().addEdit(K, invoices(), e2);
    const first = s.getState().changesets[K].edits[0];
    s.getState().removeEdit(K, first.id, invoices());
    expect(s.getState().changesets[K]).toBeUndefined();
  });
});
