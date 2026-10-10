import {
  Outlet,
  RouterProvider,
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
} from "@tanstack/react-router";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import type { RailBehaviour } from "@/lib/rail-preference";
import { AppRail, HOVER_CLOSE_DELAY_MS, HOVER_OPEN_DELAY_MS } from "./app-rail";

beforeAll(() => {
  // The router's scroll restoration calls window.scrollTo, which jsdom doesn't implement.
  Object.defineProperty(window, "scrollTo", { value: () => {}, writable: true, configurable: true });
});

/**
 * Renders the rail at `path` inside a memory router that knows the routes it links to.
 * The behaviour lives in state, like the shell's, so `setBehaviour` re-renders the same rail.
 */
async function renderRail(initial: RailBehaviour, path = "/app", badges?: { members?: number }) {
  const onBehaviourChange = vi.fn();
  let setter: (next: RailBehaviour) => void = () => {};
  function Layout() {
    const [behaviour, setBehaviourState] = useState(initial);
    setter = setBehaviourState;
    return (
      <>
        <AppRail
          behaviour={behaviour}
          badges={badges}
          onBehaviourChange={(next) => {
            onBehaviourChange(next);
            setBehaviourState(next);
          }}
        />
        <main>
          <Outlet />
        </main>
      </>
    );
  }
  const root = createRootRoute({ component: Layout });
  const dashboard = createRoute({ getParentRoute: () => root, path: "/app", component: () => <p>dashboard</p> });
  const api = createRoute({ getParentRoute: () => root, path: "/app/api", component: () => <p>api</p> });
  const project = createRoute({
    getParentRoute: () => root,
    path: "/app/projects/$projectId",
    component: () => <p>project</p>,
  });
  const members = createRoute({ getParentRoute: () => root, path: "/app/members", component: () => <p>members</p> });
  const router = createRouter({
    routeTree: root.addChildren([dashboard, api, project, members]),
    history: createMemoryHistory({ initialEntries: [path] }),
  });
  render(<RouterProvider router={router} />);
  const nav = await screen.findByRole("navigation", { name: "Studio" });
  const setBehaviour = (next: RailBehaviour) => act(() => setter(next));
  return { nav, onBehaviourChange, router, setBehaviour };
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

afterEach(() => {
  vi.useRealTimers();
});

describe("AppRail", () => {
  // The browser pass: at 375px "Expanded" took 232px of the width and the page scrolled sideways.
  // Below 40rem it keeps the narrow rail in the layout and opens over the page like "hover".
  it("expanded below 40rem: the rail stays narrow in the layout and opens over the page", async () => {
    const original = window.matchMedia;
    window.matchMedia = ((query: string) =>
      ({ matches: query.includes("max-width: 40rem"), media: query, onchange: null, addEventListener: () => {}, removeEventListener: () => {}, addListener: () => {}, removeListener: () => {}, dispatchEvent: () => false }) as MediaQueryList);
    try {
      const { nav } = await renderRail("expanded");
      expect(nav).toHaveAttribute("data-expanded", "false");
      expect(nav.className).toContain("w-14");
    } finally {
      window.matchMedia = original;
    }
  });

  it("expanded: labels are visible and no tooltip appears on focus", async () => {
    const { nav } = await renderRail("expanded");
    expect(nav).toHaveAttribute("data-behaviour", "expanded");
    expect(nav).toHaveAttribute("data-expanded", "true");
    const user = userEvent.setup();
    await user.tab(); // home link
    await user.tab(); // Projects
    const projects = screen.getByRole("link", { name: "Projects" });
    expect(projects).toHaveFocus();
    expect(within(projects).getByText("Projects")).not.toHaveClass("sr-only");
    await sleep(400);
    // Only the inline label: no tooltip popup was portalled out.
    expect(screen.getAllByText("Projects")).toHaveLength(1);
  });

  it("collapsed: each link keeps its accessible name", async () => {
    const { nav } = await renderRail("collapsed");
    expect(nav).toHaveAttribute("data-expanded", "false");
    const projects = screen.getByRole("link", { name: "Projects" });
    expect(within(projects).getByText("Projects")).toHaveClass("sr-only");
    expect(screen.getByRole("link", { name: "API reference" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Chelabase Studio home" })).toBeInTheDocument();
  });

  it("collapsed: focusing a link shows its label in a tooltip", async () => {
    const { nav } = await renderRail("collapsed");
    const user = userEvent.setup();
    await user.tab(); // home link
    await user.tab(); // Projects
    expect(screen.getByRole("link", { name: "Projects" })).toHaveFocus();
    // Base UI's tooltip popup carries no role: find the label rendered outside the rail (the portal).
    await waitFor(() => {
      const popups = screen.getAllByText("Projects").filter((el) => !nav.contains(el));
      expect(popups).toHaveLength(1);
    });
    expect(screen.queryAllByText("API reference").filter((el) => !nav.contains(el))).toHaveLength(0);
  });

  it("hover: expands after the intent delay and collapses after the grace period", async () => {
    const { nav } = await renderRail("hover");
    vi.useFakeTimers();
    expect(nav).toHaveAttribute("data-expanded", "false");
    fireEvent.pointerEnter(nav);
    act(() => vi.advanceTimersByTime(100));
    expect(nav).toHaveAttribute("data-expanded", "false");
    act(() => vi.advanceTimersByTime(HOVER_OPEN_DELAY_MS - 100));
    expect(nav).toHaveAttribute("data-expanded", "true");
    fireEvent.pointerLeave(nav);
    act(() => vi.advanceTimersByTime(150));
    expect(nav).toHaveAttribute("data-expanded", "true");
    act(() => vi.advanceTimersByTime(HOVER_CLOSE_DELAY_MS - 150));
    expect(nav).toHaveAttribute("data-expanded", "false");
  });

  it("hover: a quick pass under the intent delay never expands", async () => {
    const { nav } = await renderRail("hover");
    vi.useFakeTimers();
    fireEvent.pointerEnter(nav);
    act(() => vi.advanceTimersByTime(100));
    fireEvent.pointerLeave(nav);
    act(() => vi.advanceTimersByTime(1000));
    expect(nav).toHaveAttribute("data-expanded", "false");
  });

  it("hover: re-entering during the grace period keeps it expanded", async () => {
    const { nav } = await renderRail("hover");
    vi.useFakeTimers();
    fireEvent.pointerEnter(nav);
    act(() => vi.advanceTimersByTime(HOVER_OPEN_DELAY_MS));
    fireEvent.pointerLeave(nav);
    act(() => vi.advanceTimersByTime(100));
    fireEvent.pointerEnter(nav);
    act(() => vi.advanceTimersByTime(1000));
    expect(nav).toHaveAttribute("data-expanded", "true");
  });

  it("hover: focus inside expands it; Escape collapses it and keeps focus", async () => {
    const { nav } = await renderRail("hover");
    const user = userEvent.setup();
    await user.tab();
    const focused = document.activeElement;
    expect(nav).toContainElement(focused as HTMLElement);
    expect(nav).toHaveAttribute("data-expanded", "true");
    await user.keyboard("{Escape}");
    expect(nav).toHaveAttribute("data-expanded", "false");
    expect(document.activeElement).toBe(focused);
  });

  it("hover: after Escape, tabbing to the next rail item expands the rail again", async () => {
    const { nav } = await renderRail("hover");
    const user = userEvent.setup();
    await user.tab();
    await user.keyboard("{Escape}");
    expect(nav).toHaveAttribute("data-expanded", "false");
    const before = document.activeElement;
    await user.tab();
    expect(document.activeElement).not.toBe(before);
    expect(nav).toContainElement(document.activeElement as HTMLElement);
    expect(nav).toHaveAttribute("data-expanded", "true");
  });

  it("hover: focus leaving the rail collapses it", async () => {
    const { nav } = await renderRail("hover");
    const user = userEvent.setup();
    await user.tab();
    expect(nav).toHaveAttribute("data-expanded", "true");
    act(() => (document.activeElement as HTMLElement).blur());
    expect(nav).toHaveAttribute("data-expanded", "false");
  });

  it("hover: stays expanded while the behaviour menu is open, even after pointerleave", async () => {
    const { nav } = await renderRail("hover");
    fireEvent.pointerEnter(nav);
    await sleep(HOVER_OPEN_DELAY_MS + 50);
    expect(nav).toHaveAttribute("data-expanded", "true");
    act(() => screen.getByRole("button", { name: "Sidebar behaviour" }).click());
    const menu = await screen.findByRole("menu");
    // The pointer moves into the portalled popup: the rail sees a leave, and focus is outside it.
    fireEvent.pointerLeave(nav);
    act(() => (document.activeElement as HTMLElement | null)?.blur());
    await sleep(HOVER_CLOSE_DELAY_MS + 100);
    expect(nav).toHaveAttribute("data-expanded", "true");
    expect(menu).toBeInTheDocument();
    expect(screen.getByRole("menuitemradio", { name: "Expand on hover" })).toBeInTheDocument();
  });

  it('marks only the current item with aria-current="page"', async () => {
    const current: Record<string, string> = {
      "/app": "Projects",
      "/app/api": "API reference",
      "/app/projects/p1": "Projects",
      "/app/members": "Members",
    };
    for (const [path, name] of Object.entries(current)) {
      const { nav } = await renderRail("expanded", path);
      const marked = nav.querySelectorAll('[aria-current="page"]');
      expect(marked, path).toHaveLength(1);
      expect(marked[0], path).toBe(screen.getByRole("link", { name }));
      expect(screen.getByRole("link", { name: "Chelabase Studio home" }), path).not.toHaveAttribute("aria-current");
      cleanup();
    }
  });

  it("the home link still navigates to /app on a plain click", async () => {
    const { router } = await renderRail("expanded", "/app/api");
    const user = userEvent.setup();
    await user.click(screen.getByRole("link", { name: "Chelabase Studio home" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/app"));
    expect(screen.getByRole("link", { name: "Chelabase Studio home" })).toHaveAttribute("href", "/app");
  });

  it("hover: a pointer press inside then pointerleave collapses after the grace period", async () => {
    const { nav } = await renderRail("hover");
    vi.useFakeTimers();
    fireEvent.pointerEnter(nav);
    act(() => vi.advanceTimersByTime(HOVER_OPEN_DELAY_MS));
    expect(nav).toHaveAttribute("data-expanded", "true");
    const projects = screen.getByRole("link", { name: "Projects" });
    fireEvent.pointerDown(projects);
    act(() => projects.focus());
    fireEvent.pointerUp(projects);
    fireEvent.pointerLeave(nav);
    act(() => vi.advanceTimersByTime(HOVER_CLOSE_DELAY_MS));
    expect(projects).toHaveFocus();
    expect(nav).toHaveAttribute("data-expanded", "false");
  });

  it("hover: keyboard focus still expands after an earlier pointer press", async () => {
    const { nav } = await renderRail("hover");
    const user = userEvent.setup();
    // A press on a non-focusable part of the rail leaves no focus event to consume it.
    fireEvent.pointerDown(nav);
    fireEvent.pointerUp(nav);
    await user.tab();
    expect(nav).toContainElement(document.activeElement as HTMLElement);
    expect(nav).toHaveAttribute("data-expanded", "true");
  });

  it("hover: switching away and back doesn't leave the rail stuck open", async () => {
    const { nav, setBehaviour } = await renderRail("hover");
    vi.useFakeTimers();
    fireEvent.pointerEnter(nav);
    act(() => vi.advanceTimersByTime(HOVER_OPEN_DELAY_MS));
    expect(nav).toHaveAttribute("data-expanded", "true");
    setBehaviour("collapsed");
    fireEvent.pointerLeave(nav);
    act(() => vi.advanceTimersByTime(1000));
    setBehaviour("hover");
    act(() => vi.advanceTimersByTime(1000));
    expect(nav).toHaveAttribute("data-expanded", "false");
  });

  it("placeholders are aria-disabled and not focusable", async () => {
    await renderRail("expanded");
    const placeholders = screen.getAllByText("Placeholder").map((label) => label.closest("[aria-disabled]"));
    expect(placeholders).toHaveLength(1);
    for (const placeholder of placeholders) {
      expect(placeholder).toHaveAttribute("aria-disabled", "true");
      expect(placeholder).not.toHaveAttribute("tabindex");
      expect(placeholder?.tagName).toBe("DIV");
    }
    const user = userEvent.setup();
    const visited: Element[] = [];
    for (let i = 0; i < 6; i++) {
      await user.tab();
      if (document.activeElement) visited.push(document.activeElement);
    }
    for (const placeholder of placeholders) {
      expect(visited.some((el) => placeholder?.contains(el))).toBe(false);
    }
    expect(visited).toContain(screen.getByRole("button", { name: "Sidebar behaviour" }));
  });

  it("rail_badge_counts_pending: the Members item carries the number of waiting requests", async () => {
    await renderRail("expanded", "/app", { members: 3 });
    const link = screen.getByRole("link", { name: "Members, 3 requests waiting" });
    expect(within(link).getByText("3")).toBeInTheDocument();
    cleanup();

    await renderRail("collapsed", "/app", { members: 1 });
    const collapsed = screen.getByRole("link", { name: "Members, 1 request waiting" });
    expect(collapsed.querySelector("[data-badge-dot]")).not.toBeNull();
    cleanup();

    await renderRail("expanded", "/app", { members: 0 });
    expect(screen.getByRole("link", { name: "Members" })).toBeInTheDocument();
  });
});
