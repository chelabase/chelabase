import { BookOpenText, Boxes, FolderArchive, KeyRound, SquareTerminal, Table2, Users, Zap, type LucideIcon } from "lucide-react";
import { projectCaps } from "@/lib/capabilities";
import type { MeResponse } from "@chelabase/management-client";

export type RailLink = {
  kind: "link";
  id: "projects" | "tables" | "sql" | "storage" | "data-api" | "functions" | "api" | "members";
  label: string;
  to: "/app" | "/app/tables" | "/app/sql" | "/app/storage" | "/app/data-api" | "/app/functions" | "/app/api" | "/app/members";
  icon: LucideIcon;
};
export type RailPlaceholder = { kind: "placeholder"; id: string; label: "Placeholder"; icon: LucideIcon };
export type RailEntry = RailLink | RailPlaceholder;

/** The rail's items, top to bottom. Members is last. The union keeps a placeholder kind for sections not built yet. */
export const RAIL_ENTRIES: readonly RailEntry[] = [
  { kind: "link", id: "projects", label: "Projects", to: "/app", icon: Boxes },
  { kind: "link", id: "tables", label: "Table editor", to: "/app/tables", icon: Table2 },
  { kind: "link", id: "sql", label: "SQL editor", to: "/app/sql", icon: SquareTerminal },
  // Shown wherever the table editor is: Viewers browse buckets read-only.
  { kind: "link", id: "storage", label: "Storage", to: "/app/storage", icon: FolderArchive },
  { kind: "link", id: "data-api", label: "Data API", to: "/app/data-api", icon: KeyRound },
  // Hidden in platform mode (`/v1/me` reports `functions_available: false`).
  { kind: "link", id: "functions", label: "Functions", to: "/app/functions", icon: Zap },
  { kind: "link", id: "api", label: "API reference", to: "/app/api", icon: BookOpenText },
  { kind: "link", id: "members", label: "Members", to: "/app/members", icon: Users },
];

/** The table editor's route: every level is an optional segment, so the rail links to it bare. */
export const TABLES_ROUTE = "/app/tables/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}/{-$schema}/{-$table}" as const;

/** The SQL editor's route: every level is an optional segment, so the rail links to it bare. */
export const SQL_ROUTE = "/app/sql/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}" as const;

/** The Storage tab's route: every level is an optional segment (the splat is an object path), so the rail links to it bare. */
export const STORAGE_ROUTE = "/app/storage/{-$projectId}/{-$databaseId}/{-$branchId}/{-$bucket}/$" as const;

/** The Data API page's route: every level is an optional segment, so the rail links to it bare. */
export const DATA_API_ROUTE = "/app/data-api/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}" as const;

/** The Functions tab's route: every level is an optional segment, so the rail links to it bare. */
export const FUNCTIONS_ROUTE = "/app/functions/{-$projectId}/{-$databaseId}/{-$branchId}" as const;

/** The SQL editor needs Developer: the caller must be one on some database (Viewers read through Tables). */
export function canUseSql(me: MeResponse | undefined): boolean {
  if (!me) return false;
  if (me.member?.tenant_admin === true) return true;
  return (me.tenant?.projects ?? []).some((p) => p.databases.some((d) => projectCaps(me, p.id, d.database_id).atLeast("developer")));
}

/**
 * The rail items left out for this caller: the SQL editor for someone who isn't a Developer
 * anywhere, and Functions when `/v1/me` says the instance has none (platform mode). Until `/v1/me`
 * has loaded, Functions stays.
 */
export function hiddenRailItems(me: MeResponse | undefined): RailLink["id"][] {
  return [...(canUseSql(me) ? [] : (["sql"] as const)), ...(me?.functions_available === false ? (["functions"] as const) : [])];
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
    case "storage":
      return pathname === "/app/storage" || pathname.startsWith("/app/storage/");
    case "data-api":
      return pathname === "/app/data-api" || pathname.startsWith("/app/data-api/");
    case "functions":
      return pathname === "/app/functions" || pathname.startsWith("/app/functions/");
    case "api":
      return pathname === "/app/api" || pathname.startsWith("/app/api/");
    case "members":
      return pathname === "/app/members" || pathname.startsWith("/app/members/");
  }
}
