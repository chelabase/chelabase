import { useGetMe, useListMembers } from "@chelabase/management-client";

/** How often the rail's request count refreshes while Studio is open. */
export const PENDING_POLL_MS = 30_000;

/**
 * Sign-up requests waiting for a tenant admin (R40): the Members rail badge. Only tenant admins
 * see a count (only they can answer one); for everyone else nothing is fetched.
 */
export function usePendingRequestCount(): number {
  const me = useGetMe();
  const tenantAdmin = me.data?.member?.tenant_admin === true;
  const members = useListMembers({ query: { enabled: tenantAdmin, refetchInterval: tenantAdmin ? PENDING_POLL_MS : false } });
  if (!tenantAdmin) return 0;
  return (members.data ?? []).filter((m) => m.status === "pending").length;
}
