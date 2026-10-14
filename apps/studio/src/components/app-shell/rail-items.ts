import { BookOpenText, Boxes, Table2, Users, type LucideIcon } from "lucide-react";

export type RailLink = {
  kind: "link";
  id: "projects" | "tables" | "api" | "members";
  label: string;
  to: "/app" | "/app/tables" | "/app/api" | "/app/members";
  icon: LucideIcon;
};
export type RailPlaceholder = { kind: "placeholder"; id: string; label: "Placeholder"; icon: LucideIcon };
export type RailEntry = RailLink | RailPlaceholder;

/** The rail's items, top to bottom. Members is last. The union keeps a placeholder kind for sections not built yet. */
export const RAIL_ENTRIES: readonly RailEntry[] = [
  { kind: "link", id: "projects", label: "Projects", to: "/app", icon: Boxes },
  { kind: "link", id: "tables", label: "Table editor", to: "/app/tables", icon: Table2 },
  { kind: "link", id: "api", label: "API reference", to: "/app/api", icon: BookOpenText },
  { kind: "link", id: "members", label: "Members", to: "/app/members", icon: Users },
];

/** The table editor's route: every level is an optional segment, so the rail links to it bare. */
export const TABLES_ROUTE = "/app/tables/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}/{-$schema}/{-$table}" as const;

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
    case "api":
      return pathname === "/app/api" || pathname.startsWith("/app/api/");
    case "members":
      return pathname === "/app/members" || pathname.startsWith("/app/members/");
  }
}
