import { Outlet, RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import type { ReactElement } from "react";
import { Breadcrumbs, CrumbLabelsProvider } from "@/components/app-shell/crumbs";
import { renderWithQuery, type RenderWithQueryOptions } from "./render";

/** A root route component: the labels provider and the header trail around the page. */
export function CrumbsRoot() {
  return (
    <CrumbLabelsProvider>
      <Breadcrumbs />
      <Outlet />
    </CrumbLabelsProvider>
  );
}

/**
 * Renders `ui` in a memory router at the route pattern `route.path` (for example
 * "/app/projects/$projectId"), opened at `route.at`, as `<CrumbLabelsProvider><Breadcrumbs />{ui}</…>`
 * inside the query client of `render.tsx`. Returns the `renderWithQuery` result plus `router`.
 */
export function renderWithCrumbs(ui: ReactElement, route: { path: string; at: string }, opts: RenderWithQueryOptions = {}) {
  const root = createRootRoute({
    component: CrumbsRoot,
  });
  const page = createRoute({ getParentRoute: () => root, path: route.path, component: () => ui });
  const router = createRouter({
    routeTree: root.addChildren([page]),
    history: createMemoryHistory({ initialEntries: [route.at] }),
  });
  return { ...renderWithQuery(<RouterProvider router={router} />, opts), router };
}
