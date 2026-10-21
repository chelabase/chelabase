import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { presenceColor } from "@/lib/realtime/presence";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime, member } from "@/test/fake-realtime";
import { B, ROWS, bodies, filterByTextKey, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/**
 * RT1 browser feedback, round 2 (2026-10-21), through the table editor.
 *
 * Pins for the coder:
 * - bug 1: the viewer's own colour is `presenceColor(color_seed)` of the presence entry whose `sub` is the viewer's
 *   (`/v1/me` sub "u2" here), passed to the Grid as `activeColor` (see grid-fz.test.tsx). No own entry (or no
 *   realtime): `activeColor` undefined, the sand class stays. Another member's entry never colours it. The colour
 *   follows the entry (a presence diff that changes my seed changes it).
 * - bug 2: the gutter's "Open row N" button toggles. `aria-pressed` is true while that row is open in an expanded
 *   row panel; clicking it then closes the panel (no "Row" complementary is left visible).
 * - bug 3: the cause is in rows-view.tsx: `openKey` is `panel.row` whenever `panel.kind === "row"`, but the row panel's
 *   only close control ("Collapse panel", panel-bits.tsx CollapseButton -> `setCollapsed(true)`) keeps `kind: "row"`
 *   and `row`, so the key stays "open". "Open" means: row panel, not collapsed, that row. Expanding again re-marks it.
 *   (The row panel has no Escape handling; nothing to pin there.) A collapsed panel on a row is not "pressed"; clicking
 *   its gutter button expands the panel again.
 * - bug 4: see grid-fz.test.tsx; here, the real save path (conflict check included) on an outside pointer-down.
 */

const Q = `${B}/console/query`;
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const rowOf = (el: HTMLElement) => el.closest('[role="row"]') as HTMLElement;
const isOpenRow = async (text: string) => rowOf(await cell(text)).classList.contains("bg-channel");
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");
const activeCells = () => Array.from(document.querySelectorAll('[data-active="true"]'));
const outlineColor = (el: Element) => (el as HTMLElement).style.outlineColor;
/** The value jsdom stores for a colour (a hex literal comes back as `rgb(...)`, a `var(--x)` unchanged). */
const asStyle = (color: string) => {
  const probe = document.createElement("div");
  probe.style.outlineColor = color;
  return probe.style.outlineColor;
};

type Data = Record<string, unknown>[];
function matches(row: Record<string, unknown>, w: any): boolean {
  if (!w) return true;
  if (w.and) return w.and.every((p: unknown) => matches(row, p));
  if ("is" in w) return (row[w.col] ?? null) === w.is;
  if ("eq" in w) return row[w.col] !== null && row[w.col] !== undefined && String(row[w.col]) === w.eq;
  return true;
}
function server(data: Data): FakeHandler {
  return async (req) => {
    const { tree } = await req.clone().json();
    if (tree.op === "select") return jsonResponse(filterByTextKey(tree.where, data));
    if (tree.op === "update") {
      const hit = data.filter((r) => matches(r, tree.where));
      for (const r of hit) Object.assign(r, tree.set);
      return jsonResponse(hit);
    }
    return jsonResponse(data);
  };
}
function mount(o: { realtime?: boolean } = {}) {
  const rt = o.realtime ? new FakeRealtime() : undefined;
  const data: Data = ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ realtime: rt, routes: { [`POST ${Q}`]: server(data) } });
  return { rt, data, ...s };
}
const live = async (rt: FakeRealtime) => {
  await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
  rt.joined();
};
const openRow = async (user: ReturnType<typeof mount>["user"], n: number) => user.click(await screen.findByRole("button", { name: `Open row ${n}` }));
const openButton = (n: number) => screen.getByRole("button", { name: `Open row ${n}` });
const rowPanel = () => screen.queryByRole("complementary", { name: "Row" });

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("bug 1: the active cell takes the viewer's own presence colour", () => {
  const me = (seed: number) => member({ id: "me", sub: "u2", name: "Bo", color_seed: seed });
  const ana = member({ id: "ana", sub: "u-ana", name: "Ana", color_seed: 6 });

  it("a_cell_selected_by_a_member_whose_seed_is_the_slot_6_hue_is_outlined_in_it", async () => {
    const { rt, user } = mount({ realtime: true });
    await live(rt!);
    const c = await cell("45000");
    rt!.presenceState([me(6)]);
    expect(presenceColor(6)).toBe("#e08ad6");
    await user.click(c);
    await vi.waitFor(() => expect(outlineColor(c)).toBe(asStyle("#e08ad6")));
    expect(c).toHaveAttribute("data-active", "true");
    expect(c.classList.contains("outline-sand")).toBe(false);
    expect(c.classList.contains("outline-2")).toBe(true);
  });

  it("the_colour_comes_from_my_entry_not_from_another_members", async () => {
    const { rt, user } = mount({ realtime: true });
    await live(rt!);
    const c = await cell("45000");
    rt!.presenceState([ana, me(3)]);
    await user.click(c);
    await vi.waitFor(() => expect(outlineColor(c)).toBe(presenceColor(3)));
    expect(presenceColor(3)).not.toBe(presenceColor(6));
  });

  it("without_my_entry_yet_the_outline_stays_sand_and_takes_the_colour_once_it_arrives", async () => {
    const { rt, user } = mount({ realtime: true });
    await live(rt!);
    const c = await cell("45000");
    rt!.presenceState([ana]);
    await user.click(c);
    expect(c).toHaveAttribute("data-active", "true");
    expect(c.classList.contains("outline-sand")).toBe(true);
    expect(outlineColor(c)).toBe("");
    rt!.presenceDiff([me(2)]);
    await vi.waitFor(() => expect(outlineColor(c)).toBe(presenceColor(2)));
    expect(c.classList.contains("outline-sand")).toBe(false);
  });

  it("a_diff_that_changes_my_seed_changes_the_outline", async () => {
    const { rt, user } = mount({ realtime: true });
    await live(rt!);
    const c = await cell("45000");
    rt!.presenceState([me(6)]);
    await user.click(c);
    await vi.waitFor(() => expect(outlineColor(c)).toBe(asStyle(presenceColor(6))));
    rt!.presenceDiff([me(1)], [me(6)]);
    await vi.waitFor(() => expect(outlineColor(c)).toBe(presenceColor(1)));
  });

  it("without_realtime_the_outline_is_sand", async () => {
    const { user } = mount();
    const c = await cell("45000");
    await user.click(c);
    expect(c).toHaveAttribute("data-active", "true");
    expect(c.classList.contains("outline-sand")).toBe(true);
    expect(outlineColor(c)).toBe("");
  });
});

describe("bug 2: the gutter's open-row button toggles the row panel", () => {
  it("a_second_click_closes_the_panel_and_the_button_is_no_longer_pressed", async () => {
    const { user } = mount();
    await openRow(user, 2);
    expect(await screen.findByRole("complementary", { name: "Row" })).toBeInTheDocument();
    expect(openButton(2)).toHaveAttribute("aria-pressed", "true");
    expect(openButton(1)).toHaveAttribute("aria-pressed", "false");
    await user.click(openButton(2));
    expect(rowPanel()).toBeNull();
    expect(openButton(2)).toHaveAttribute("aria-pressed", "false");
    expect(await isOpenRow("45000")).toBe(false);
  });

  it("another_rows_button_switches_the_panel_instead_of_closing_it", async () => {
    const { user } = mount();
    await openRow(user, 2);
    await screen.findByRole("complementary", { name: "Row" });
    await user.click(openButton(3));
    expect(await screen.findByRole("complementary", { name: "Row" })).toBeInTheDocument();
    expect(openButton(3)).toHaveAttribute("aria-pressed", "true");
    expect(openButton(2)).toHaveAttribute("aria-pressed", "false");
    expect(await isOpenRow("8900")).toBe(true);
    expect(await isOpenRow("45000")).toBe(false);
  });

  it("a_closed_row_opens_again_with_the_same_button", async () => {
    const { user } = mount();
    await openRow(user, 2);
    await screen.findByRole("complementary", { name: "Row" });
    await user.click(openButton(2));
    expect(rowPanel()).toBeNull();
    await user.click(openButton(2));
    expect(await screen.findByRole("complementary", { name: "Row" })).toBeInTheDocument();
    expect(openButton(2)).toHaveAttribute("aria-pressed", "true");
  });

  it("on_a_collapsed_panel_the_row_is_not_pressed_and_the_button_expands_the_panel", async () => {
    const { user } = mount();
    await openRow(user, 2);
    const panel = await screen.findByRole("complementary", { name: "Row" });
    await user.click(within(panel).getByRole("button", { name: "Collapse panel" }));
    expect(openButton(2)).toHaveAttribute("aria-pressed", "false");
    await user.click(openButton(2));
    expect(await screen.findByRole("complementary", { name: "Row" })).toBeInTheDocument();
    expect(openButton(2)).toHaveAttribute("aria-pressed", "true");
  });
});

describe("bug 3: closing the panel by any path clears the row's open styling", () => {
  it("the_panels_own_close_control_clears_it_even_after_clicking_elsewhere", async () => {
    const { user } = mount();
    await openRow(user, 2);
    const panel = await screen.findByRole("complementary", { name: "Row" });
    expect(await isOpenRow("45000")).toBe(true);
    await user.click(within(panel).getByRole("button", { name: "Collapse panel" }));
    expect(rowPanel()).toBeNull();
    expect(await isOpenRow("45000")).toBe(false);
    await user.click(await cell("Q3 plan"));
    await user.click(document.body);
    expect(await isOpenRow("45000")).toBe(false);
  });

  it("the_gutter_toggle_clears_it", async () => {
    const { user } = mount();
    await openRow(user, 2);
    await screen.findByRole("complementary", { name: "Row" });
    expect(await isOpenRow("45000")).toBe(true);
    await user.click(openButton(2));
    expect(await isOpenRow("45000")).toBe(false);
  });

  it("expanding_the_collapsed_panel_marks_the_row_again", async () => {
    const { user } = mount();
    await openRow(user, 2);
    const panel = await screen.findByRole("complementary", { name: "Row" });
    await user.click(within(panel).getByRole("button", { name: "Collapse panel" }));
    expect(await isOpenRow("45000")).toBe(false);
    await user.click(screen.getByRole("button", { name: "Expand panel" }));
    await screen.findByRole("complementary", { name: "Row" });
    expect(await isOpenRow("45000")).toBe(true);
  });
});

describe("bug 4: an outside pointer-down while editing, through the real save path", () => {
  it("saves_a_changed_value_with_the_conflict_check_then_closes_the_editor_and_clears_the_cell", async () => {
    const { f, user } = mount();
    const c = await cell("Q3 plan"); // an open editor renames the cell: keep the element
    await user.dblClick(c);
    const input = await screen.findByRole("textbox", { name: "memo" });
    await user.clear(input);
    await user.type(input, "Q4 plan");
    await user.click(document.body);
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    const w = (await writes(f))[0].tree;
    expect(w.op).toBe("update");
    expect(w.set).toEqual({ memo: "Q4 plan" });
    expect(w.where).toEqual({ and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "memo", eq: "Q3 plan", text: true }] });
    await vi.waitFor(() => expect(screen.queryByRole("textbox", { name: "memo" })).toBeNull());
    expect(activeCells()).toEqual([]);
  });

  it("an_unchanged_editor_closes_without_a_write_and_clears_the_cell", async () => {
    const { f, user } = mount();
    const c = await cell("Q3 plan");
    await user.dblClick(c);
    await screen.findByRole("textbox", { name: "memo" });
    await user.click(document.body);
    await vi.waitFor(() => expect(screen.queryByRole("textbox", { name: "memo" })).toBeNull());
    expect(await writes(f)).toHaveLength(0);
    expect(activeCells()).toEqual([]);
  });

  it("opening_the_row_panel_from_the_gutter_while_editing_keeps_the_active_cell", async () => {
    const { user } = mount();
    const c = await cell("Q3 plan");
    await user.dblClick(c);
    await screen.findByRole("textbox", { name: "memo" });
    await user.click(openButton(2));
    await screen.findByRole("complementary", { name: "Row" });
    expect(activeCells()).toHaveLength(1);
  });
});
