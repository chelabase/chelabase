import {
  Outlet,
  RouterProvider,
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
} from "@tanstack/react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { RAIL_STORAGE_KEY } from "@/lib/rail-preference";
import { mockMatchMedia } from "@/test/match-media";
import { AppShell } from "./app-shell";

beforeAll(() => {
  // The router's scroll restoration calls window.scrollTo, which jsdom doesn't implement.
  Object.defineProperty(window, "scrollTo", { value: () => {}, writable: true, configurable: true });
});

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});

async function renderShell(waitForRail = true, initialPath = "/app/") {
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
    history: createMemoryHistory({ initialEntries: [initialPath] }),
  });
  const queryClient = new QueryClient();
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  if (waitForRail) await screen.findByRole("navigation", { name: "Studio" });
  else await screen.findByRole("main");
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

  // Replaces "expanded below 40rem keeps a 56px rail column" (T2, intended contract change): below
  // 40rem there is no docked column at all; the rail lives in the drawer.
  it("below 40rem the grid is one column and no rail is docked", async () => {
    const mm = mockMatchMedia((q) => q.includes("width < 40rem"));
    try {
      window.localStorage.setItem(RAIL_STORAGE_KEY, "expanded");
      await renderShell(false);
      const grid = screen.getByRole("main").closest("[style*='grid-template-columns']");
      expect(grid).toHaveStyle({ gridTemplateColumns: "1fr" });
      expect(screen.queryByRole("navigation", { name: "Studio" })).toBeNull();
      expect(screen.getByRole("button", { name: "Menu" })).toHaveAttribute("aria-expanded", "false");
    } finally {
      mm.restore();
    }
  });

  it("at wide widths there is no menu button", async () => {
    await renderShell();
    expect(screen.queryByRole("button", { name: "Menu" })).toBeNull();
  });
});

describe("AppShell drawer", () => {
  let mm: ReturnType<typeof mockMatchMedia>;
  beforeEach(() => {
    mm = mockMatchMedia((q) => q.includes("width < 40rem"));
  });
  afterEach(() => mm.restore());

  async function openDrawer(initialPath = "/app/") {
    const router = await renderShell(false, initialPath);
    const button = screen.getByRole("button", { name: "Menu" });
    const user = userEvent.setup();
    await user.click(button);
    const dialog = await screen.findByRole("dialog", { name: "Menu" });
    return { router, button, user, dialog };
  }

  it("opens an expanded rail in a modal dialog, with aria-expanded and aria-controls", async () => {
    window.localStorage.setItem(RAIL_STORAGE_KEY, "collapsed");
    const { button, dialog } = await openDrawer();
    expect(button).toHaveAttribute("aria-expanded", "true");
    expect(button).toHaveAttribute("aria-controls", dialog.id);
    const nav = within(dialog).getByRole("navigation", { name: "Studio" });
    expect(nav).toHaveAttribute("data-expanded", "true");
    expect(within(nav).getByRole("link", { name: "Projects" })).toBeVisible();
    // The setting is untouched and not offered here.
    expect(within(nav).queryByRole("button")).toBeNull();
    expect(window.localStorage.getItem(RAIL_STORAGE_KEY)).toBe("collapsed");
  });

  it("aria-controls is absent while closed", async () => {
    await renderShell(false);
    expect(screen.getByRole("button", { name: "Menu" })).not.toHaveAttribute("aria-controls");
  });

  it("moves focus into the drawer and keeps Tab inside it", async () => {
    const { dialog, user } = await openDrawer();
    await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
    for (let i = 0; i < 12; i++) {
      await user.tab();
      // Focus guards hand focus back asynchronously.
      await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
    }
    for (let i = 0; i < 12; i++) {
      await user.tab({ shift: true });
      await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
    }
  });

  it("Escape closes it and returns focus to the button", async () => {
    const { button, user } = await openDrawer();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(button).toHaveFocus();
    expect(button).toHaveAttribute("aria-expanded", "false");
  });

  it("a backdrop click closes it and returns focus to the button", async () => {
    const { button, user } = await openDrawer();
    const scrim = Array.from(document.querySelectorAll<HTMLElement>("div.fixed.inset-0")).find((e) => e.className.includes("bg-black"))!;
    await user.click(scrim);
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(button).toHaveFocus());
  });

  it("clicking a rail link closes it, even the page already shown", async () => {
    const { user, dialog } = await openDrawer();
    await user.click(within(dialog).getByRole("link", { name: "Projects" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("clicking the home link closes it", async () => {
    // Opened at /app, the home link leads to the same pathname: only onNavigate can close the drawer.
    const { user, dialog, router } = await openDrawer("/app");
    await user.click(within(dialog).getByRole("link", { name: "Chelabase Studio home" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(router.state.location.pathname).toBe("/app");
  });

  it("a navigation (browser Back style) closes it", async () => {
    const { router } = await openDrawer();
    await act(() => router.navigate({ to: "/app/api" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("widening past the breakpoint closes it and shows the docked rail", async () => {
    await openDrawer();
    act(() => mm.set(() => false));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(screen.getByRole("navigation", { name: "Studio" })).toHaveAttribute("data-variant", "docked");
    expect(screen.queryByRole("button", { name: "Menu" })).toBeNull();
    expect(screen.getAllByRole("navigation", { name: "Studio" })).toHaveLength(1);
  });

  it("widening past the breakpoint with the drawer open moves focus to #content", async () => {
    const { dialog } = await openDrawer();
    await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
    act(() => mm.set(() => false));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(document.getElementById("content")));
  });

  it("only one Studio navigation exists at narrow width", async () => {
    await openDrawer();
    expect(screen.getAllByRole("navigation", { name: "Studio" })).toHaveLength(1);
  });

  it("the page behind is hidden from assistive tech while open", async () => {
    await openDrawer();
    expect(screen.queryByRole("main")).toBeNull();
  });
});

describe("AppShell stored behaviour", () => {
  it("starts in the stored behaviour", async () => {
    window.localStorage.setItem(RAIL_STORAGE_KEY, "collapsed");
    await renderShell();
    expect(screen.getByRole("navigation", { name: "Studio" })).toHaveAttribute("data-behaviour", "collapsed");
  });
});
