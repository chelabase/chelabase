import { useMemo } from "react";
import { useGetTable, useListTypes, type Operation, type Table, type TableRef, type TypeEntry } from "@chelabase/management-client";
import { newEdit, project, type Changeset, type Edit, type ProjectedTable } from "@/lib/table-editor/changeset";
import { useTableEditor } from "@/lib/table-editor/store";
import type { TableTarget } from "./target";

const NO_EDITS: Edit[] = [];

export interface TableStructure {
  /** The table as `getTable` last read it (the same query the rows view reads). */
  base: Table | undefined;
  /** Why the table couldn't be read, or null. */
  error: Error | null;
  /** The type catalog (`listTypes`). */
  catalog: TypeEntry[];
  changeset: Changeset | undefined;
  edits: Edit[];
  /** The table as it will look once the pending edits apply; undefined until the table is read. */
  projected: ProjectedTable | undefined;
  /** The table the operations address. */
  ref: TableRef;
  /** True while an Apply of this table's changes is in flight: nothing can be staged. */
  applying: boolean;
  /**
   * Why the table's structure can't take more edits beyond the changes panel, or null: a staged
   * `drop_table` ("public.invoices is dropped in your pending changes").
   */
  reason: string | null;
  /** Stages one user action. */
  stage: (ops: Operation[]) => void;
}

/** The open table's structure: its definition, the type catalog and its pending changeset. */
export function useTableStructure(target: TableTarget, opts: { enabled?: boolean } = {}): TableStructure {
  const { databaseId, branchId, database, schema, table, key } = target;
  const params = database ? { database } : undefined;
  const draft = target.draft;
  const got = useGetTable(databaseId, branchId, schema, table, params, { query: { enabled: (opts.enabled ?? true) && !draft && !target.notTable } });
  const types = useListTypes(databaseId, branchId, params);
  const changeset = useTableEditor((s) => s.changesets[key]);
  const addEdit = useTableEditor((s) => s.addEdit);
  const applying = useTableEditor((s) => !!s.applying[key]);
  const base = got.data;
  const edits = changeset?.edits ?? NO_EDITS;
  // A new-table draft has no base: the first `create_table` gives the table. A types draft has no table at all.
  const projected = useMemo(() => (base ? project(base, edits) : draft === "new_table" && edits.length > 0 ? project(null, edits) : undefined), [base, edits, draft]);
  const ref = useMemo(() => ({ schema, name: table }), [schema, table]);
  return {
    base,
    error: got.error ?? null,
    catalog: types.data?.types ?? [],
    changeset,
    edits,
    projected,
    ref,
    applying,
    reason: projected?.dropped ? `${schema}.${table} is dropped in your pending changes` : null,
    stage: (ops) => {
      if ((base || draft) && ops.length > 0 && !applying) addEdit(key, base ?? null, newEdit(ops));
    },
  };
}
