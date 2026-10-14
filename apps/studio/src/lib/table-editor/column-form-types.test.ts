import { describe, expect, it } from "vitest";
import type { TypeEntry } from "@chelabase/management-client";
import { project } from "./changeset";
import { columnFormOps, formFromColumn } from "./column-form";
import { CATALOG, invoices } from "./test-tables";

const catalog = CATALOG as unknown as TypeEntry[];
const T = { schema: "public", name: "invoices" };

describe("column form type comparison", () => {
  it("an_unchanged_form_on_sql_spelled_types_emits_nothing", () => {
    const p = project(invoices(), []);
    for (const name of ["id", "status", "amount_cents", "memo"]) {
      const col = p.columns.find((c) => c.name === name)!;
      expect(columnFormOps(formFromColumn(col, p, catalog), { table: T, editing: col, projected: p })).toEqual([]);
    }
  });
  it("modifiers_and_qualified_names_compare_by_type", () => {
    const base = invoices();
    const varchar = { ...base.columns[4], data_type: "character varying(20)", type_oid: 1043 };
    const ts = { ...base.columns[4], data_type: "timestamp with time zone", type_oid: 1184 };
    const p = project({ ...base, columns: [varchar] } as typeof base, []);
    const col = p.columns[0];
    expect(columnFormOps({ ...formFromColumn(col, p, catalog), type: { schema: "pg_catalog", name: "varchar" } }, { table: T, editing: col, projected: p })).toEqual([]);
    const q = project({ ...base, columns: [ts] } as typeof base, []);
    const tcol = q.columns[0];
    const f = formFromColumn(tcol, q, catalog);
    expect(f.type).toEqual({ schema: "pg_catalog", name: "timestamptz" });
    expect(columnFormOps(f, { table: T, editing: tcol, projected: q })).toEqual([]);
  });
});
