import { RouterProvider, createBrowserHistory, createRootRoute, createRoute, createRouter, Outlet } from "@tanstack/react-router";
import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fakeFetch } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { FULL, editorRoutes, stubLayout } from "@/test/table-editor-fixtures";
import { RouteDraftBlocker } from "./route-blocker";
import { TableEditor } from "./table-editor";

/**
 * RT1 F6 follow-up (coordinator, 2026-10-21): the router's own navigations (Back, Forward, a link away) ask about a
 * dirty row draft with the editor's dialog; one dialog per attempt, none when clean.
 */

function mount() {
  const root = createRootRoute({ component: () => <Outlet /> });
  const other = createRoute({ getParentRoute: () => root, path: "/other", component: () => <p>elsewhere</p> });
  const tables = createRoute({
    getParentRoute: () => root,
    path: "/t/$table",
    component: function Tables() {
      const { table } = tables.useParams();
      return (
        <TableEditor
          params={{ ...FULL, table }}
          leaveGuard={RouteDraftBlocker}
          // The test's own route tree isn't the registered one: navigate by path.
          navigate={(p) => void router.navigate({ href: `/t/${p.table ?? "invoices"}` })}
        />
      );
    },
  });
  // The browser history: Back and Forward reach the blockers through `popstate` (a memory history checks only pushes).
  // The entries are pushed through the router's history, so they carry its index (Back is told from Forward by it).
  window.history.pushState(null, "", "/other");
  const history = createBrowserHistory();
  history.push("/t/invoices");
  const router = createRouter({ routeTree: root.addChildren([other, tables]), history });
  renderWithQuery(<RouterProvider router={router} />, { fetch: fakeFetch(editorRoutes()) });
  return { router, user: userEvent.setup() };
}
type User = ReturnType<typeof mount>["user"];
const path = (router: ReturnType<typeof mount>["router"]) => router.state.location.pathname;

async function dirtyRow(user: User) {
  await user.click(await screen.findByRole("button", { name: "Open row 2" }));
  const panel = await screen.findByRole("complementary", { name: "Row" });
  await user.type(within(panel).getByLabelText("amount_cents"), "1");
  return panel;
}

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("the route blocker for a dirty row draft", () => {
  it("back_while_dirty_asks_and_keep_editing_stays_on_the_route_with_the_draft", async () => {
    const { router, user } = mount();
    await dirtyRow(user);
    act(() => router.history.back());
    const d = await screen.findByRole("dialog", { name: "Discard changes to row 2?" });
    await user.click(within(d).getByRole("button", { name: "Keep editing" }));
    await vi.waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(path(router)).toBe("/t/invoices");
    expect(within(screen.getByRole("complementary", { name: "Row" })).getByLabelText("amount_cents")).toHaveValue("450001");
  });

  it("back_while_dirty_then_discard_navigates", async () => {
    const { router, user } = mount();
    await dirtyRow(user);
    act(() => router.history.back());
    await user.click(within(await screen.findByRole("dialog", { name: "Discard changes to row 2?" })).getByRole("button", { name: "Discard" }));
    expect(await screen.findByText("elsewhere")).toBeInTheDocument();
    expect(path(router)).toBe("/other");
  });

  it("back_while_clean_does_not_ask", async () => {
    const { router, user } = mount();
    await user.click(await screen.findByRole("button", { name: "Open row 2" }));
    await screen.findByRole("complementary", { name: "Row" });
    act(() => router.history.back());
    expect(await screen.findByText("elsewhere")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closing_the_tab_while_dirty_is_asked_once_by_the_editors_own_guard_not_by_the_router_too", async () => {
    const { user } = mount();
    await dirtyRow(user);
    const unload = () => {
      const e = new Event("beforeunload", { cancelable: true });
      const prevent = vi.spyOn(e, "preventDefault");
      window.dispatchEvent(e);
      return prevent.mock.calls.length;
    };
    await vi.waitFor(() => expect(unload()).toBe(1));
  });

  it("a_store_guarded_table_switch_asks_once_not_twice", async () => {
    const { router, user } = mount();
    await dirtyRow(user);
    const list = await screen.findByRole("navigation", { name: "Tables" });
    await user.click(within(list).getByRole("button", { name: "customers" }));
    expect(await screen.findAllByRole("dialog")).toHaveLength(1);
    await user.click(within(screen.getByRole("dialog", { name: "Discard changes to row 2?" })).getByRole("button", { name: "Discard" }));
    await vi.waitFor(() => expect(path(router)).toBe("/t/customers"));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("a_second_back_after_keep_editing_asks_again", async () => {
    const { router, user } = mount();
    await dirtyRow(user);
    act(() => router.history.back());
    await screen.findByRole("dialog", { name: "Discard changes to row 2?" });
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Keep editing" }));
    // The blocked pop is undone by a forward step of its own: let it land first.
    await act(() => new Promise((r) => setTimeout(r, 20)));
    act(() => router.history.back());
    await user.click(within(await screen.findByRole("dialog", { name: "Discard changes to row 2?" })).getByRole("button", { name: "Keep editing" }));
    await vi.waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(path(router)).toBe("/t/invoices");
  });
});
