import { BookOpenText, Boxes, KeyRound, SquareTerminal, Table2, Users, type LucideIcon } from "lucide-react";
import { projectCaps } from "@/lib/capabilities";
import type { MeResponse } from "@chelabase/management-client";

export type RailLink = {
  kind: "link";
  id: "projects" | "tables" | "sql" | "data-api" | "api" | "members";
  label: string;
  to: "/app" | "/app/tables" | "/app/sql" | "/app/data-api" | "/app/api" | "/app/members";
  icon: LucideIcon;
};
export type RailPlaceholder = { kind: "placeholder"; id: string; label: "Placeholder"; icon: LucideIcon };
export type RailEntry = RailLink | RailPlaceholder;

/** The rail's items, top to bottom. Members is last. The union keeps a placeholder kind for sections not built yet. */
export const RAIL_ENTRIES: readonly RailEntry[] = [
  { kind: "link", id: "projects", label: "Projects", to: "/app", icon: Boxes },
  { kind: "link", id: "tables", label: "Table editor", to: "/app/tables", icon: Table2 },
  { kind: "link", id: "sql", label: "SQL editor", to: "/app/sql", icon: SquareTerminal },
  { kind: "link", id: "data-api", label: "Data API", to: "/app/data-api", icon: KeyRound },
  { kind: "link", id: "api", label: "API reference", to: "/app/api", icon: BookOpenText },
  { kind: "link", id: "members", label: "Members", to: "/app/members", icon: Users },
];

/** The table editor's route: every level is an optional segment, so the rail links to it bare. */
export const TABLES_ROUTE = "/app/tables/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}/{-$schema}/{-$table}" as const;

/** The SQL editor's route: every level is an optional segment, so the rail links to it bare. */
export const SQL_ROUTE = "/app/sql/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}" as const;

/** The Data API page's route: every level is an optional segment, so the rail links to it bare. */
export const DATA_API_ROUTE = "/app/data-api/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}" as const;

/** The SQL editor needs Developer: the caller must be one on some database (Viewers read through Tables). */
export function canUseSql(me: MeResponse | undefined): boolean {
  if (!me) return false;
  if (me.member?.tenant_admin === true) return true;
  return (me.tenant?.projects ?? []).some((p) => p.databases.some((d) => projectCaps(me, p.id, d.database_id).atLeast("developer")));
}

/** Counts shown on rail items (Members: sign-up requests waiting for a tenant admin). */
export type RailBadges = Partial<Record<RailLink["id"], number>>;

/**
 * Whether the item is the current section. Matched by segment, never by a bare
 * "/app" prefix, so Projects stays dark on "/app/api".
 */
export function isRailItemActive(id: RailLink["id"], pathname: string): boolean {
  switch (id) {
    case "projects":
      return pathname === "/app" || pathname === "/app/" || pathname.startsWith("/app/projects/");
    case "tables":
      return pathname === "/app/tables" || pathname.startsWith("/app/tables/");
    case "sql":
      return pathname === "/app/sql" || pathname.startsWith("/app/sql/");
    case "data-api":
      return pathname === "/app/data-api" || pathname.startsWith("/app/data-api/");
    case "api":
      return pathname === "/app/api" || pathname.startsWith("/app/api/");
    case "members":
      return pathname === "/app/members" || pathname.startsWith("/app/members/");
  }
}
