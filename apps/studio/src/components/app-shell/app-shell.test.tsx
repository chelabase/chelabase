import {
  Outlet,
  RouterProvider,
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
} from "@tanstack/react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { RAIL_STORAGE_KEY } from "@/lib/rail-preference";
import { AppShell } from "./app-shell";

beforeAll(() => {
  // The router's scroll restoration calls window.scrollTo, which jsdom doesn't implement.
  Object.defineProperty(window, "scrollTo", { value: () => {}, writable: true, configurable: true });
});

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});

async function renderShell() {
  const root = createRootRoute();
  const layout = createRoute({
    getParentRoute: () => root,
    path: "/app",
    component: () => (
      <AppShell profile={{ name: "Ada", email: "ada@x.dev" }} onSignOut={() => {}}>
        <Outlet />
      </AppShell>
    ),
  });
  const index = createRoute({ getParentRoute: () => layout, path: "/", component: () => <p>dashboard body</p> });
  const api = createRoute({ getParentRoute: () => layout, path: "/api", component: () => <p>api body</p> });
  const router = createRouter({
    routeTree: root.addChildren([layout.addChildren([index, api])]),
    history: createMemoryHistory({ initialEntries: ["/app/"] }),
  });
  const queryClient = new QueryClient();
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  await screen.findByRole("navigation", { name: "Studio" });
  return router;
}

describe("AppShell", () => {
  it("the skip link is first and targets #content", async () => {
    await renderShell();
    await userEvent.tab();
    const skip = screen.getByRole("link", { name: "Skip to content" });
    expect(skip).toHaveFocus();
    expect(skip).toHaveAttribute("href", "#content");
    expect(screen.getByRole("main")).toHaveAttribute("id", "content");
  });

  it("renders nav, header and main landmarks", async () => {
    await renderShell();
    expect(screen.getByRole("navigation", { name: "Studio" })).toBeInTheDocument();
    expect(screen.getByRole("banner")).toBeInTheDocument();
    expect(screen.getByRole("main")).toBeInTheDocument();
  });

  it("renders the child route inside main", async () => {
    await renderShell();
    expect(await screen.findByText("dashboard body")).toBeInTheDocument();
    expect(screen.getByRole("main")).toContainElement(screen.getByText("dashboard body"));
  });

  it("keeps the same rail element across a child route change", async () => {
    const router = await renderShell();
    const before = screen.getByRole("navigation", { name: "Studio" });
    await act(() => router.navigate({ to: "/app/api" }));
    expect(await screen.findByText("api body")).toBeInTheDocument();
    expect(screen.getByRole("navigation", { name: "Studio" })).toBe(before);
  });

  it("defaults to hover behaviour when nothing is stored", async () => {
    await renderShell();
    expect(screen.getByRole("navigation", { name: "Studio" })).toHaveAttribute("data-behaviour", "hover");
  });

  it.each([
    ["expanded", "232px"],
    ["hover", "56px"],
    ["collapsed", "56px"],
  ])("%s behaviour gives a %s rail column", async (behaviour, width) => {
    window.localStorage.setItem(RAIL_STORAGE_KEY, behaviour);
    await renderShell();
    const grid = screen.getByRole("navigation", { name: "Studio" }).closest("[style*='grid-template-columns']");
    expect(grid).not.toBeNull();
    expect(grid).toHaveStyle({ gridTemplateColumns: `${width} 1fr` });
  });

  it("starts in the stored behaviour", async () => {
    window.localStorage.setItem(RAIL_STORAGE_KEY, "collapsed");
    await renderShell();
    expect(screen.getByRole("navigation", { name: "Studio" })).toHaveAttribute("data-behaviour", "collapsed");
  });
});
