import type { AccessLevel, Member, PlatformRole } from "@chelabase/management-client";

export const P1 = "11111111-1111-4111-8111-111111111111";
export const P2 = "22222222-2222-4222-8222-222222222222";

/** A roster member; `projects` are the caller-visible `access.projects` (via always own in step 1). */
export function person(
  id: string,
  opts: {
    level?: AccessLevel;
    projects?: [string, PlatformRole][];
    status?: Member["status"];
    tenantAdmin?: boolean;
    name?: string;
    hasAccess?: boolean;
  } = {},
): Member {
  const level = opts.level ?? "none";
  return {
    id,
    email: `${id}@example.test`,
    display_name: opts.name ?? id,
    status: opts.status ?? "active",
    tenant_admin: opts.tenantAdmin ?? level === "tenant_admin",
    has_access: opts.hasAccess ?? level !== "none",
    invite: null,
    accepted_at: null, suspended_at: null, suspended_by: null,
    created_at: "2026-10-01T00:00:00Z",
    access: { level, projects: (opts.projects ?? []).map(([project_id, platform_role]) => ({ project_id, platform_role, via: "own" as const })) },
  };
}
