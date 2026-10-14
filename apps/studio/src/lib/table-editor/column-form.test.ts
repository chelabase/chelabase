import { describe, expect, it } from "vitest";
import type { TypeEntry } from "@chelabase/management-client";
import { newEdit, project } from "./changeset";
import { columnFormOps, emptyColumnForm, formFromColumn, sqlLiteral, validateColumnForm, type ColumnForm } from "./column-form";
import { CATALOG, invoices } from "./test-tables";

const catalog = CATALOG as unknown as TypeEntry[];
const T = { schema: "public", name: "invoices" };
const p = project(invoices(), []);
const form = (over: Partial<ColumnForm>): ColumnForm => ({ ...emptyColumnForm(), ...over });
const col = (name: string) => p.columns.find((c) => c.name === name)!;

describe("column form", () => {
  it("sql_literal_doubles_quotes", () => {
    expect(sqlLiteral("it's")).toBe("'it''s'");
    expect(sqlLiteral("0")).toBe("'0'");
  });
  it("validation_messages", () => {
    const v = (f: Partial<ColumnForm>, editing: string | null = null) => validateColumnForm(form(f), { table: p, editing, catalog }).map((x) => [x.field, x.message]);
    expect(v({})).toEqual([["name", "Name the column"], ["type", "Pick a type"]]);
    expect(v({ name: "x".repeat(64), type: { schema: "pg_catalog", name: "int8" } })).toEqual([["name", "Postgres names are at most 63 bytes"]]);
    expect(v({ name: "é".repeat(32), type: { schema: "pg_catalog", name: "int8" } })).toEqual([["name", "Postgres names are at most 63 bytes"]]);
    expect(v({ name: "memo", type: { schema: "pg_catalog", name: "text" } })).toEqual([["name", "memo is already a column"]]);
    expect(v({ name: "memo", type: { schema: "pg_catalog", name: "text" } }, "memo")).toEqual([]);
    expect(v({ name: "d", type: { schema: "pg_catalog", name: "int8" }, defaultKind: "expression", defaultText: "" })).toEqual([["defaultText", "Write the default expression"]]);
    expect(v({ name: "d", type: { schema: "pg_catalog", name: "int8" }, references: { schema: "public", table: "", column: "", onDelete: null } })).toEqual([["references", "Pick the referenced table and column"]]);
  });
  it("new_column_emits_add_then_constraints_in_order", () => {
    const ops = columnFormOps(
      form({ name: "code", type: { schema: "pg_catalog", name: "text" }, defaultKind: "value", defaultText: "it's", notNull: true, unique: true, check: "length(code) > 2", references: { schema: "public", table: "customers", column: "code", onDelete: "cascade" } }),
      { table: T, editing: null, projected: p },
    );
    expect(ops).toEqual([
      { add_column: { table: T, column: { name: "code", data_type: { schema: "pg_catalog", name: "text", array: false }, not_null: true, default: "'it''s'", comment: null } } },
      { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["code"] } } } } },
      { add_constraint: { table: T, constraint: { kind: { foreign_key: { columns: ["code"], references: { schema: "public", name: "customers" }, referenced_columns: ["code"], on_delete: "cascade" } } } } },
      { add_constraint: { table: T, constraint: { kind: { check: { expression: "length(code) > 2" } } } } },
    ]);
  });
  it("existing_column_emits_only_changed_fields_then_rename", () => {
    const f = formFromColumn(col("memo"), p, catalog);
    expect(f).toMatchObject({ name: "memo", type: { schema: "pg_catalog", name: "text" }, array: false, defaultKind: "none", notNull: false, unique: false, primaryKey: false });
    expect(columnFormOps({ ...f, notNull: true, name: "note", comment: "free text" }, { table: T, editing: col("memo"), projected: p })).toEqual([
      { alter_column: { table: T, column: "memo", change: { not_null: true, comment: { set: "free text" } } } },
      { rename_column: { table: T, column: "memo", to: "note" } },
    ]);
    expect(columnFormOps(f, { table: T, editing: col("memo"), projected: p })).toEqual([]);
  });
  it("existing_default_shows_as_expression_and_can_be_dropped", () => {
    const f = formFromColumn(col("status"), p, catalog);
    expect(f).toMatchObject({ type: { schema: "public", name: "invoice_status" }, defaultKind: "expression", defaultText: "'open'::invoice_status", notNull: true });
    expect(columnFormOps({ ...f, defaultKind: "none" }, { table: T, editing: col("status"), projected: p })).toEqual([
      { alter_column: { table: T, column: "status", change: { default: "drop" } } },
    ]);
  });
  it("type_and_unique_toggles_on_an_existing_column", () => {
    const f = formFromColumn(col("amount_cents"), p, catalog);
    expect(f.type).toEqual({ schema: "pg_catalog", name: "int8" });
    expect(columnFormOps({ ...f, type: { schema: "pg_catalog", name: "text" }, unique: true }, { table: T, editing: col("amount_cents"), projected: p })).toEqual([
      { alter_column: { table: T, column: "amount_cents", change: { data_type: { schema: "pg_catalog", name: "text", array: false } } } },
      { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["amount_cents"] } } } } },
    ]);
    const withKey = project(invoices(), [newEdit([{ add_constraint: { table: T, constraint: { name: "memo_key", kind: { unique: { columns: ["memo"] } } } } }])]);
    const memo = withKey.columns.find((c) => c.name === "memo")!;
    const g = formFromColumn(memo, withKey, catalog);
    expect(g.unique).toBe(true);
    expect(columnFormOps({ ...g, unique: false }, { table: T, editing: memo, projected: withKey })).toEqual([
      { drop_constraint: { table: T, name: "memo_key" } },
    ]);
  });
  it("array_types_come_from_array_oid", () => {
    const arr = { ...col("memo"), type_oid: 1009, data_type: "text[]" };
    expect(formFromColumn(arr, p, catalog)).toMatchObject({ type: { schema: "pg_catalog", name: "text" }, array: true });
  });
});
