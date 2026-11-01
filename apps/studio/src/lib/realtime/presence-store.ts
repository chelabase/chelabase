import { createContext, useContext } from "react";
import { createStore, useStore, type StoreApi } from "zustand";
import type { PresenceMember } from "./types";

/**
 * A table's presence, one store per open table (RowsView's mount). The realtime callbacks write it with `setState`;
 * the presence layer and the avatar stack read it through selectors, so a presence frame never re-renders RowsView
 * or the grid.
 */
export interface PresenceStoreState {
  members: PresenceMember[];
}

export function createPresenceStore(): StoreApi<PresenceStoreState> {
  return createStore<PresenceStoreState>()(() => ({ members: [] }));
}

export const PresenceStoreContext = createContext<StoreApi<PresenceStoreState> | null>(null);

export function usePresence<T>(selector: (s: PresenceStoreState) => T): T {
  const store = useContext(PresenceStoreContext);
  if (!store) throw new Error("usePresence needs a PresenceStoreContext provider");
  return useStore(store, selector);
}
