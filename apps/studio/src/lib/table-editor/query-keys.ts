import type { QueryClient } from "@tanstack/react-query";
import { getGetSqlSchemaQueryKey, getListObjectsQueryKey, getListTypesQueryKey } from "@chelabase/management-client";

/**
 * The first key of every rows query. Apply (T5) and every row write (inline edits, the row
 * panel's save, insert and delete, insert with defaults) invalidate both
 * `[ROWS_QUERY, databaseId, branchId]` and `[COUNT_QUERY, databaseId, branchId]`.
 */
export const ROWS_QUERY = "table-editor-rows";
/** The first key of the selection bar's filtered count (kept apart from `ROWS_QUERY`; see there). */
export const COUNT_QUERY = "table-editor-count";

/** The invalidations after a row write or an Apply: the rows and the filtered count of the branch. */
export function rowWriteKeys(databaseId: string, branchId: string): readonly (readonly unknown[])[] {
  return [
    [ROWS_QUERY, databaseId, branchId],
    [COUNT_QUERY, databaseId, branchId],
  ];
}

/**
 * After a SQL run that changed the schema (realtime sends no DDL): every table's structure on the branch, the object
 * list and the types of the Postgres database, the rows (a column added or dropped), and the SQL schema for every
 * role (completion as any Act as role) are read again. Rows written by plain DML arrive live.
 * `pgParams` names the Postgres database; left out, the branch's main one.
 */
export function invalidateSchemaQueries(queryClient: QueryClient, databaseId: string, branchId: string, pgParams?: { database: string }) {
  const tables = `/v1/databases/${databaseId}/branches/${branchId}/tables/`;
  void queryClient.invalidateQueries({ predicate: (q) => typeof q.queryKey[0] === "string" && q.queryKey[0].startsWith(tables) });
  for (const queryKey of [
    getListObjectsQueryKey(databaseId, branchId, pgParams),
    getListTypesQueryKey(databaseId, branchId, pgParams),
    getGetSqlSchemaQueryKey(databaseId, branchId),
    ...rowWriteKeys(databaseId, branchId),
  ]) {
    void queryClient.invalidateQueries({ queryKey });
  }
}
