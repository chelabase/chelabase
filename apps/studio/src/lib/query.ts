import { ApiError } from "@chelabase/management-client";
import { QueryClient } from "@tanstack/react-query";

export const POLL_MS = 2000;

/**
 * Environment/branch statuses that settle on their own; screens poll while in one.
 * `resetting` is a branch status only (reset to parent, restore); databases never show it.
 */
export const TRANSITIONAL = new Set(["pending", "provisioning", "resetting", "deleting"]);

const MAX_RETRIES = 2;

function shouldRetry(failureCount: number, error: unknown): boolean {
  if (!(error instanceof ApiError)) return false;
  if (error.status >= 400 && error.status <= 499) return false;
  if (error.code === "network" || error.status >= 500) return failureCount < MAX_RETRIES;
  return false;
}

export function createStudioQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: { staleTime: 10_000, refetchOnWindowFocus: true, retry: shouldRetry },
      mutations: { retry: false },
    },
  });
}

/**
 * Builds a `refetchInterval` that polls every POLL_MS while `busy(data)` holds.
 * It stops once a 401 restarted sign-in: the page is about to leave.
 */
export function pollWhile<T>(busy: (data: T) => boolean) {
  // `error` is read untyped so this type doesn't drive the hooks' TError inference.
  return (query: { state: { data: T | undefined } }): number | false => {
    const { data } = query.state;
    const error: unknown = (query.state as { error?: unknown }).error;
    if (error instanceof ApiError && error.signInRestarted) return false;
    return data !== undefined && busy(data) ? POLL_MS : false;
  };
}
