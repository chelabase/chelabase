/** The table a view works on, as every request about it addresses it. */
export interface TableTarget {
  databaseId: string;
  branchId: string;
  /** The Postgres database's name (`app`). */
  pgDatabase: string;
  /** The `database` request parameter: null for the branch's main database (P9). */
  database: string | null;
  schema: string;
  table: string;
  /** The role rows are read and written as; null is the owner. */
  role: string | null;
  /** The changeset key (`changesetKey`): also keys per-table preferences such as column widths. */
  key: string;
  /**
   * Set for the two views that have no table yet: the schema's enums (`types`, changeset `…/{schema}.#types`)
   * and the new-table form (`new_table`, `…/{schema}.#new`). `table` then holds the `#…` name.
   */
  draft?: "types" | "new_table";
  /**
   * Set when the object list says this is not an editable table (a view, materialized view, foreign or partitioned
   * table): the table-only endpoints (`GET …/tables/{schema}/{name}`) are never asked for it.
   */
  notTable?: boolean;
}
