import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import type { ReactElement } from "react";
import { renderWithQuery, type RenderWithQueryOptions } from "./render";

/**
 * Renders `ui` at "/" inside a memory router that also knows the project page
 * ("/app/projects/$projectId", shown as a marker) and the dashboard ("/app").
 */
export function renderWithRouter(ui: ReactElement, opts: RenderWithQueryOptions = {}) {
  const root = createRootRoute();
  const home = createRoute({ getParentRoute: () => root, path: "/", component: () => ui });
  const dashboard = createRoute({ getParentRoute: () => root, path: "/app", component: () => <p>dashboard route</p> });
  const project = createRoute({
    getParentRoute: () => root,
    path: "/app/projects/$projectId",
    component: function Marker() {
      return <p>project route {project.useParams().projectId}</p>;
    },
  });
  const router = createRouter({
    routeTree: root.addChildren([home, dashboard, project]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  return { ...renderWithQuery(<RouterProvider router={router} />, opts), router };
}
