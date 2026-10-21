import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, ROWS, T, bodies, filterByTextKey, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/**
 * RT1 Playwright pass, F6 (coder's own, coordinator rulings 1-5 and the live-update ruling): every path that would
 * drop a dirty row-panel draft asks first; a live update keeps the draft's own fields and refreshes the others.
 */

const Q = `${B}/console/query`;
const key = (id: string) => JSON.stringify([id]);
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");

type Data = Record<string, unknown>[];
function matches(row: Record<string, unknown>, w: any): boolean {
  if (!w) return true;
  if (w.and) return w.and.every((p: unknown) => matches(row, p));
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
function mount(o: { realtime?: boolean; routes?: Record<string, FakeHandler> } = {}) {
  const rt = o.realtime ? new FakeRealtime() : undefined;
  const data: Data = ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ realtime: rt, routes: { [`POST ${Q}`]: server(data), ...o.routes } });
  return { rt, data, ...s };
}
type User = ReturnType<typeof mount>["user"];

const openRow = async (user: User, n: number) => {
  await user.click(await screen.findByRole("button", { name: `Open row ${n}` }));
  return screen.findByRole("complementary", { name: "Row" });
};
const dirty = async (user: User, panel: HTMLElement) => user.type(within(panel).getByLabelText("amount_cents"), "1");
const dialog = (title: string) => screen.findByRole("dialog", { name: title });
const answer = async (user: User, d: HTMLElement, name: "Discard" | "Keep editing") => user.click(within(d).getByRole("button", { name }));
const rowPanel = () => screen.queryByRole("complementary", { name: "Row" });
const amount = () => within(rowPanel()!).getByLabelText("amount_cents");

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("F6 ruling 1: Act as", () => {
  it("switching_role_over_a_dirty_panel_asks_keep_leaves_the_role_and_discard_switches", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    const actAs = screen.getByLabelText("Act as");
    await user.click(actAs);
    await user.click(await screen.findByRole("option", { name: "reporter" }));
    await answer(user, await dialog("Discard changes to row 2?"), "Keep editing");
    expect(actAs).not.toHaveValue("reporter");
    expect(amount()).toHaveValue("450001");
    await user.click(actAs);
    await user.click(await screen.findByRole("option", { name: "reporter" }));
    await answer(user, await dialog("Discard changes to row 2?"), "Discard");
    await vi.waitFor(() => expect(screen.getByLabelText("Act as")).toHaveValue("reporter"));
  });
});

describe("F6 ruling 2: Insert with all defaults", () => {
  it("asks_before_the_request_is_sent", async () => {
    const insert = vi.fn(() => jsonResponse({ row: { ...ROWS[0], id: "new-1" } }));
    const { user } = mount({ routes: { [`POST ${T}/rows/default`]: insert } });
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(screen.getByRole("button", { name: "More insert options" }));
    await user.click(await screen.findByRole("menuitem", { name: "Insert with all defaults" }));
    const d = await dialog("Discard changes to row 2?");
    expect(insert).not.toHaveBeenCalled();
    await answer(user, d, "Keep editing");
    expect(insert).not.toHaveBeenCalled();
    expect(amount()).toHaveValue("450001");
    await user.click(screen.getByRole("button", { name: "More insert options" }));
    await user.click(await screen.findByRole("menuitem", { name: "Insert with all defaults" }));
    await answer(user, await dialog("Discard changes to row 2?"), "Discard");
    await vi.waitFor(() => expect(insert).toHaveBeenCalledTimes(1));
  });
});

describe("F6 ruling 3: the column strip", () => {
  it("a_chip_over_a_dirty_panel_asks_and_discard_opens_the_column_form", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(within(screen.getByRole("list", { name: "Columns" })).getByRole("button", { name: /^memo / }));
    await answer(user, await dialog("Discard changes to row 2?"), "Keep editing");
    expect(amount()).toHaveValue("450001");
    expect(screen.queryByRole("complementary", { name: "Edit column memo" })).toBeNull();
    await user.click(within(screen.getByRole("list", { name: "Columns" })).getByRole("button", { name: /^memo / }));
    await answer(user, await dialog("Discard changes to row 2?"), "Discard");
    expect(await screen.findByRole("complementary", { name: "Edit column memo" })).toBeInTheDocument();
    expect(rowPanel()).toBeNull();
  });

  it("add_column_over_a_dirty_panel_asks", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(within(screen.getByRole("list", { name: "Columns" })).getByRole("button", { name: "Add column" }));
    await answer(user, await dialog("Discard changes to row 2?"), "Keep editing");
    expect(amount()).toHaveValue("450001");
  });
});

describe("F6 ruling 4: leaving the page", () => {
  const unload = () => {
    const e = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(e);
    return e.defaultPrevented;
  };

  it("the_browser_asks_while_the_draft_is_dirty_and_not_once_it_is_clean_again", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    expect(unload()).toBe(false);
    await dirty(user, panel);
    await vi.waitFor(() => expect(unload()).toBe(true));
    await user.type(within(panel).getByLabelText("amount_cents"), "{Backspace}");
    await vi.waitFor(() => expect(unload()).toBe(false));
  });

  it("the_listener_goes_once_the_draft_is_discarded", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await vi.waitFor(() => expect(unload()).toBe(true));
    await user.click(within(panel).getByRole("button", { name: "Cancel" }));
    await answer(user, await dialog("Discard changes to row 2?"), "Discard");
    await vi.waitFor(() => expect(unload()).toBe(false));
  });
});

describe("F6 ruling 5: other paths found", () => {
  it.each(["Types", "New table"])("the table list's %s view over a dirty panel asks", async (name) => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    const list = await screen.findByRole("navigation", { name: "Tables" });
    await user.click(within(list).getByRole("button", { name }));
    await answer(user, await dialog("Discard changes to row 2?"), "Keep editing");
    expect(amount()).toHaveValue("450001");
    expect(within(list).getByRole("button", { name })).toHaveAttribute("aria-pressed", "false");
  });

  it("the_structure_tabs_add_column_over_a_folded_dirty_panel_asks", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(screen.getByRole("tab", { name: "Structure" }));
    const structure = await screen.findByRole("tabpanel");
    await user.click(await within(structure).findByRole("button", { name: "Add column" }));
    await answer(user, await dialog("Discard changes to row 2?"), "Keep editing");
    await user.click(screen.getByRole("tab", { name: "Rows" }));
    expect(within(await screen.findByRole("complementary", { name: "Row" })).getByLabelText("amount_cents")).toHaveValue("450001");
  });
});

describe("F6: a live update to the open dirty row", () => {
  it("refreshes_the_untouched_fields_and_keeps_the_edited_one", async () => {
    const { rt, data, user } = mount({ realtime: true });
    await vi.waitFor(() => expect(rt!.active.length).toBeGreaterThan(0));
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    data[1].memo = "changed live";
    data[1].amount_cents = 47000;
    rt!.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    await vi.waitFor(() => expect(within(rowPanel()!).getByLabelText("memo")).toHaveValue("changed live"));
    expect(amount()).toHaveValue("450001");
    expect(within(rowPanel()!).getByText("1 field changed")).toBeInTheDocument();
  });

  it("an_untouched_field_refreshed_live_then_edited_is_checked_against_the_value_shown", async () => {
    const { f, rt, data, user } = mount({ realtime: true });
    await vi.waitFor(() => expect(rt!.active.length).toBeGreaterThan(0));
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    data[1].memo = "changed live";
    rt!.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    const memo = () => within(rowPanel()!).getByLabelText("memo");
    await vi.waitFor(() => expect(memo()).toHaveValue("changed live"));
    await user.type(memo(), "!");
    await user.click(within(rowPanel()!).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    const w = (await writes(f))[0].tree;
    expect(w.set).toEqual({ amount_cents: 450001, memo: "changed live!" });
    expect(w.where.and).toContainEqual({ col: "memo", eq: "changed live", text: true });
    expect(w.where.and).toContainEqual({ col: "amount_cents", eq: "45000", text: true });
    expect(screen.queryByRole("dialog", { name: "Changed by someone else" })).toBeNull();
  });
});
