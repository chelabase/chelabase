import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, FULL, ROWS, bodies, filterByTextKey, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/**
 * RT1 Playwright pass, fix F6 (user ruling, 2026-10-21): the row panel asks before it discards unsaved edits.
 *
 * Pins for the coder (names are what the tests look up):
 * - Dirty means what the panel's footer counts: an open row with at least one field whose draft differs from the row's
 *   value (typing the original value back is not dirty), or a "New row" panel with at least one field touched
 *   ("N fields filled"). A collapsed panel is still dirty (its draft stays mounted).
 * - Anything that would drop a dirty draft asks first with a `Modal` (role "dialog") whose title is
 *   "Discard changes to row N?" (N = the row's number in the grid's gutter, `firstRowNumber + index`), or
 *   "Discard changes to this row?" when the row is not on the page shown, or "Discard the new row?" for a New row
 *   panel; with the buttons "Discard" (destructive) and "Keep editing" (ghost). "Keep editing" changes nothing, "Discard"
 *   does what was asked. Escape (and the dialog's own close) act as "Keep editing". Nothing runs before the answer.
 *   The paths: the gutter button of the open row (close) and of another row (switch), the cell menu's "Open row" on
 *   another row, the "Insert row" button, the panel's own Cancel, the panel's "Open <fk column>" link and the table list /
 *   branch picker (the `navigate` prop is not called until Discard).
 * - No dialog when nothing is dirty, and none for: Collapse panel (the draft stays mounted; the strip's Expand panel gives
 *   it back), the Structure tab (it folds the panel the same way), the cell menu's / gutter's "Open row" on the row that
 *   is already open (expands it, draft kept), a successful Save (an update, and an insert that opens the saved row), and
 *   the panel's confirmed Delete row (the draft goes with the row, the user already confirmed).
 * - A live update to the open row while it is dirty leaves the draft alone.
 * - Decisions reported, not tested: the Act as picker (it also remounts the panel and drops the draft), browser back /
 *   closing the tab (no `beforeunload` for a draft), the "Insert with all defaults" menu item.
 */

const Q = `${B}/console/query`;
const key = (id: string) => JSON.stringify([id]);
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");

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
    if (tree.op === "delete") {
      const hit = data.filter((r) => matches(r, tree.where));
      return jsonResponse(hit.map((r) => ({ id: r.id })));
    }
    return jsonResponse(data);
  };
}
function mount(o: { realtime?: boolean; rows?: Data; handler?: FakeHandler } = {}) {
  const rt = o.realtime ? new FakeRealtime() : undefined;
  const data: Data = o.rows ?? ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ realtime: rt, routes: { [`POST ${Q}`]: o.handler ?? server(data) } });
  return { rt, data, ...s };
}
type Mounted = ReturnType<typeof mount>;
type User = Mounted["user"];

const openButton = (n: number) => screen.getByRole("button", { name: `Open row ${n}` });
const openRow = async (user: User, n: number) => {
  await user.click(await screen.findByRole("button", { name: `Open row ${n}` }));
  return screen.findByRole("complementary", { name: "Row" });
};
/** Edits the amount: 45000 becomes 450001. */
const dirty = async (user: User, panel: HTMLElement) => user.type(within(panel).getByLabelText("amount_cents"), "1");
const dialog = (title: string) => screen.findByRole("dialog", { name: title });
const noDialog = () => expect(screen.queryByRole("dialog")).toBeNull();
const rowPanel = () => screen.queryByRole("complementary", { name: "Row" });
const keep = async (user: User, d: HTMLElement) => user.click(within(d).getByRole("button", { name: "Keep editing" }));
const discard = async (user: User, d: HTMLElement) => user.click(within(d).getByRole("button", { name: "Discard" }));
const tableCell = async (text: string) => within(await screen.findByRole("grid", { name: "public.invoices" })).findByRole("gridcell", { name: text });

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("F6: the gutter", () => {
  it("closing_a_dirty_row_by_its_gutter_button_asks_and_keep_editing_changes_nothing", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(openButton(2));
    const d = await dialog("Discard changes to row 2?");
    expect(within(d).getByRole("button", { name: "Discard" })).toBeInTheDocument();
    await keep(user, d);
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(rowPanel()).toBeInTheDocument();
    expect(within(rowPanel()!).getByLabelText("amount_cents")).toHaveValue("450001");
    expect(openButton(2)).toHaveAttribute("aria-pressed", "true");
  });

  it("discard_closes_the_panel", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(openButton(2));
    await discard(user, await dialog("Discard changes to row 2?"));
    expect(rowPanel()).toBeNull();
    expect(openButton(2)).toHaveAttribute("aria-pressed", "false");
  });

  it("escape_acts_as_keep_editing", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(openButton(2));
    await dialog("Discard changes to row 2?");
    await user.keyboard("{Escape}");
    await vi.waitFor(noDialog);
    expect(within(rowPanel()!).getByLabelText("amount_cents")).toHaveValue("450001");
  });

  it("switching_to_another_row_asks_about_the_row_being_left_and_discard_opens_the_other_with_its_own_values", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    // elements grabbed before the modal opens: it marks the page behind it aria-hidden
    const two = openButton(2);
    await user.click(openButton(3));
    const d = await dialog("Discard changes to row 2?");
    // nothing has switched yet
    expect(two).toHaveAttribute("aria-pressed", "true");
    expect(within(panel).getByLabelText("amount_cents", { selector: "input" })).toHaveValue("450001");
    await discard(user, d);
    await vi.waitFor(() => expect(openButton(3)).toHaveAttribute("aria-pressed", "true"));
    expect(openButton(2)).toHaveAttribute("aria-pressed", "false");
    await vi.waitFor(() => expect(within(rowPanel()!).getByLabelText("amount_cents")).toHaveValue("8900"));
    noDialog();
  });

  it("keep_editing_on_a_switch_stays_on_the_row_with_its_draft", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(openButton(3));
    await keep(user, await dialog("Discard changes to row 2?"));
    expect(openButton(2)).toHaveAttribute("aria-pressed", "true");
    expect(openButton(3)).toHaveAttribute("aria-pressed", "false");
    expect(within(rowPanel()!).getByLabelText("amount_cents")).toHaveValue("450001");
  });

  it("a_collapsed_panel_with_a_draft_asks_when_another_row_is_opened", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(within(panel).getByRole("button", { name: "Collapse panel" }));
    await user.click(openButton(3));
    await keep(user, await dialog("Discard changes to row 2?"));
    await user.click(screen.getByRole("button", { name: "Expand panel" }));
    expect(within(await screen.findByRole("complementary", { name: "Row" })).getByLabelText("amount_cents")).toHaveValue("450001");
  });

  it("a_row_that_is_not_on_the_page_shown_is_named_this_row", async () => {
    const first = Array.from({ length: 50 }, (_, i) => ({ ...ROWS[1], id: `a-${i}` }));
    const second = Array.from({ length: 10 }, (_, i) => ({ ...ROWS[1], id: `b-${i}` }));
    const handler: FakeHandler = async (req) => {
      const { tree } = await req.clone().json();
      if (tree.op !== "select") return jsonResponse([]);
      if (tree.where) return jsonResponse(filterByTextKey(tree.where, [...first, ...second]));
      return jsonResponse(tree.offset === 0 ? first : second);
    };
    const { user } = mount({ handler });
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(await screen.findByRole("button", { name: "Next page" }));
    await user.click(await screen.findByRole("button", { name: "Open row 52" }));
    const d = await dialog("Discard changes to this row?");
    await discard(user, d);
    await vi.waitFor(() => expect(screen.getByRole("button", { name: "Open row 52" })).toHaveAttribute("aria-pressed", "true"));
  });
});

describe("F6: the other ways out of a dirty panel", () => {
  it("the_cell_menus_open_row_on_another_row_asks", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.pointer({ keys: "[MouseRight]", target: await tableCell("8900") });
    await user.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Open row" }));
    const d = await dialog("Discard changes to row 2?");
    expect(within(panel).getByLabelText("amount_cents", { selector: "input" })).toHaveValue("450001");
    await discard(user, d);
    await vi.waitFor(() => expect(openButton(3)).toHaveAttribute("aria-pressed", "true"));
  });

  it("the_insert_row_button_asks_and_discard_opens_an_empty_new_row_panel", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(screen.getByRole("button", { name: "Insert row" }));
    await discard(user, await dialog("Discard changes to row 2?"));
    const fresh = await screen.findByRole("complementary", { name: "New row" });
    expect(within(fresh).getByLabelText("customer_id")).toHaveValue("");
  });

  it("the_panels_cancel_button_asks", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(within(panel).getByRole("button", { name: "Cancel" }));
    await keep(user, await dialog("Discard changes to row 2?"));
    expect(rowPanel()).toBeInTheDocument();
    await user.click(within(rowPanel()!).getByRole("button", { name: "Cancel" }));
    await discard(user, await dialog("Discard changes to row 2?"));
    expect(rowPanel()).toBeNull();
  });

  it("a_new_row_panel_with_a_filled_field_asks_for_the_new_row", async () => {
    const { user } = mount();
    await user.click(await screen.findByRole("button", { name: "Insert row" }));
    const fresh = await screen.findByRole("complementary", { name: "New row" });
    await user.type(within(fresh).getByLabelText("customer_id"), "c-9");
    await user.click(within(fresh).getByRole("button", { name: "Cancel" }));
    const d = await dialog("Discard the new row?");
    await keep(user, d);
    expect(within(screen.getByRole("complementary", { name: "New row" })).getByLabelText("customer_id")).toHaveValue("c-9");
    await user.click(openButton(2));
    await discard(user, await dialog("Discard the new row?"));
    expect(await screen.findByRole("complementary", { name: "Row" })).toBeInTheDocument();
  });

  it("the_foreign_key_open_link_asks_before_it_navigates", async () => {
    const { user, navigate } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(within(panel).getByRole("button", { name: "Open customer_id" }));
    const d = await dialog("Discard changes to row 2?");
    expect(navigate).not.toHaveBeenCalled();
    await keep(user, d);
    expect(navigate).not.toHaveBeenCalled();
    await user.click(within(rowPanel()!).getByRole("button", { name: "Open customer_id" }));
    await discard(user, await dialog("Discard changes to row 2?"));
    expect(navigate).toHaveBeenLastCalledWith({ ...FULL, schema: "public", table: "customers" });
  });

  it("switching_table_from_the_table_list_asks_and_navigates_only_on_discard", async () => {
    const { user, navigate } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    const list = await screen.findByRole("navigation", { name: "Tables" });
    await user.click(within(list).getByRole("button", { name: "customers" }));
    const d = await dialog("Discard changes to row 2?");
    expect(navigate).not.toHaveBeenCalled();
    await keep(user, d);
    expect(navigate).not.toHaveBeenCalled();
    expect(within(rowPanel()!).getByLabelText("amount_cents")).toHaveValue("450001");
    await user.click(within(list).getByRole("button", { name: "customers" }));
    await discard(user, await dialog("Discard changes to row 2?"));
    expect(navigate).toHaveBeenLastCalledWith({ ...FULL, table: "customers" });
  });

  it("switching_branch_asks_and_navigates_only_on_discard", async () => {
    const { user, navigate } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(await screen.findByLabelText("Branch"));
    await user.click(await screen.findByRole("option", { name: /^feat-discounts/ }));
    const d = await dialog("Discard changes to row 2?");
    expect(navigate).not.toHaveBeenCalled();
    await discard(user, d);
    expect(navigate).toHaveBeenLastCalledWith({ projectId: "p1", databaseId: "d1", branchId: "b3" });
  });
});

describe("F6: no dialog when nothing would be lost", () => {
  it("a_clean_panel_closes_and_switches_at_once", async () => {
    const { user } = mount();
    await openRow(user, 2);
    await user.click(openButton(3));
    await vi.waitFor(() => expect(openButton(3)).toHaveAttribute("aria-pressed", "true"));
    await user.click(openButton(3));
    await vi.waitFor(() => expect(rowPanel()).toBeNull());
    noDialog();
  });

  it("typing_the_original_value_back_is_not_dirty", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.type(within(panel).getByLabelText("amount_cents"), "{Backspace}");
    expect(within(panel).getByText("No changes")).toBeInTheDocument();
    await user.click(openButton(2));
    await vi.waitFor(() => expect(rowPanel()).toBeNull());
    noDialog();
  });

  it("collapse_keeps_the_draft_without_asking", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(within(panel).getByRole("button", { name: "Collapse panel" }));
    noDialog();
    expect(rowPanel()).toBeNull();
    await user.click(screen.getByRole("button", { name: "Expand panel" }));
    expect(within(await screen.findByRole("complementary", { name: "Row" })).getByLabelText("amount_cents")).toHaveValue("450001");
  });

  it("the_structure_tab_folds_the_panel_without_asking", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(screen.getByRole("tab", { name: "Structure" }));
    noDialog();
    await user.click(screen.getByRole("tab", { name: "Rows" }));
    expect(within(await screen.findByRole("complementary", { name: "Row" })).getByLabelText("amount_cents")).toHaveValue("450001");
  });

  it("opening_the_row_that_is_already_open_from_the_cell_menu_keeps_the_draft_without_asking", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.pointer({ keys: "[MouseRight]", target: await tableCell("Q3 plan") });
    await user.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Open row" }));
    noDialog();
    expect(within(await screen.findByRole("complementary", { name: "Row" })).getByLabelText("amount_cents")).toHaveValue("450001");
  });

  it("the_gutter_button_of_the_open_row_on_a_collapsed_panel_expands_it_without_asking", async () => {
    const { user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(within(panel).getByRole("button", { name: "Collapse panel" }));
    await user.click(openButton(2));
    noDialog();
    expect(within(await screen.findByRole("complementary", { name: "Row" })).getByLabelText("amount_cents")).toHaveValue("450001");
  });

  it("a_saved_update_leaves_nothing_to_discard", async () => {
    const { f, user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    await vi.waitFor(() => expect(within(rowPanel()!).getByText("No changes")).toBeInTheDocument());
    await user.click(openButton(3));
    await vi.waitFor(() => expect(openButton(3)).toHaveAttribute("aria-pressed", "true"));
    noDialog();
  });

  it("saving_a_new_row_opens_the_saved_row_without_asking", async () => {
    const { f, user } = mount({ handler: async (req) => {
      const { tree } = await req.clone().json();
      return jsonResponse(tree.op === "select" ? filterByTextKey(tree.where, ROWS) : [ROWS[0]]);
    } });
    await user.click(await screen.findByRole("button", { name: "Insert row" }));
    const fresh = await screen.findByRole("complementary", { name: "New row" });
    await user.type(within(fresh).getByLabelText("customer_id"), "c-9");
    await user.type(within(fresh).getByLabelText("amount_cents"), "100");
    await user.click(within(fresh).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect(await screen.findByRole("complementary", { name: "Row" })).toBeInTheDocument();
    noDialog();
  });

  it("a_confirmed_row_delete_closes_the_panel_without_a_discard_dialog", async () => {
    const { f, user } = mount();
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    await user.click(within(panel).getByRole("button", { name: "Delete row" }));
    const confirm = await screen.findByRole("dialog", { name: "Delete this row?" });
    await user.click(within(confirm).getByRole("button", { name: "Delete row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    await vi.waitFor(() => expect(rowPanel()).toBeNull());
    expect(screen.queryByRole("dialog", { name: /^Discard/ })).toBeNull();
  });
});

describe("F6: a live update to the open row", () => {
  it("does_not_drop_the_draft_and_the_panel_still_asks_afterwards", async () => {
    const { rt, data, user } = mount({ realtime: true });
    await vi.waitFor(() => expect(rt!.active.length).toBeGreaterThan(0));
    const panel = await openRow(user, 2);
    await dirty(user, panel);
    data[1].memo = "changed live";
    rt!.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    expect(await tableCell("changed live")).toBeInTheDocument();
    rt!.resync();
    await vi.waitFor(() => expect(within(rowPanel()!).getByLabelText("amount_cents")).toHaveValue("450001"));
    expect(within(rowPanel()!).getByText("1 field changed")).toBeInTheDocument();
    await user.click(openButton(2));
    await keep(user, await dialog("Discard changes to row 2?"));
    expect(within(rowPanel()!).getByLabelText("amount_cents")).toHaveValue("450001");
  });
});
