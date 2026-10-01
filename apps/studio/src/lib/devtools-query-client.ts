import type { QueryClient } from "@tanstack/react-query";
import { useEffect, useSyncExternalStore } from "react";

/**
 * The root's devtools sit above the per-page QueryClientProviders (the
 * dashboard's and the sign-in pages'), so each page hands its client over here.
 * Dev builds only: production registers nothing.
 */
let current: QueryClient | null = null;
const listeners = new Set<() => void>();

function set(client: QueryClient | null) {
  current = client;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useRegisterDevtoolsQueryClient(client: QueryClient): void {
  useEffect(() => {
    if (!import.meta.env.DEV) return;
    set(client);
    return () => {
      if (current === client) set(null);
    };
  }, [client]);
}

export function useDevtoolsQueryClient(): QueryClient | null {
  return useSyncExternalStore(subscribe, () => current, () => null);
}
