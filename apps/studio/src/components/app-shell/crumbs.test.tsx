import { RouterProvider, createMemoryHistory, createRootRoute, createRoute, createRouter, useParams } from "@tanstack/react-router";
import { act, cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { memo, useState } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { mockMatchMedia } from "@/test/match-media";
import { mockTrailLayout } from "@/test/trail-layout";
import { renderWithCrumbs } from "@/test/render-crumbs";
import { renderWithQuery } from "@/test/render";
import { Breadcrumbs, useCrumbLabel } from "./crumbs";
import { TABLES_ROUTE } from "./rail-items";

const DB = "11111111-1111-4111-8111-111111111111";
const BR = "22222222-2222-4222-8222-222222222222";

const PROJECT = { path: "/app/projects/$projectId", at: "/app/projects/p1" };
const MEMBER = { path: "/app/members/$memberId", at: "/app/members/m1" };
const ROLE = (role: string) => ({
  path: "/app/members/roles/$databaseId/$branchId/$role",
  at: `/app/members/roles/${DB}/${BR}/${encodeURIComponent(role)}`,
});
const MERGE = { path: "/app/projects/$projectId/databases/$databaseId/merges/$mergeId", at: "/app/projects/p1/databases/d1/merges/m1" };

function Label({ param, id, label }: { param: string; id: string | undefined; label: string | undefined }) {
  useCrumbLabel(param, id, label);
  return null;
}

const MERGE_P1 = { path: MERGE.path, at: "/app/projects/p1/databases/d1/merges/m1" };
const nav = () => screen.getByRole("navigation", { name: "Breadcrumb" });
const items = () => within(nav()).getAllByRole("listitem");
const hrefOf = (el: HTMLElement) => new URL(el.getAttribute("href") ?? "", "http://x");

let media: ReturnType<typeof mockMatchMedia> | undefined;
let layout: ReturnType<typeof mockTrailLayout> | undefined;
afterEach(() => {
  cleanup();
  media?.restore();
  media = undefined;
  layout?.restore();
  layout = undefined;
});
/** Narrow, and the trail doesn't fit (it needs 320 px of the 200 it has) unless `need` says otherwise. */
const goNarrow = (need = 320) => {
  layout = mockTrailLayout({ nav: 200, need });
  media = mockMatchMedia((q) => q.includes("width < 40rem"));
};

describe("Breadcrumbs (wide)", () => {
  it("is a nav named Breadcrumb with an ordered list; separators are decorative", async () => {
    renderWithCrumbs(<Label param="projectId" id="p1" label="Acme" />, PROJECT);
    expect(await screen.findByRole("navigation", { name: "Breadcrumb" })).toBeInTheDocument();
    expect(within(nav()).getByRole("list").tagName).toBe("OL");
    expect(items().map((li) => li.textContent?.replace(/\s+/g, ""))).toEqual(["Projects", "Acme"]);
    for (const svg of nav().querySelectorAll("svg")) expect(svg).toHaveAttribute("aria-hidden", "true");
  });

  it("every crumb but the last is a link; the last has aria-current=page and truncates; only it", async () => {
    renderWithCrumbs(<Label param="projectId" id="p1" label="Acme" />, PROJECT);
    const projects = await screen.findByRole("link", { name: "Projects" });
    expect(hrefOf(projects).pathname).toBe("/app");
    expect(projects).not.toHaveAttribute("aria-current");
    const last = await within(nav()).findByText("Acme");
    expect(last).toHaveAttribute("aria-current", "page");
    expect(last.closest("a")).toBeNull();
    expect(last.className).toContain("truncate");
    expect(nav().querySelectorAll("[aria-current]")).toHaveLength(1);
  });

  it("long names keep the full text in title", async () => {
    const long = "x".repeat(200);
    renderWithCrumbs(<Label param="projectId" id="p1" label={long} />, PROJECT);
    const last = await within(await screen.findByRole("navigation", { name: "Breadcrumb" })).findByText(long);
    expect(last).toHaveAttribute("title", long);
  });

  it("shows '…' (not the raw id) while there is no label, and the crumb is not a link", async () => {
    renderWithCrumbs(<p>page</p>, PROJECT);
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(within(n).getByText("…")).toBeInTheDocument();
    expect(within(n).queryByText("p1")).toBeNull();
    expect(within(n).getAllByRole("link").map((a) => a.textContent)).toEqual(["Projects"]);
  });

  it("a label registered by a page appears, and goes when the page unmounts it", async () => {
    const user = userEvent.setup();
    function Page() {
      const [on, setOn] = useState(true);
      return (
        <>
          <button onClick={() => setOn(false)}>drop</button>
          {on && <Label param="projectId" id="p1" label="Acme" />}
        </>
      );
    }
    renderWithCrumbs(<Page />, PROJECT);
    expect(await within(await screen.findByRole("navigation", { name: "Breadcrumb" })).findByText("Acme")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "drop" }));
    await waitFor(() => expect(within(nav()).queryByText("Acme")).toBeNull());
    expect(within(nav()).getByText("…")).toBeInTheDocument();
  });

  it("a label that changes (rename) updates the crumb", async () => {
    const user = userEvent.setup();
    function Page() {
      const [n, setN] = useState("Acme");
      return (
        <>
          <button onClick={() => setN("Acme 2")}>rename</button>
          <Label param="projectId" id="p1" label={n} />
        </>
      );
    }
    renderWithCrumbs(<Page />, PROJECT);
    await within(await screen.findByRole("navigation", { name: "Breadcrumb" })).findByText("Acme");
    await user.click(screen.getByRole("button", { name: "rename" }));
    expect(await within(nav()).findByText("Acme 2")).toBeInTheDocument();
  });

  it("an empty label or an undefined id registers nothing", async () => {
    renderWithCrumbs(
      <>
        <Label param="projectId" id="p1" label="" />
        <Label param="projectId" id={undefined} label="Ghost" />
      </>,
      PROJECT,
    );
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(within(n).getByText("…")).toBeInTheDocument();
    expect(within(n).queryByText("Ghost")).toBeNull();
  });

  it("a label for another id never shows (S1): navigating A to B shows '…' at once", async () => {
    const { router } = renderWithCrumbs(<Label param="projectId" id="pA" label="Project A" />, { path: "/app/projects/$projectId", at: "/app/projects/pA" });
    expect(await within(await screen.findByRole("navigation", { name: "Breadcrumb" })).findByText("Project A")).toBeInTheDocument();
    await act(async () => {
      await router.navigate({ to: "/app/projects/$projectId", params: { projectId: "pB" } } as never);
    });
    expect(within(nav()).queryByText("Project A")).toBeNull();
    expect(within(nav()).getByText("…")).toBeInTheDocument();
  });

  it("the member crumb of one member never shows another member's name", async () => {
    const { router } = renderWithCrumbs(<Label param="memberId" id="m1" label="Ada" />, MEMBER);
    await within(await screen.findByRole("navigation", { name: "Breadcrumb" })).findByText("Ada");
    await act(async () => {
      await router.navigate({ to: "/app/members/$memberId", params: { memberId: "m2" } } as never);
    });
    expect(within(nav()).queryByText("Ada")).toBeNull();
  });

  it("the Members crumb on a member page links to plain /app/members", async () => {
    renderWithCrumbs(<Label param="memberId" id="m1" label="Ada" />, MEMBER);
    const members = await screen.findByRole("link", { name: "Members" });
    const u = hrefOf(members);
    expect(u.pathname).toBe("/app/members");
    expect(u.search).toBe("");
  });

  it("a role named 'a/b%c é<x>' works end to end: label matches the router's param, links are encoded once (S2)", async () => {
    const role = "a/b%c é<x>";
    function Page() {
      const p = useParams({ strict: false }) as { role?: string };
      return <Label param="role" id={p.role} label={p.role} />;
    }
    renderWithCrumbs(<Page />, ROLE(role));
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(await within(n).findByText(role)).toHaveAttribute("aria-current", "page");
    const roles = within(n).getByRole("link", { name: "Roles" });
    const u = hrefOf(roles);
    expect(u.pathname).toBe("/app/members");
    expect(u.searchParams.get("tab")).toBe("roles");
    expect(u.searchParams.get("database")).toBe(DB);
    expect(u.searchParams.get("branch")).toBe(BR);
    expect(hrefOf(within(n).getByRole("link", { name: "Members" })).search).toBe("");
  });

  it("the merge trail: project is a link encoded once, the database is text, Merge is last", async () => {
    const id = "a/b%c é";
    renderWithCrumbs(
      <>
        <Label param="projectId" id={id} label="Acme" />
        <Label param="databaseId" id="d1" label="main-db" />
      </>,
      { path: MERGE.path, at: `/app/projects/${encodeURIComponent(id)}/databases/d1/merges/m1` },
    );
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    const proj = await within(n).findByRole("link", { name: "Acme" });
    expect(proj.getAttribute("href")).toBe(`/app/projects/${encodeURIComponent(id)}`);
    const db = await within(n).findByText("main-db");
    expect(db.closest("a")).toBeNull();
    expect(db).not.toHaveAttribute("aria-current");
    expect(within(n).getByText("Merge")).toHaveAttribute("aria-current", "page");
    expect(within(n).getAllByRole("link").map((a) => a.textContent)).toEqual(["Projects", "Acme"]);
  });

  it("the Tables route: zero params is one Tables crumb; with a table, Tables links to the schema list (S6)", async () => {
    const view = renderWithCrumbs(<p>p</p>, { path: TABLES_ROUTE, at: "/app/tables" });
    const n0 = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(items()).toHaveLength(1);
    expect(within(n0).getByText("Tables")).toHaveAttribute("aria-current", "page");
    view.unmount();

    renderWithCrumbs(<Label param="table" id="users" label="users" />, { path: TABLES_ROUTE, at: "/app/tables/p1/d1/b1/postgres/public/users" });
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(await within(n).findByText("users")).toHaveAttribute("aria-current", "page");
    const tables = within(n).getByRole("link", { name: "Tables" });
    expect(tables.getAttribute("href")).toBe("/app/tables/p1/d1/b1/postgres/public");
  });

  it("the Tables route with a table not yet loaded shows '…' after the link", async () => {
    renderWithCrumbs(<p>p</p>, { path: TABLES_ROUTE, at: "/app/tables/p1/d1/b1/postgres/public/users" });
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(within(n).getByText("…")).toBeInTheDocument();
    expect(within(n).getByRole("link", { name: "Tables" })).toBeInTheDocument();
  });

  it("an unknown path renders no crumbs", async () => {
    renderWithCrumbs(<p>home</p>, { path: "/", at: "/" });
    await screen.findByText("home");
    expect(screen.queryAllByRole("listitem")).toHaveLength(0);
  });

  it("/app/ (the index route's pathname) shows Projects", async () => {
    renderWithCrumbs(<p>home</p>, { path: "/app/", at: "/app/" });
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(within(n).getByText("Projects")).toHaveAttribute("aria-current", "page");
  });
});

describe("label provider (S8, no provider)", () => {
  it("registering a label doesn't re-render a sibling that only calls the hook", async () => {
    const user = userEvent.setup();
    const renders = { a: 0 };
    const A = memo(function A() {
      renders.a++;
      useCrumbLabel("projectId", "p1", "Acme");
      return <p>a</p>;
    });
    function Page() {
      const [showB, setShowB] = useState(false);
      return (
        <>
          <button onClick={() => setShowB(true)}>add</button>
          <A />
          {showB && <Label param="databaseId" id="d1" label="Db" />}
        </>
      );
    }
    renderWithCrumbs(<Page />, MERGE_P1);
    await within(await screen.findByRole("navigation", { name: "Breadcrumb" })).findByText("Acme");
    expect(renders.a).toBe(1);
    await user.click(screen.getByRole("button", { name: "add" }));
    await within(nav()).findByText("Db");
    expect(renders.a).toBe(1);
  });

  it("without a provider the hook is a no-op and Breadcrumbs shows dynamic crumbs as '…'", async () => {
    function Page() {
      useCrumbLabel("projectId", "p1", "Acme");
      return <p>page</p>;
    }
    const root = createRootRoute({
      component: () => (
        <>
          <Breadcrumbs />
          <Page />
        </>
      ),
    });
    const route = createRoute({ getParentRoute: () => root, path: "/app/projects/$projectId", component: () => null });
    const router = createRouter({ routeTree: root.addChildren([route]), history: createMemoryHistory({ initialEntries: ["/app/projects/p1"] }) });
    renderWithQuery(<RouterProvider router={router} />);
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    expect(within(n).getByText("…")).toBeInTheDocument();
    expect(within(n).queryByText("Acme")).toBeNull();
  });
});

describe("Breadcrumbs (narrow, Q3)", () => {
  it("a 2-crumb trail never collapses", async () => {
    goNarrow();
    renderWithCrumbs(<Label param="projectId" id="p1" label="Acme" />, PROJECT);
    await within(await screen.findByRole("navigation", { name: "Breadcrumb" })).findByText("Acme");
    expect(screen.queryByRole("button", { name: "Show hidden breadcrumbs" })).toBeNull();
    expect(items()).toHaveLength(2);
  });

  it("a 3-crumb trail keeps first and last and hides the middle one behind a menu button", async () => {
    goNarrow();
    renderWithCrumbs(<Label param="role" id="app_user" label="app_user" />, ROLE("app_user"));
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    const button = await within(n).findByRole("button", { name: "Show hidden breadcrumbs" });
    expect(button).toHaveAttribute("aria-haspopup", "menu");
    expect(button).toHaveAttribute("aria-expanded", "false");
    expect(within(n).getByRole("link", { name: "Members" })).toBeInTheDocument();
    expect(within(n).getByText("app_user")).toHaveAttribute("aria-current", "page");
    expect(within(n).queryByRole("link", { name: "Roles" })).toBeNull();
  });

  it("the menu lists the hidden crumbs as links, in order, and Escape closes it and returns focus", async () => {
    goNarrow();
    const user = userEvent.setup();
    renderWithCrumbs(
      <>
        <Label param="projectId" id="p1" label="Acme" />
        <Label param="databaseId" id="d1" label="main-db" />
      </>,
      MERGE,
    );
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    const button = await within(n).findByRole("button", { name: "Show hidden breadcrumbs" });
    await within(n).findByText("Merge");
    expect(within(n).getByRole("link", { name: "Projects" })).toBeInTheDocument();
    expect(within(n).queryByText("Acme")).toBeNull();

    await user.click(button);
    const menuItems = await screen.findAllByRole("menuitem");
    expect(menuItems.map((m) => m.textContent)).toEqual(["Acme", "main-db"]);
    expect(menuItems[0].getAttribute("href")).toBe("/app/projects/p1");
    expect(menuItems[1].getAttribute("href")).toBeNull(); // the database crumb is text (S4)
    expect(button).toHaveAttribute("aria-expanded", "true");

    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryAllByRole("menuitem")).toHaveLength(0));
    expect(button).toHaveFocus();
    expect(button).toHaveAttribute("aria-expanded", "false");
  });

  it("a loading crumb among the hidden ones is a non-link item named Loading, its '…' hidden from assistive tech", async () => {
    goNarrow();
    const user = userEvent.setup();
    renderWithCrumbs(<Label param="databaseId" id="d1" label="main-db" />, MERGE);
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    const button = await within(n).findByRole("button", { name: "Show hidden breadcrumbs" });
    await user.click(button);
    const menuItems = await screen.findAllByRole("menuitem");
    expect(menuItems.map((m) => m.textContent)).toEqual(expect.arrayContaining(["main-db"]));
    const loading = screen.getByRole("menuitem", { name: "Loading" });
    expect(loading.getAttribute("href")).toBeNull();
    const dots = within(loading).getByText("…");
    expect(dots).toHaveAttribute("aria-hidden", "true");
    // The trigger is never confused with a loading crumb.
    expect(button).toHaveAccessibleName("Show hidden breadcrumbs");
  });

  it("the last crumb ellipsises at narrow width too", async () => {
    goNarrow();
    renderWithCrumbs(<Label param="role" id="r" label="r" />, ROLE("r"));
    const last = await within(await screen.findByRole("navigation", { name: "Breadcrumb" })).findByText("r");
    expect(last.className).toContain("truncate");
  });

  it("switching to wide shows every crumb and no button; switching back collapses again", async () => {
    goNarrow();
    renderWithCrumbs(<Label param="role" id="r" label="r" />, ROLE("r"));
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    await within(n).findByRole("button", { name: "Show hidden breadcrumbs" });
    act(() => media!.set(() => false));
    expect(within(nav()).queryByRole("button", { name: "Show hidden breadcrumbs" })).toBeNull();
    expect(within(nav()).getByRole("link", { name: "Roles" })).toBeInTheDocument();
    expect(items()).toHaveLength(3);
    act(() => media!.set((q) => q.includes("width < 40rem")));
    expect(within(nav()).getByRole("button", { name: "Show hidden breadcrumbs" })).toBeInTheDocument();
  });
});

describe("Breadcrumbs (narrow, fit measured)", () => {
  it("a trail that fits is shown whole", async () => {
    goNarrow(200);
    renderWithCrumbs(<Label param="role" id="billing_editor" label="billing_editor" />, ROLE("billing_editor"));
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    await within(n).findByText("billing_editor");
    expect(within(n).queryByRole("button", { name: "Show hidden breadcrumbs" })).toBeNull();
    expect(within(n).getByRole("link", { name: "Roles" })).toBeInTheDocument();
  });

  it("collapses when the nav shrinks below what the trail needs, and expands when it grows back", async () => {
    goNarrow(200);
    renderWithCrumbs(<Label param="role" id="r" label="r" />, ROLE("r"));
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    await within(n).findByText("r");
    expect(within(n).queryByRole("button", { name: "Show hidden breadcrumbs" })).toBeNull();
    act(() => {
      layout!.set({ nav: 150, need: 220 });
      layout!.fire();
    });
    expect(within(nav()).getByRole("button", { name: "Show hidden breadcrumbs" })).toBeInTheDocument();
    act(() => {
      layout!.set({ nav: 260 });
      layout!.fire();
    });
    expect(within(nav()).queryByRole("button", { name: "Show hidden breadcrumbs" })).toBeNull();
    expect(within(nav()).getByRole("link", { name: "Roles" })).toBeInTheDocument();
  });
  it("a long last crumb does not collapse the middle while it still has its minimum (8rem)", async () => {
    goNarrow();
    layout!.restore();
    layout = mockTrailLayout({ nav: 300, need: 500, lastClient: 140 });
    renderWithCrumbs(<Label param="role" id="analytics_rep_long_name" label="analytics_rep_long_name" />, ROLE("analytics_rep_long_name"));
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    await within(n).findByText("analytics_rep_long_name");
    expect(within(n).queryByRole("button", { name: "Show hidden breadcrumbs" })).toBeNull();
    expect(within(n).getByRole("link", { name: "Roles" })).toBeInTheDocument();
  });

  it("collapses when even the last crumb at its minimum does not fit", async () => {
    goNarrow();
    layout!.restore();
    layout = mockTrailLayout({ nav: 300, need: 500, lastClient: 60 });
    renderWithCrumbs(<Label param="role" id="analytics_rep_long_name" label="analytics_rep_long_name" />, ROLE("analytics_rep_long_name"));
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    await within(n).findByText("analytics_rep_long_name");
    expect(within(n).getByRole("button", { name: "Show hidden breadcrumbs" })).toBeInTheDocument();
  });
});

describe("Breadcrumbs (final fix round)", () => {
  it("middle crumbs with dynamic labels can shrink; static ones keep their width; links carry a title", async () => {
    renderWithCrumbs(
      <>
        <Label param="projectId" id="p1" label="Acme" />
        <Label param="databaseId" id="d1" label="main-db" />
      </>,
      MERGE,
    );
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    const projects = await within(n).findByRole("link", { name: "Projects" });
    const acme = await within(n).findByRole("link", { name: "Acme" });
    expect(projects).toHaveAttribute("title", "Projects");
    expect(acme).toHaveAttribute("title", "Acme");
    expect(projects.closest("li")!.className).toContain("shrink-0");
    expect(acme.closest("li")!.className).not.toContain("shrink-0");
    expect(acme.closest("li")!.className).toContain("min-w-0");
    expect(acme.className).toContain("truncate");
  });

  it("hidden plain-text and loading items are aria-disabled", async () => {
    goNarrow();
    const user = userEvent.setup();
    renderWithCrumbs(<Label param="databaseId" id="d1" label="main-db" />, MERGE);
    const n = await screen.findByRole("navigation", { name: "Breadcrumb" });
    await user.click(await within(n).findByRole("button", { name: "Show hidden breadcrumbs" }));
    const text = await screen.findByRole("menuitem", { name: "main-db" });
    const loading = screen.getByRole("menuitem", { name: "Loading" });
    expect(text).toHaveAttribute("aria-disabled", "true");
    expect(loading).toHaveAttribute("aria-disabled", "true");
  });
});
