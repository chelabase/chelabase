import { describe, expect, it } from "vitest";
import { project } from "./changeset";
import { constraintFormOps, emptyConstraintForm, validateConstraintForm } from "./constraint-form";
import { invoices } from "./test-tables";

const table = project(invoices(), []);
const ref = { schema: "public", name: "invoices" };

describe("constraint form", () => {
  it("needs columns, except for a check", () => {
    expect(validateConstraintForm(emptyConstraintForm(), table).map((e) => e.field)).toEqual(["columns"]);
    expect(validateConstraintForm({ ...emptyConstraintForm(), kind: "check", expression: "amount_cents > 0" }, table)).toEqual([]);
    expect(validateConstraintForm({ ...emptyConstraintForm(), kind: "check" }, table).map((e) => e.field)).toEqual(["expression"]);
  });

  it("refuses a second primary key, a taken name and a foreign key that doesn't line up", () => {
    expect(validateConstraintForm({ ...emptyConstraintForm(), kind: "primary_key", columns: ["memo"] }, table)[0].message).toBe("invoices already has a primary key");
    expect(validateConstraintForm({ ...emptyConstraintForm(), columns: ["memo"], name: "invoices_pkey" }, table)[0].message).toBe("invoices_pkey is already a constraint");
    const fk = { ...emptyConstraintForm(), kind: "foreign_key" as const, columns: ["customer_id", "memo"], refTable: { schema: "public", name: "customers" }, refColumns: ["id"] };
    expect(validateConstraintForm(fk, table)[0].message).toBe("Pick 2 referenced columns");
  });

  it("builds the operation; NOT VALID only where Postgres allows it", () => {
    const fk = { ...emptyConstraintForm(), kind: "foreign_key" as const, columns: ["customer_id"], refTable: { schema: "public", name: "customers" }, refColumns: ["id"], onDelete: "cascade" as const, notValid: true, name: " fk1 " };
    expect(constraintFormOps(fk, ref)).toEqual([
      { add_constraint: { table: ref, constraint: { kind: { foreign_key: { columns: ["customer_id"], references: { schema: "public", name: "customers" }, referenced_columns: ["id"], on_delete: "cascade" } }, name: "fk1", not_valid: true } } },
    ]);
    expect(constraintFormOps({ ...emptyConstraintForm(), columns: ["memo"], notValid: true }, ref)).toEqual([{ add_constraint: { table: ref, constraint: { kind: { unique: { columns: ["memo"] } } } } }]);
  });
});
