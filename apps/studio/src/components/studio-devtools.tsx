import { TanStackDevtools } from "@tanstack/react-devtools";
import { ReactQueryDevtoolsPanel } from "@tanstack/react-query-devtools";
import { TanStackRouterDevtoolsPanel } from "@tanstack/react-router-devtools";
import { useDevtoolsQueryClient } from "@/lib/devtools-query-client";

/** The query panel for the current page's client (sign-in pages and the dashboard each have their own). */
export function QueryDevtoolsPlugin() {
  const client = useDevtoolsQueryClient();
  if (!client) return <p className="p-4 text-sm">This page has no query client.</p>;
  return (
    <div data-testid="query-devtools-panel" className="h-full">
      <ReactQueryDevtoolsPanel client={client} />
    </div>
  );
}

/**
 * TanStack's devtools (router and query panels), dev builds only. No floating trigger: every position it offers covers
 * a Studio control (the row panel's Save, the rail, the account menu); the library's hotkey (Control+~) opens them.
 */
export function StudioDevtools() {
  if (!import.meta.env.DEV) return null;
  return (
    <TanStackDevtools
      config={{ triggerHidden: true }}
      plugins={[
        { name: "TanStack Router", render: <TanStackRouterDevtoolsPanel /> },
        { name: "TanStack Query", render: <QueryDevtoolsPlugin /> },
      ]}
    />
  );
}
