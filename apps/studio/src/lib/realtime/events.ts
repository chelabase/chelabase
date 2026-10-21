import { getGetMeQueryKey, getListBranchDatabasesQueryKey, getListBranchesQueryKey } from "@chelabase/management-client";
import type { QueryClient } from "@tanstack/react-query";
import type { ProjectEvent } from "./types";

/** P10: the slow fallback poll while the socket is live. */
export const SLOW_POLL_MS = 30_000;

function invalidate(qc: QueryClient, keys: readonly (readonly unknown[])[]) {
  for (const queryKey of keys) void qc.invalidateQueries({ queryKey });
}

/** A `project` event invalidates exactly the orval keys it concerns (§5.3), nothing else. */
export function invalidateForEvent(qc: QueryClient, event: ProjectEvent): void {
  switch (event.event) {
    case "branch.status":
    case "branch.created":
    case "branch.deleted":
      return invalidate(qc, [getListBranchesQueryKey(event.database_id), getListBranchDatabasesQueryKey(event.database_id, event.branch_id)]);
    case "database.created":
    case "database.deleted":
      // The caller's databases come with `/v1/me` (R21: an unknown database refreshes that list).
      return invalidate(qc, [getGetMeQueryKey()]);
    case "pg_database.created":
    case "pg_database.dropped":
      return invalidate(qc, [getListBranchDatabasesQueryKey(event.database_id, event.branch_id)]);
  }
}

/** A reconnect or `resync`: everything the editor shows (at least the branch list, the branch's databases and `/v1/me`). */
export function invalidateAll(qc: QueryClient, scope: { databaseId: string; branchId: string }): void {
  invalidate(qc, [getListBranchesQueryKey(scope.databaseId), getListBranchDatabasesQueryKey(scope.databaseId, scope.branchId), getGetMeQueryKey()]);
}

/** The `refetchInterval` of a polled query: the slow poll while live, today's `whileNotLive` otherwise. */
export function fallbackInterval(live: boolean, whileNotLive: number | false): number | false {
  return live ? SLOW_POLL_MS : whileNotLive;
}
