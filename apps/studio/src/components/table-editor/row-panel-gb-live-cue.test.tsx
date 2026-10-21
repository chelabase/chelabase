import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, ROWS, bodies, filterByTextKey, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/**
 * RT1 second Playwright pass, ruling 2 (user, 2026-10-21): the row panel marks the fields a live update changed.
 *
 * Pins for the coder (all inside the open row's panel, `complementary` named "Row"):
 * - Each field's wrapper (the `div` holding its label line, control and NULL switch) gets `data-field="<column>"`.
 * - Untouched field, server value changed by a live update while the panel is DIRTY: the field shows the new value
 *   (as today), its wrapper gets `data-changed="live"` and the visible text "Updated" (an element whose whole text is
 *   "Updated"). The marker is gone as soon as the viewer edits that field, and after a save, a discard (the panel
 *   remounts) or "Take theirs".
 * - Field the viewer has typed in (in the draft), whose server value then changed: the draft stays; the wrapper gets
 *   `data-changed="since-started"` and the visible text "Changed since you started" (the same words as the grid's
 *   marker and the changes panel's "This table changed since you started"), and its text content also carries
 *   "Now: <the server value>" ("Now: NULL" for NULL), as the grid's marker shows it on hover. A later live change of the
 *   same field updates the value. On Save the existing conflict check decides (the dialog "Changed by someone else").
 * - A field whose value the live update did not change carries no marker; neither does anything before an update, nor
 *   the fields a save of the viewer's own changed.
 * - Only a DIRTY panel is pinned. A clean panel's refresh may show the same marker or none: not tested.
 */

const Q = `${B}/console/query`;
const key = (id: string) => JSON.stringify([id]);
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
function mount() {
  const rt = new FakeRealtime();
  const data: Data = ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ realtime: rt, routes: { [`POST ${Q}`]: server(data) } });
  return { rt, data, ...s };
}
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");

const rowPanel = () => screen.getByRole("complementary", { name: "Row" });
const field = (name: string) => {
  const el = rowPanel().querySelector<HTMLElement>(`[data-field="${name}"]`);
  if (!el) throw new Error(`the row panel has no [data-field="${name}"] wrapper`);
  return el;
};
const marks = () => Array.from(rowPanel().querySelectorAll("[data-changed]")).map((el) => `${el.closest("[data-field]")?.getAttribute("data-field")}:${el.getAttribute("data-changed")}`);
const update = (rt: FakeRealtime, data: Data, patch: Record<string, unknown>) => {
  Object.assign(data[1], patch);
  rt.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
};
/** Opens row 2 and types into amount_cents (45000 becomes 450001): a dirty panel. */
async function dirtyPanel(s: ReturnType<typeof mount>) {
  await vi.waitFor(() => expect(s.rt.active.length).toBeGreaterThan(0));
  await s.user.click(await screen.findByRole("button", { name: "Open row 2" }));
  const panel = await screen.findByRole("complementary", { name: "Row" });
  await s.user.type(within(panel).getByLabelText("amount_cents"), "1");
  return panel;
}

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("ruling 2: the dirty row panel marks what a live update changed", () => {
  it("an_untouched_field_refreshes_and_is_marked_updated_and_the_edited_one_is_not", async () => {
    const s = mount();
    await dirtyPanel(s);
    expect(marks()).toEqual([]);
    update(s.rt, s.data, { memo: "changed live" });
    await vi.waitFor(() => expect(within(rowPanel()).getByLabelText("memo")).toHaveValue("changed live"));
    expect(field("memo")).toHaveAttribute("data-changed", "live");
    expect(within(field("memo")).getByText("Updated")).toBeInTheDocument();
    expect(marks()).toEqual(["memo:live"]);
    expect(within(rowPanel()).getByLabelText("amount_cents")).toHaveValue("450001");
  });

  it("a_typed_field_whose_server_value_changed_says_changed_since_you_started_with_the_new_value_and_keeps_the_draft", async () => {
    const s = mount();
    await dirtyPanel(s);
    update(s.rt, s.data, { amount_cents: 47000 });
    await vi.waitFor(() => expect(field("amount_cents")).toHaveAttribute("data-changed", "since-started"));
    expect(field("amount_cents")).toHaveTextContent("Changed since you started");
    expect(field("amount_cents")).toHaveTextContent("Now: 47000");
    expect(within(rowPanel()).getByLabelText("amount_cents")).toHaveValue("450001");
    expect(within(rowPanel()).queryByText("Updated")).toBeNull();
    expect(marks()).toEqual(["amount_cents:since-started"]);
  });

  it("both_kinds_at_once_and_a_second_change_of_the_typed_field_updates_its_value", async () => {
    const s = mount();
    await dirtyPanel(s);
    update(s.rt, s.data, { memo: "changed live", amount_cents: 47000 });
    await vi.waitFor(() => expect(marks().sort()).toEqual(["amount_cents:since-started", "memo:live"]));
    update(s.rt, s.data, { amount_cents: 48000 });
    await vi.waitFor(() => expect(field("amount_cents")).toHaveTextContent("Now: 48000"));
    expect(field("amount_cents")).not.toHaveTextContent("Now: 47000");
    expect(field("memo")).toHaveAttribute("data-changed", "live");
  });

  it("a_typed_field_changed_to_null_says_now_null", async () => {
    const s = mount();
    await dirtyPanel(s);
    update(s.rt, s.data, { amount_cents: null });
    await vi.waitFor(() => expect(field("amount_cents")).toHaveAttribute("data-changed", "since-started"));
    expect(field("amount_cents")).toHaveTextContent("Now: NULL");
  });

  it("GUARD_an_update_that_changes_nothing_in_the_panel_marks_nothing", async () => {
    const s = mount();
    await dirtyPanel(s);
    update(s.rt, s.data, { memo: "Q3 plan" });
    // the grid re-read the row; give the panel's read the same time
    await new Promise((r) => setTimeout(r, 50));
    expect(marks()).toEqual([]);
  });

  it("editing_a_live_marked_field_clears_its_marker", async () => {
    const s = mount();
    await dirtyPanel(s);
    update(s.rt, s.data, { memo: "changed live" });
    await vi.waitFor(() => expect(field("memo")).toHaveAttribute("data-changed", "live"));
    await s.user.type(within(rowPanel()).getByLabelText("memo"), "!");
    expect(field("memo")).not.toHaveAttribute("data-changed");
    expect(within(rowPanel()).queryByText("Updated")).toBeNull();
    expect(marks()).toEqual([]);
  });

  it("a_save_clears_every_marker_and_the_viewers_own_saved_fields_are_not_marked", async () => {
    const s = mount();
    await dirtyPanel(s);
    update(s.rt, s.data, { memo: "changed live" });
    await vi.waitFor(() => expect(field("memo")).toHaveAttribute("data-changed", "live"));
    await s.user.click(within(rowPanel()).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(s.f)).toHaveLength(1));
    await vi.waitFor(() => expect(within(rowPanel()).getByText("No changes")).toBeInTheDocument());
    expect(marks()).toEqual([]);
    expect(within(rowPanel()).queryByText("Updated")).toBeNull();
    // the refresh after the save has read the row again: still nothing
    await new Promise((r) => setTimeout(r, 50));
    expect(marks()).toEqual([]);
  });

  it("a_discard_leaves_a_fresh_panel_without_markers", async () => {
    const s = mount();
    await dirtyPanel(s);
    update(s.rt, s.data, { memo: "changed live", amount_cents: 47000 });
    await vi.waitFor(() => expect(marks()).toHaveLength(2));
    await s.user.click(within(rowPanel()).getByRole("button", { name: "Cancel" }));
    const d = await screen.findByRole("dialog", { name: "Discard changes to row 2?" });
    await s.user.click(within(d).getByRole("button", { name: "Discard" }));
    await vi.waitFor(() => expect(screen.queryByRole("complementary", { name: "Row" })).toBeNull());
    await s.user.click(await screen.findByRole("button", { name: "Open row 2" }));
    await screen.findByRole("complementary", { name: "Row" });
    expect(marks()).toEqual([]);
    expect(within(rowPanel()).getByLabelText("amount_cents")).toHaveValue("47000");
  });

  it("saving_a_since_started_field_still_goes_through_the_conflict_check", async () => {
    const s = mount();
    await dirtyPanel(s);
    update(s.rt, s.data, { amount_cents: 47000 });
    await vi.waitFor(() => expect(field("amount_cents")).toHaveAttribute("data-changed", "since-started"));
    await s.user.click(within(rowPanel()).getByRole("button", { name: "Save row" }));
    expect(await screen.findByRole("dialog", { name: "Changed by someone else" })).toBeInTheDocument();
  });
});
