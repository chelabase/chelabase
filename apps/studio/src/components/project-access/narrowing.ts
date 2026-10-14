import type { GrantOverride, PlatformRole } from "@chelabase/management-client";
import { ROLE_LABEL } from "@/lib/members";

const RANK: Record<PlatformRole, number> = { viewer: 0, developer: 1, admin: 2 };

/** What a narrowing form holds: `null` means "same as the project". */
export interface NarrowingDraft {
  platformRole: PlatformRole | null;
  pgRoles: string[] | null;
}

/**
 * Whether the draft takes anything away from the grant: a lower role, or fewer roles. The API
 * refuses "narrows nothing" (422), so Save waits until this is true.
 */
export function narrowsGrant(grant: { platformRole: PlatformRole; pgRoles: "all" | readonly string[] }, draft: NarrowingDraft): boolean {
  const lower = draft.platformRole !== null && RANK[draft.platformRole] < RANK[grant.platformRole];
  // Every role (an Admin grant, R17): any named roles are fewer.
  const fewer = draft.pgRoles !== null && (grant.pgRoles === "all" || draft.pgRoles.length < grant.pgRoles.length);
  return lower || fewer;
}

/** An override in words: "Viewer", "billing only", "Viewer, billing only", "no roles". */
export function overrideWords(o: Pick<GrantOverride, "platform_role" | "pg_roles">): string {
  const parts: string[] = [];
  if (o.platform_role) parts.push(ROLE_LABEL[o.platform_role]);
  if (o.pg_roles) parts.push(o.pg_roles.length === 0 ? "no roles" : `${o.pg_roles.join(", ")} only`);
  return parts.join(", ");
}
