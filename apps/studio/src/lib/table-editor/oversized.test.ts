import { describe, expect, it } from "vitest";
import { isLargeColumn, smallColumns } from "./oversized";
import { invoices } from "./test-tables";

describe("oversized rows", () => {
  it("small_columns_drop_large_types_and_arrays_but_keep_the_key", () => {
    const t = invoices();
    expect(smallColumns(t)).toEqual(["id", "customer_id", "status", "amount_cents"]);
    t.columns = [
      ...t.columns,
      { ...t.columns[4], attnum: 6, name: "tags", data_type: "character varying[]" },
      { ...t.columns[4], attnum: 7, name: "doc", data_type: "jsonb" },
      { ...t.columns[4], attnum: 8, name: "label", data_type: "character varying(20)" },
    ];
    expect(smallColumns(t)).toEqual(["id", "customer_id", "status", "amount_cents", "label"]);
    // A key column is read even when its type is large.
    t.columns[0] = { ...t.columns[0], data_type: "text" };
    expect(smallColumns(t)).toContain("id");
  });

  it("large_columns_are_arrays_and_the_large_types", () => {
    for (const t of ["text", "bytea", "json", "jsonb", "xml", "integer[]", "text[]", "uuid[3]"]) expect(isLargeColumn(t)).toBe(true);
    for (const t of ["uuid", "bigint", "character varying(20)", "timestamp with time zone", "invoice_status"]) expect(isLargeColumn(t)).toBe(false);
  });
});
