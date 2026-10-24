import type { ReactNode } from "react";
import { ShieldCheck } from "lucide-react";
import type { AccessLevel } from "@chelabase/management-client";

/** The words for each level; `none` has no tag (the group and status already say it). */
export const LEVEL_LABEL: Record<AccessLevel, string | null> = {
  tenant_admin: "Tenant admin",
  admin: "Project admin",
  developer: "Developer",
  viewer: "Viewer",
  other: "Other projects",
  none: null,
};

/** A member's level as a small framed tag; the tenant admin's is chalk with a shield. */
export function LevelBadge({ level }: { level: AccessLevel }) {
  const label = LEVEL_LABEL[level];
  if (label === null) return null;
  const tenant = level === "tenant_admin";
  return (
    <Tag chalk={tenant}>
      {tenant && <ShieldCheck aria-hidden="true" size={12} strokeWidth={1.5} />}
      {label}
    </Tag>
  );
}

/** The small framed tag the roster uses for levels and roles; it never wraps. */
export function Tag({ children, chalk = false }: { children: ReactNode; chalk?: boolean }) {
  return (
    <span className={`inline-flex items-center gap-1 border border-line-strong px-1.5 py-px text-xs whitespace-nowrap ${chalk ? "text-chalk" : "text-chalk-dim"}`}>
      {children}
    </span>
  );
}
