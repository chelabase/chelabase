import {
  Outlet,
  RouterProvider,
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
} from "@tanstack/react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { AppHeader } from "./app-header";
import { AppShell } from "./app-shell";
import { CrumbLabelsProvider, useCrumbLabel } from "./crumbs";

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});

function ProjectLabel() {
  useCrumbLabel("projectId", "p1", "Acme");
  return <p>project body</p>;
}

async function renderHeaderAt(at: string) {
  const root = createRootRoute({
    component: () => (
      <CrumbLabelsProvider>
        <AppHeader profile={{ name: "Ada", email: "ada@x.dev" }} onSignOut={() => {}} />
        <Outlet />
      </CrumbLabelsProvider>
    ),
  });
  const api = createRoute({ getParentRoute: () => root, path: "/app/api", component: () => <p>api body</p> });
  const project = createRoute({ getParentRoute: () => root, path: "/app/projects/$projectId", component: ProjectLabel });
  const router = createRouter({ routeTree: root.addChildren([api, project]), history: createMemoryHistory({ initialEntries: [at] }) });
  render(
    <QueryClientProvider client={new QueryClient()}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return await screen.findByRole("banner");
}

describe("AppHeader crumbs (P5, S12)", () => {
  it("keeps a 52px first row; only below the breakpoint the context slot wraps to a second row (P5, amended)", async () => {
    const header = await renderHeaderAt("/app/api");
    expect(header.className).toContain("min-h-[52px]");
    expect(header.className).toContain("items-center");
    // Wide widths never wrap; the wrap exists only under 40rem.
    expect(header.className).not.toMatch(/(^|\s)flex-wrap/);
    expect(header.className).toContain("max-[40rem]:flex-wrap");
    expect(header.firstElementChild!.className).toContain("h-[52px]");
    const slot = header.querySelector("#app-header-context")!;
    expect(slot.className).toContain("max-[40rem]:order-last");
    expect(slot.className).toContain("max-[40rem]:overflow-x-auto");
    expect(slot.className).toContain("sm:has-[>*]:min-w-[20rem]");
    expect(slot.className).not.toMatch(/(^|\s)sm:min-w-\[20rem\]/);
  });

  it("lays out crumbs, then the context slot, then the account menu", async () => {
    const header = await renderHeaderAt("/app/api");
    const crumbs = await within(header).findByRole("navigation", { name: "Breadcrumb" });
    expect(within(crumbs).getByText("API")).toHaveAttribute("aria-current", "page");
    const slot = header.querySelector("#app-header-context");
    const account = within(header).getByRole("button", { name: "Ada" });
    expect(slot).not.toBeNull();
    expect(crumbs.compareDocumentPosition(slot!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(slot!.compareDocumentPosition(account) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("keeps the context slot's id and attribute", async () => {
    const header = await renderHeaderAt("/app/api");
    expect(header.querySelector("#app-header-context")).toHaveAttribute("data-slot", "context");
  });

  it("the crumbs shrink (min-w-0) so the slot and account menu keep their room", async () => {
    const header = await renderHeaderAt("/app/api");
    const crumbs = await within(header).findByRole("navigation", { name: "Breadcrumb" });
    expect(crumbs.className).toContain("min-w-0");
  });

  it("at narrow width the visible account name goes sr-only but stays the accessible name", async () => {
    const header = await renderHeaderAt("/app/api");
    const name = within(within(header).getByRole("button", { name: "Ada" })).getByText("Ada");
    expect(name.className).toContain("max-[40rem]:sr-only");
  });

  it("has no menu button unless onMenu is given", async () => {
    const header = await renderHeaderAt("/app/api");
    await within(header).findByRole("navigation", { name: "Breadcrumb" });
    expect(within(header).queryByRole("button", { name: "Menu" })).toBeNull();
  });

  it("shows a page's label in the header", async () => {
    const header = await renderHeaderAt("/app/projects/p1");
    expect(await within(header).findByText("Acme")).toHaveAttribute("aria-current", "page");
  });
});

describe("AppShell mounts the label provider", () => {
  it("a page's useCrumbLabel reaches the header crumbs", async () => {
    const root = createRootRoute();
    const layout = createRoute({
      getParentRoute: () => root,
      path: "/app",
      component: () => (
        <AppShell profile={{ name: "Ada" }} onSignOut={() => {}}>
          <Outlet />
        </AppShell>
      ),
    });
    const project = createRoute({ getParentRoute: () => layout, path: "/projects/$projectId", component: ProjectLabel });
    const router = createRouter({
      routeTree: root.addChildren([layout.addChildren([project])]),
      history: createMemoryHistory({ initialEntries: ["/app/projects/p1"] }),
    });
    render(
      <QueryClientProvider client={new QueryClient()}>
        <RouterProvider router={router} />
      </QueryClientProvider>,
    );
    const header = await screen.findByRole("banner");
    expect(await within(header).findByText("Acme")).toHaveAttribute("aria-current", "page");
    expect(within(header).getByRole("link", { name: "Projects" })).toHaveAttribute("href", "/app");
  });
});
