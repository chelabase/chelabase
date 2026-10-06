import { BookOpenText, Boxes, CircleDashed, type LucideIcon } from "lucide-react";

export type RailLink = { kind: "link"; id: "projects" | "api"; label: string; to: "/app" | "/app/api"; icon: LucideIcon };
export type RailPlaceholder = { kind: "placeholder"; id: string; label: "Placeholder"; icon: LucideIcon };
export type RailEntry = RailLink | RailPlaceholder;

/** The rail's items, top to bottom. Placeholders stand for sections not built yet. */
export const RAIL_ENTRIES: readonly RailEntry[] = [
  { kind: "link", id: "projects", label: "Projects", to: "/app", icon: Boxes },
  { kind: "link", id: "api", label: "API reference", to: "/app/api", icon: BookOpenText },
  { kind: "placeholder", id: "placeholder-1", label: "Placeholder", icon: CircleDashed },
  { kind: "placeholder", id: "placeholder-2", label: "Placeholder", icon: CircleDashed },
];

/**
 * Whether the item is the current section. Matched by segment, never by a bare
 * "/app" prefix, so Projects stays dark on "/app/api".
 */
export function isRailItemActive(id: RailLink["id"], pathname: string): boolean {
  switch (id) {
    case "projects":
      return pathname === "/app" || pathname === "/app/" || pathname.startsWith("/app/projects/");
    case "api":
      return pathname === "/app/api" || pathname.startsWith("/app/api/");
  }
}
