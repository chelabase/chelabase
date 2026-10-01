import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ApiNavLink } from "./api-nav-link";

describe("ApiNavLink", () => {
  it("renders an API link to /app/api", async () => {
    const root = createRootRoute({ component: ApiNavLink });
    const api = createRoute({ getParentRoute: () => root, path: "/app/api" });
    const router = createRouter({ routeTree: root.addChildren([api]), history: createMemoryHistory({ initialEntries: ["/"] }) });
    render(<RouterProvider router={router} />);
    const link = await screen.findByRole("link", { name: "API" });
    expect(link).toHaveAttribute("href", "/app/api");
  });
});
