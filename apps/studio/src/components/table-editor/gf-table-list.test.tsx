import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ObjectSummary } from "@chelabase/management-client";
import { obj } from "@/test/table-editor-fixtures";
import { TableEditorStoreContext, createTableEditorStore } from "@/lib/table-editor/store";
import { TableList } from "./table-list";

/**
 * RT1 gf, item 5 (user, 2026-10-21): the list header counts each kind apart and shows only the kinds present, in this
 * order: tables, views, materialized views, foreign tables. Partitioned tables count as tables (and so does anything
 * that inherits: it is a `table`). Singular and plural are right for each kind.
 */
const params = { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public" };
const kinds = (list: Record<string, number>, schema = "public") =>
  Object.entries(list).flatMap(([kind, n]) => Array.from({ length: n }, (_, i) => obj(schema, `${kind}_${i}`, { kind, editable: kind === "table" || kind === "partitioned_table" }))) as ObjectSummary[];
const header = (objects: ObjectSummary[]) => {
  render(
    <TableEditorStoreContext.Provider value={createTableEditorStore(null)}>
      <TableList params={params} navigate={vi.fn()} objects={objects} />
    </TableEditorStoreContext.Provider>,
  );
  return screen.getByRole("navigation", { name: "Tables" }).querySelector("p")?.textContent ?? "";
};

describe("table list count line", () => {
  it("counts_each_kind_apart", () => {
    expect(header(kinds({ table: 4, view: 2, materialized_view: 1, foreign_table: 1 }))).toBe("public · 4 tables, 2 views, 1 materialized view, 1 foreign table");
  });

  it("a_materialized_view_is_not_a_view", () => {
    expect(header(kinds({ table: 3, view: 2, materialized_view: 1 }))).toBe("public · 3 tables, 2 views, 1 materialized view");
  });

  it("plural_forms_are_right_for_every_kind", () => {
    expect(header(kinds({ table: 2, view: 1, materialized_view: 2, foreign_table: 3 }))).toBe("public · 2 tables, 1 view, 2 materialized views, 3 foreign tables");
  });

  it("only_the_kinds_present_are_shown", () => {
    expect(header(kinds({ materialized_view: 1 }))).toBe("public · 1 materialized view");
  });

  it("only_foreign_tables", () => {
    expect(header(kinds({ foreign_table: 2 }))).toBe("public · 2 foreign tables");
  });

  it("partitioned_tables_count_as_tables", () => {
    expect(header(kinds({ table: 1, partitioned_table: 2, view: 1 }))).toBe("public · 3 tables, 1 view");
  });

  it("another_schemas_objects_are_not_counted", () => {
    const objects = [...kinds({ table: 1 }), ...kinds({ materialized_view: 4, foreign_table: 4 }, "billing")];
    expect(header(objects)).toBe("public · 1 table");
  });

  it("an_empty_schema_keeps_the_zero_tables_line", () => {
    expect(header(kinds({ table: 2 }, "billing"))).toBe("public · 0 tables");
  });
});
