import { configureManagementClient } from "@chelabase/management-client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render } from "@testing-library/react";
import type { ReactElement, ReactNode } from "react";
import { RealtimeContext } from "@/lib/realtime/context";
import type { RealtimeClient } from "@/lib/realtime/types";
import { fakeFetch } from "./fake-fetch";

export interface RenderWithQueryOptions {
  fetch?: typeof fetch;
  /** The management client's 401 hook; defaults to "not restarting sign-in". */
  onUnauthorized?: () => boolean | void;
  /** The tab's realtime client; none = no realtime. */
  realtime?: RealtimeClient | null;
}

export function renderWithQuery(ui: ReactElement, opts: RenderWithQueryOptions = {}) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  configureManagementClient({
    baseUrl: "http://studio.test",
    getToken: () => "test-token",
    onUnauthorized: opts.onUnauthorized ?? (() => false),
    fetchImpl: opts.fetch ?? fakeFetch({}),
  });
  // A wrapper (not a wrapped `ui`) so `rerender` keeps the provider.
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>
      <RealtimeContext.Provider value={opts.realtime ?? null}>{children}</RealtimeContext.Provider>
    </QueryClientProvider>
  );
  const result = render(ui, { wrapper });
  return { ...result, queryClient };
}
