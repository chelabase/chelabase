import { describe, expect, it } from "vitest";
import { ApiError, type Operation, type Table } from "@chelabase/management-client";
import { fold, newEdit, project, withoutEdit, type Edit } from "./changeset";
import { emptyConstraintForm, validateConstraintForm } from "./constraint-form";
import { mapChangeError } from "./dry-run";
import { invoices } from "./test-tables";

// Final fix round (FA), model-level findings.
const T = { schema: "public", name: "invoices" };
const INT8 = { schema: "pg_catalog", name: "int8" };

describe("T5 M4: deadlock and serialization failures are retryable", () => {
  const edits = [newEdit([{ alter_column: { table: T, column: "memo", change: { not_null: true } } }])];
  it("conflict_with_40P01_or_40001_is_retryable_other_conflicts_are_not", () => {
    expect(mapChangeError(edits, new ApiError(409, "conflict", "deadlock detected", { op_index: 0, sqlstate: "40P01" })).retryable).toBe(true);
    expect(mapChangeError(edits, new ApiError(409, "conflict", "could not serialize", { op_index: 0, sqlstate: "40001" })).retryable).toBe(true);
    expect(mapChangeError(edits, new ApiError(409, "conflict", "duplicate key", { op_index: 0, sqlstate: "23505" })).retryable).toBe(false);
  });
});

describe("E-M5: enum edits in the #types changeset", () => {
  const S = "public";
  const create: Operation = { create_enum: { schema: S, name: "tier", values: ["free", "pro"] } };
  const addValue: Operation = { add_enum_value: { schema: S, name: "tier", value: "team" } };
  const renameValue: Operation = { rename_enum_value: { schema: S, name: "tier", from: "pro", to: "plus" } };
  const other: Operation = { add_enum_value: { schema: S, name: "invoice_status", value: "refunded" } };
  it("removing_create_enum_drops_the_later_ops_on_that_enum", () => {
    const c = newEdit([create]);
    const a = newEdit([addValue]);
    const r = newEdit([renameValue]);
    const o = newEdit([other]);
    expect(withoutEdit(null, [c, a, r, o], c.id).map((e) => e.id)).toEqual([o.id]);
  });
  it("removing_an_add_value_drops_a_later_rename_of_that_value", () => {
    const c = newEdit([create]);
    const a = newEdit([addValue]);
    const r = newEdit([{ rename_enum_value: { schema: S, name: "tier", from: "team", to: "group" } }]);
    expect(withoutEdit(null, [c, a, r], a.id).map((e) => e.id)).toEqual([c.id]);
  });
  it("removing_a_rename_enum_drops_later_ops_that_name_the_new_name", () => {
    const rn = newEdit([{ rename_enum: { schema: S, name: "invoice_status", to: "status" } }]);
    const a = newEdit([{ add_enum_value: { schema: S, name: "status", value: "refunded" } }]);
    expect(withoutEdit(null, [rn, a], rn.id)).toEqual([]);
  });
});

describe("T1 note: generated constraint names follow Postgres's 63-byte rule", () => {
  const longTable = "t".repeat(60);
  const longCol = "c".repeat(20);
  const ref = { schema: "public", name: longTable };
  const base = (over: Partial<Table> = {}) => invoices({ name: longTable, ...over } as Partial<Table>);
  const steps = (b: Table, ...ops: Operation[][]): Edit[] => ops.reduce<Edit[]>((acc, o) => fold(b, acc, newEdit(o)), []);
  it("truncates_the_longer_part_first_like_makeObjectName", () => {
    const b = base();
    const edits = steps(b, [{ add_column: { table: ref, column: { name: longCol, data_type: INT8 } } }, { add_constraint: { table: ref, constraint: { kind: { unique: { columns: [longCol] } } } } }]);
    const added = project(b, edits).constraints.filter((c) => c.mark === "added");
    // availchars = 63 - "_" - "_key" = 58; name1 shrinks to 38 so 38 + 20 = 58.
    expect(added.map((c) => c.name)).toEqual([`${"t".repeat(38)}_${longCol}_key`]);
  });
  it("a_taken_truncated_name_gets_a_number_inside_the_label_and_is_truncated_again", () => {
    const b = base();
    const first = [{ add_column: { table: ref, column: { name: longCol, data_type: INT8 } } }, { add_constraint: { table: ref, constraint: { kind: { unique: { columns: [longCol] } } } } }] as Operation[];
    const second = [{ add_constraint: { table: ref, constraint: { kind: { unique: { columns: [longCol] } } } } }] as Operation[];
    const names = project(b, steps(b, first, second)).constraints.filter((c) => c.mark === "added").map((c) => c.name);
    // Label "key1": availchars = 63 - 1 - 5 = 57; name1 shrinks to 37.
    expect(names).toEqual([`${"t".repeat(38)}_${longCol}_key`, `${"t".repeat(37)}_${longCol}_key1`]);
  });
});

describe("E-M12: typed constraint names", () => {
  const table = project(invoices(), []);
  const relations = new Set(["invoices", "customers", "invoices_pkey", "v_open"]);
  it("an_index_backed_constraint_name_cannot_reuse_a_relation_name_in_the_schema", () => {
    const pk = { ...emptyConstraintForm(), kind: "unique" as const, columns: ["memo"], name: "customers" };
    expect(validateConstraintForm(pk, table, relations)).toEqual([{ field: "name", message: "customers is already the name of a table, view or index in public" }]);
  });
  it("a_check_or_fk_name_may_match_a_relation_name", () => {
    const check = { ...emptyConstraintForm(), kind: "check" as const, expression: "memo <> ''", name: "customers" };
    expect(validateConstraintForm(check, table, relations)).toEqual([]);
  });
  it("relations_default_to_none", () => {
    expect(validateConstraintForm({ ...emptyConstraintForm(), columns: ["memo"], name: "customers" }, table)).toEqual([]);
  });
});
