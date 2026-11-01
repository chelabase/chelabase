import { act, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { presenceColor } from "@/lib/realtime/presence";
import { FakeRealtime, member } from "@/test/fake-realtime";
import { renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/**
 * Presence lives in a per-mount store (lib/realtime/presence-store.ts), not in RowsView's state: a presence frame
 * re-renders the presence layer and the avatar stack, never RowsView or the grid.
 *
 * The counts are commits of RowsView's and the grid's own bodies (a layout effect injected through `vi.mock`). A
 * `Profiler` around either would also fire for the presence layer, which the grid renders as its overlay.
 */
const commits = vi.hoisted(() => ({ rows: 0, grid: 0 }));

// `useColumnHeaders` is called once per render of RowsView's data view (RowsData) and nowhere else.
vi.mock("./column-headers", async (importOriginal) => {
  const m = await importOriginal<typeof import("./column-headers")>();
  const { useLayoutEffect } = await import("react");
  return {
    ...m,
    useColumnHeaders: (...args: Parameters<typeof m.useColumnHeaders>) => {
      useLayoutEffect(() => void commits.rows++);
      return m.useColumnHeaders(...args);
    },
  };
});
// The grid's body is called from the wrapper, so its own state updates count too.
vi.mock("./grid", async (importOriginal) => {
  const m = await importOriginal<typeof import("./grid")>();
  const { useLayoutEffect } = await import("react");
  return {
    ...m,
    Grid: (props: Parameters<typeof m.Grid>[0]) => {
      useLayoutEffect(() => void commits.grid++);
      return m.Grid(props);
    },
  };
});

const key = (id: string) => JSON.stringify([id]);
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const outlineColor = (el: Element) => (el as HTMLElement).style.outlineColor;

function mount() {
  const rt = new FakeRealtime();
  return { rt, ...renderEditor({ realtime: rt }) };
}
/** The editor has joined; the server has said nothing yet (so no refetch races the counts). */
const join = (rt: FakeRealtime) => vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
const snapshot = () => ({ ...commits });

const ana = member({ id: "ana", sub: "u-ana", name: "Ana", color_seed: 2, state: { row: key("81c4-2"), column: "amount_cents", editing: false } });
const ravi = member({ id: "ravi", sub: "u-ravi", name: "Ravi", color_seed: 4, state: { row: key("3f9a-1"), column: "status", editing: false } });

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("presence stays out of RowsView's render path", () => {
  it("cursor_only_diff_commits_neither_rows_view_nor_grid", async () => {
    const { rt } = mount();
    await join(rt);
    await cell("45000");
    act(() => rt.presenceState([ana]));
    await screen.findByTestId("presence-cell");
    const before = snapshot();
    expect(before.rows).toBeGreaterThan(0);
    expect(before.grid).toBeGreaterThan(0);

    const moved = { ...ana, state: { ...ana.state, row: key("3f9a-1"), column: "status" } };
    act(() => rt.presenceDiff([moved], [ana]));
    const drawn = screen.getByTestId("presence-cell");
    expect(drawn).toHaveAttribute("data-row", key("3f9a-1"));
    expect(drawn).toHaveAttribute("data-column", "status");
    expect(snapshot()).toEqual(before);
  });

  it("join_and_leave_update_the_avatar_stack_without_a_grid_commit", async () => {
    const { rt } = mount();
    await join(rt);
    await cell("45000");
    act(() => rt.presenceState([ana]));
    expect(await screen.findByTestId("avatar-stack")).toHaveAttribute("aria-label", "1 person here");
    const before = snapshot();

    act(() => rt.presenceDiff([ravi]));
    expect(screen.getByTestId("avatar-stack")).toHaveAttribute("aria-label", "2 people here");
    act(() => rt.presenceDiff([], [ana]));
    expect(screen.getByTestId("avatar-stack")).toHaveAttribute("aria-label", "1 person here");
    expect(screen.getAllByTestId("avatar")).toHaveLength(1);
    expect(snapshot().grid).toBe(before.grid);
    expect(snapshot().rows).toBe(before.rows);
  });

  it("reconnecting_clears_presence_until_presence_state", async () => {
    const { rt } = mount();
    await join(rt);
    await cell("45000");
    act(() => rt.presenceState([ana, ravi]));
    expect(await screen.findAllByTestId("presence-cell")).toHaveLength(2);

    act(() => rt.status(false, "reconnecting"));
    await vi.waitFor(() => expect(screen.queryByTestId("presence-cell")).toBeNull());
    expect(screen.queryByTestId("avatar-stack")).toBeNull();
    // A diff before the rejoin's state adds only what it carries.
    act(() => rt.presenceDiff([], [ana]));
    expect(screen.queryByTestId("presence-cell")).toBeNull();

    act(() => rt.presenceState([ravi]));
    expect(await screen.findByTestId("presence-cell")).toHaveAttribute("data-member", "ravi");
    expect(screen.getByTestId("avatar-stack")).toHaveAttribute("aria-label", "1 person here");
  });

  it("editing_notice_reads_current_members", async () => {
    const { rt, user } = mount();
    await join(rt);
    const c = await cell("45000");
    act(() => rt.presenceState([ana]));
    await screen.findByTestId("presence-cell");
    // Ana starts editing after the grid last rendered: the notice reads the members when the editor opens.
    act(() => rt.presenceDiff([{ ...ana, state: { ...ana.state, editing: true } }], [ana]));
    await user.dblClick(c);
    expect(await screen.findByText("Ana is editing this cell")).toBeInTheDocument();
  });

  it("own_color_seed_sets_the_active_outline", async () => {
    const { rt, user } = mount();
    await join(rt);
    const c = await cell("45000");
    await user.click(c);
    expect(c.classList.contains("outline-sand")).toBe(true);
    // The viewer's own entry (`/v1/me` sub "u2") colours the outline; another member's never does.
    act(() => rt.presenceState([ana, member({ id: "me", sub: "u2", name: "Bo", color_seed: 5 })]));
    await vi.waitFor(() => expect(outlineColor(c)).toBe(presenceColor(5)));
    expect(c.classList.contains("outline-sand")).toBe(false);
    act(() => rt.presenceDiff([member({ id: "me", sub: "u2", name: "Bo", color_seed: 1 })], [member({ id: "me", sub: "u2" })]));
    await vi.waitFor(() => expect(outlineColor(c)).toBe(presenceColor(1)));
  });
});
