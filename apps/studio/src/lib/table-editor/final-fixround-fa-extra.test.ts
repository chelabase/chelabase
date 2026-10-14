import { describe, expect, it } from "vitest";
import type { Operation } from "@chelabase/management-client";
import { fold, newEdit, opsOf, withoutEdit, type Edit } from "./changeset";
import { invoices } from "./test-tables";

// FA, implementer's own: table settings after a rename, and enum renames in the #types changeset.
const T = { schema: "public", name: "invoices" };

describe("table settings after a pending rename", () => {
  it("rls_set_again_after_a_rename_stays_after_it", () => {
    const steps: Operation[][] = [[{ set_rls: { table: T, enabled: true, forced: false } }], [{ rename_table: { table: T, to: "bills" } }], [{ set_rls: { table: T, enabled: true, forced: true } }]];
    const edits = steps.reduce<Edit[]>((acc, o) => fold(invoices(), acc, newEdit(o)), []);
    expect(opsOf(edits)).toEqual([
      { rename_table: { table: T, to: "bills" } },
      { set_rls: { table: { schema: "public", name: "bills" }, enabled: true, forced: true } },
    ]);
  });
});

describe("enum renames (E-M5)", () => {
  it("removing_a_value_rename_drops_a_later_rename_of_its_new_name_only", () => {
    const S = "public";
    const a = newEdit([{ rename_enum_value: { schema: S, name: "invoice_status", from: "void", to: "cancelled" } }]);
    const b = newEdit([{ rename_enum_value: { schema: S, name: "invoice_status", from: "cancelled", to: "voided" } }]);
    const c = newEdit([{ add_enum_value: { schema: S, name: "invoice_status", value: "refunded" } }]);
    expect(withoutEdit(null, [a, b, c], a.id).map((e) => e.id)).toEqual([c.id]);
  });
});
