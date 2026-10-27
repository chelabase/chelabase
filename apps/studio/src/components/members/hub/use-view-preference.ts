import { useCallback, useState } from "react";
import type { RosterView } from "./view-toggle";

const key = (scope: string) => `chelabase.members.view.${scope}`;

function read(scope: string): RosterView {
  try {
    const v = localStorage.getItem(key(scope));
    return v === "cards" || v === "list" ? v : "list";
  } catch {
    return "list";
  }
}

/** The remembered cards/list choice of a hub list (MH-38b): storage may be absent or throw, and then it is the list. */
export function useViewPreference(scope: "roster" | "roles" | "teams"): [RosterView, (next: RosterView) => void] {
  const [view, setView] = useState<RosterView>(() => read(scope));
  const set = useCallback(
    (next: RosterView) => {
      setView(next);
      try {
        localStorage.setItem(key(scope), next);
      } catch {
        // Kept for this visit only.
      }
    },
    [scope],
  );
  return [view, set];
}
