import { useQuery } from "@tanstack/react-query";
import { getPrivileges, type BranchRole } from "@chelabase/management-client";
import { privilegeSummary } from "./model";

/** The most objects one schema's summary read asks for (the read's maximum page). */
const SUMMARY_LIMIT = "500";

/**
 * A role card's one-line privilege summary: the role's schema list, then each schema's first page
 * (up to 500 objects; "+" when there was more). Read lazily per card and cached; Viewer-level reads.
 */
export function usePrivilegeSummary(databaseId: string, branchId: string, pgDatabase: string | undefined, role: BranchRole) {
  return useQuery({
    queryKey: ["members", "role-summary", databaseId, branchId, pgDatabase, role.name, role.member_of],
    enabled: pgDatabase !== undefined,
    staleTime: 60_000,
    queryFn: async ({ signal }) => {
      const pg = pgDatabase!;
      const head = await getPrivileges(databaseId, branchId, pg, { role: role.name }, { signal });
      const pages = await Promise.all(
        head.schemas.map(async (s) => {
          const page = await getPrivileges(databaseId, branchId, pg, { role: role.name, schema: s.name, limit: SUMMARY_LIMIT }, { signal });
          return { objects: page.objects, truncated: page.next_after != null };
        }),
      );
      return privilegeSummary(role, pages);
    },
  });
}
