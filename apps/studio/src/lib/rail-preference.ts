import { useCallback, useState } from "react";

export type RailBehaviour = "expanded" | "collapsed" | "hover";

export const RAIL_BEHAVIOURS: readonly RailBehaviour[] = ["expanded", "collapsed", "hover"];

export const RAIL_STORAGE_KEY = "chelabase.studio.rail";

export const DEFAULT_RAIL_BEHAVIOUR: RailBehaviour = "hover";

function isRailBehaviour(value: unknown): value is RailBehaviour {
  return RAIL_BEHAVIOURS.includes(value as RailBehaviour);
}

/**
 * The stored sidebar behaviour, or the default when nothing valid is stored or
 * storage is unavailable. `getStorage` is called inside the try: with site data
 * blocked, reading `window.localStorage` itself throws.
 */
export function readRailBehaviour(getStorage: () => Storage = () => window.localStorage): RailBehaviour {
  try {
    const stored = getStorage().getItem(RAIL_STORAGE_KEY);
    return isRailBehaviour(stored) ? stored : DEFAULT_RAIL_BEHAVIOUR;
  } catch {
    return DEFAULT_RAIL_BEHAVIOUR;
  }
}

/** Persists the behaviour; a blocked or full storage is ignored (the choice then lasts the session). */
export function writeRailBehaviour(
  value: RailBehaviour,
  getStorage: () => Storage = () => window.localStorage,
): void {
  try {
    getStorage().setItem(RAIL_STORAGE_KEY, value);
  } catch {
    // Storage unavailable: nothing to persist to.
  }
}

/** The sidebar behaviour, read synchronously on first render; the setter updates state, then persists. */
export function useRailBehaviour(): [RailBehaviour, (next: RailBehaviour) => void] {
  const [behaviour, setBehaviour] = useState<RailBehaviour>(() => readRailBehaviour());
  const set = useCallback((next: RailBehaviour) => {
    setBehaviour(next);
    writeRailBehaviour(next);
  }, []);
  return [behaviour, set];
}
