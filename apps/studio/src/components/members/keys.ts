import type { QueryClient } from "@tanstack/react-query";
import { getGetMeQueryKey, getListMembersQueryKey } from "@chelabase/management-client";

/** The tenant's access log (the Activity tab's infinite query). */
export const ACCESS_EVENTS_KEY = ["members", "access-events"] as const;

/** After any access change: the members list, the caller's own capabilities and the log. */
export function refreshAccess(queryClient: QueryClient): void {
  void queryClient.invalidateQueries({ queryKey: getListMembersQueryKey() });
  void queryClient.invalidateQueries({ queryKey: getGetMeQueryKey() });
  void queryClient.invalidateQueries({ queryKey: ACCESS_EVENTS_KEY });
}
