import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { GONE_TEXT } from "@/lib/realtime/conflict";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime, member } from "@/test/fake-realtime";
import { B, D, ROWS, bodies, filterByTextKey, meAs, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

const Q = `${B}/console/query`;
const key = (id: string) => JSON.stringify([id]);
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const rowOf = (el: HTMLElement) => el.closest('[role="row"]') as HTMLElement;
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");

type Data = Record<string, unknown>[];
/** Evaluates the text-form `where`s Studio sends (`eq`/`is` conditions, `and`). */
function matches(row: Record<string, unknown>, w: any): boolean {
  if (!w) return true;
  if (w.and) return w.and.every((p: unknown) => matches(row, p));
  if ("is" in w) return (row[w.col] ?? null) === w.is;
  if ("eq" in w) return row[w.col] !== null && row[w.col] !== undefined && String(row[w.col]) === w.eq;
  return true;
}
/** A console answering selects from `data` and applying updates whose `where` still matches (a stale original matches nothing). */
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

function mount(o: { me?: ReturnType<typeof meAs>; rows?: Data } = {}) {
  const rt = new FakeRealtime();
  const data: Data = o.rows ?? ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ realtime: rt, me: o.me, routes: { [`POST ${Q}`]: server(data) } });
  return { rt, data, ...s };
}
/** The editor has joined; the server has said nothing yet (so no refetch races a test's own changes). */
const join = (rt: FakeRealtime) => vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
const live = async (rt: FakeRealtime) => {
  await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
  rt.joined();
};
const matchMedia = (reduce: boolean) =>
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: reduce && q.includes("reduce"), media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  matchMedia(false);
});
afterEach(() => vi.unstubAllGlobals());

describe("joining", () => {
  it("joins_the_table_channel_as_the_owner", async () => {
    const { rt } = mount();
    await cell("45000");
    await vi.waitFor(() => expect(rt.active).toHaveLength(1));
    expect(rt.table.params).toEqual({ database_id: "d1", branch_id: "b1", pg_database: "app", schema: "public", table: "invoices", role: null });
  });

  it("joins_as_the_act_as_role", async () => {
    const { rt } = mount({ me: meAs("developer", ["billing_admin", "reporter"]) });
    await cell("45000");
    await vi.waitFor(() => expect(rt.active).toHaveLength(1));
    expect(rt.table.params.role).toBe("billing_admin");
  });

  it("an_act_as_switch_leaves_and_rejoins_with_the_new_role", async () => {
    const { rt, user } = mount({ me: meAs("developer", ["billing_admin", "reporter"]) });
    await cell("45000");
    await vi.waitFor(() => expect(rt.active).toHaveLength(1));
    const first = rt.table;
    await user.click(screen.getByLabelText("Act as"));
    await user.click(await screen.findByRole("option", { name: "reporter" }));
    await vi.waitFor(() => expect(rt.table.params.role).toBe("reporter"));
    expect(first.left).toBe(true);
    expect(rt.active).toHaveLength(1);
  });

  it("leaves_when_the_editor_goes_away", async () => {
    const { rt, unmount } = mount();
    await cell("45000");
    await vi.waitFor(() => expect(rt.active).toHaveLength(1));
    unmount();
    expect(rt.active).toHaveLength(0);
  });

  it("joins_the_project_channel_and_leaves_it_on_unmount", async () => {
    const { rt, unmount } = mount();
    await cell("45000");
    await vi.waitFor(() => expect(rt.projects.filter((p) => !p.left).map((p) => p.projectId)).toEqual(["p1"]));
    unmount();
    expect(rt.projects.every((p) => p.left)).toBe(true);
  });
});

describe("the live header (§6.1)", () => {
  it("shows_Live_once_joined_live", async () => {
    const { rt } = mount();
    await live(rt);
    expect(await screen.findByTestId("live-state")).toHaveTextContent(/^Live$/);
  });

  it("shows_paused_with_the_reason", async () => {
    const { rt } = mount();
    await live(rt);
    rt.status(false, "no_slot");
    const state = await screen.findByTestId("live-state");
    await vi.waitFor(() => expect(state).toHaveTextContent("Live updates paused"));
    expect(state).toHaveTextContent("no free slot");
    rt.status(false, "reconnecting");
    await vi.waitFor(() => expect(state).toHaveTextContent("reconnecting"));
  });

  it("not_live_when_joined_without_a_slot", async () => {
    const { rt } = mount();
    await vi.waitFor(() => expect(rt.active).toHaveLength(1));
    rt.joined({ live: false, reason: "unavailable" });
    expect(await screen.findByTestId("live-state")).toHaveTextContent("Live updates paused");
  });
});

describe("refetching on live (R8) and the view set (R19)", () => {
  it("joined_live_refetches_the_page", async () => {
    const { rt, f } = mount();
    await cell("45000");
    await vi.waitFor(() => expect(rt.active).toHaveLength(1));
    const before = (await selects(f)).length;
    rt.joined();
    await vi.waitFor(async () => expect((await selects(f)).length).toBeGreaterThan(before));
  });

  it("status_live_true_refetches_the_page", async () => {
    const { rt, f } = mount();
    await live(rt);
    await cell("45000");
    rt.status(false, "reconnecting");
    const before = (await selects(f)).length;
    rt.status(true);
    await vi.waitFor(async () => expect((await selects(f)).length).toBeGreaterThan(before));
  });

  it("reports_the_page_keys_as_key_strings_in_a_view_frame", async () => {
    const { rt } = mount();
    await cell("45000");
    await vi.waitFor(() => expect(rt.table.views.at(-1)).toEqual([key("3f9a-1"), key("81c4-2"), key("c2d1-3")]));
  });

  it("a_page_change_sends_the_new_pages_keys", async () => {
    const first = Array.from({ length: 50 }, (_, i) => ({ ...ROWS[0], id: `a-${i}` }));
    const second = Array.from({ length: 3 }, (_, i) => ({ ...ROWS[0], id: `b-${i}` }));
    const rt = new FakeRealtime();
    const answer: FakeHandler = async (req) => {
      const { tree } = await req.clone().json();
      return jsonResponse(tree.op === "select" ? (tree.offset === 0 ? first : second) : []);
    };
    const { user } = renderEditor({ realtime: rt, routes: { [`POST ${Q}`]: answer } });
    await vi.waitFor(() => expect(rt.table.views.at(-1)).toHaveLength(50));
    await screen.findByText("Rows 1–50 of ≈ 48,210");
    await user.click(screen.getByRole("button", { name: "Next page" }));
    await vi.waitFor(() => expect(rt.table.views.at(-1)).toEqual(second.map((r) => key(r.id as string))));
  });
});

describe("live rows (§6.1)", () => {
  it("a_changed_key_on_the_page_is_refetched_once_by_key_and_spliced", async () => {
    const { rt, data, f } = mount();
    await join(rt);
    await cell("45000");
    const before = (await selects(f)).length;
    data[1].amount_cents = 46000;
    rt.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    expect(await cell("46000")).toBeInTheDocument();
    expect(screen.queryByRole("gridcell", { name: "45000" })).toBeNull();
    const after = (await selects(f)).slice(before);
    expect(after).toHaveLength(1);
    expect(after[0].tree.where).toEqual({ col: "id", in: ["81c4-2"], parse: true });
  });

  it("the_spliced_cells_flash_and_reduced_motion_turns_the_motion_off", async () => {
    const { rt, data } = mount();
    await join(rt);
    await cell("45000");
    data[1].amount_cents = 46000;
    rt.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    const changed = await cell("46000");
    expect(changed).toHaveAttribute("data-changed", "true");
    expect(changed).toHaveAttribute("data-motion", "flash");
  });

  it("under_reduced_motion_the_flash_has_no_motion", async () => {
    matchMedia(true);
    const { rt, data } = mount();
    await join(rt);
    await cell("45000");
    data[1].amount_cents = 46000;
    rt.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    const changed = await cell("46000");
    expect(changed).toHaveAttribute("data-changed", "true");
    expect(changed).toHaveAttribute("data-motion", "none");
  });

  it("a_spliced_row_that_left_the_view_fades", async () => {
    const { rt, data } = mount();
    await join(rt);
    const c = await cell("45000");
    data.splice(1, 1);
    rt.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    await vi.waitFor(() => expect(c).toBeInTheDocument());
    await vi.waitFor(() => expect(rowOf(c)).toHaveAttribute("data-fading", "true"));
  });

  it("inserts_off_the_page_show_a_new_rows_chip_that_refetches_on_show", async () => {
    const { rt, data, f, user } = mount();
    await join(rt);
    await cell("45000");
    data.push({ id: "z-1", customer_id: "c-9", status: "open", amount_cents: 1, memo: null }, { id: "z-2", customer_id: "c-9", status: "open", amount_cents: 2, memo: null });
    rt.rows(1, [{ op: "insert", keys: [key("z-1"), key("z-2")] }]);
    const chip = await screen.findByText(/^2 new rows/);
    expect(chip).toBeInTheDocument();
    const before = (await selects(f)).length;
    await user.click(screen.getByRole("button", { name: "Show" }));
    await vi.waitFor(async () => expect((await selects(f)).length).toBeGreaterThan(before));
    expect(await cell("z-2")).toBeInTheDocument();
    await vi.waitFor(() => expect(screen.queryByText(/new rows/)).toBeNull());
  });

  it("a_keyed_delete_strikes_the_row_through_until_the_next_page_refetch", async () => {
    const { rt, data } = mount();
    await join(rt);
    const c = await cell("8900");
    rt.rows(1, [{ op: "delete", keys: [key("c2d1-3")] }]);
    await vi.waitFor(() => expect(rowOf(c)).toHaveAttribute("data-deleted", "true"));
    expect(c).toBeInTheDocument();
    data.splice(2, 1);
    rt.rows(2, [{ op: "changed" }]);
    await vi.waitFor(() => expect(screen.queryByRole("gridcell", { name: "8900" })).toBeNull(), { timeout: 8000 });
  });

  it.each([
    ["a keyless delete", (rt: FakeRealtime) => rt.rows(1, [{ op: "delete" }])],
    ["changed", (rt: FakeRealtime) => rt.rows(1, [{ op: "changed" }])],
    ["resync", (rt: FakeRealtime) => rt.resync()],
    ["a seq gap", (rt: FakeRealtime) => rt.rows(5, [{ op: "update", keys: [key("81c4-2")] }])],
  ])("%s_refetches_the_page_after_the_debounce_not_before_it", async (_name, send) => {
    const { rt, f } = mount();
    await vi.waitFor(() => expect(rt.active).toHaveLength(1));
    await cell("45000");
    const before = (await selects(f)).length;
    send(rt);
    await new Promise((r) => setTimeout(r, 300));
    expect((await selects(f)).length).toBe(before);
    await vi.waitFor(async () => expect((await selects(f)).length).toBeGreaterThan(before), { timeout: 8000 });
  });

  it("a_keyless_burst_is_one_refetch", async () => {
    const { rt, f } = mount();
    await vi.waitFor(() => expect(rt.active).toHaveLength(1));
    await cell("45000");
    const before = (await selects(f)).length;
    rt.rows(1, [{ op: "changed" }]);
    rt.rows(2, [{ op: "changed" }]);
    rt.rows(3, [{ op: "delete" }]);
    await new Promise((r) => setTimeout(r, 1500));
    expect((await selects(f)).length).toBe(before + 1);
  });
});

describe("project events (§5.3, P10)", () => {
  it("a_branch_event_refetches_the_branch_list", async () => {
    const { rt, f } = mount();
    await cell("45000");
    await vi.waitFor(() => expect(rt.projects.some((p) => !p.left)).toBe(true));
    const lists = () => f.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname === `${D}/branches`).length;
    const before = lists();
    rt.projects.find((p) => !p.left)!.handlers.onEvent({ event: "branch.status", database_id: "d1", branch_id: "b1", status: "ready" });
    await vi.waitFor(() => expect(lists()).toBeGreaterThan(before));
  });

  it("a_pg_database_event_refetches_the_branchs_databases", async () => {
    const { rt, f } = mount();
    await cell("45000");
    await vi.waitFor(() => expect(rt.projects.some((p) => !p.left)).toBe(true));
    const reads = () => f.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname === `${B}/databases`).length;
    const before = reads();
    rt.projects.find((p) => !p.left)!.handlers.onEvent({ event: "pg_database.created", database_id: "d1", branch_id: "b1", name: "reporting" });
    await vi.waitFor(() => expect(reads()).toBeGreaterThan(before));
  });
});

describe("presence on the grid (§6.2)", () => {
  const ana = member({ id: "ana", sub: "u-ana", name: "Ana", email: "ana@x.test", color_seed: 2, state: { row: key("81c4-2"), column: "amount_cents", editing: false } });

  it("another_members_selection_is_drawn_and_the_avatar_stack_lists_them", async () => {
    const { rt } = mount();
    await join(rt);
    await cell("45000");
    rt.presenceState([ana]);
    expect(await screen.findByTestId("presence-cell")).toHaveAttribute("data-member", "ana");
    expect(screen.getByTestId("avatar-stack")).toHaveTextContent("A");
  });

  it("my_own_sub_is_never_drawn", async () => {
    const { rt } = mount();
    await join(rt);
    await cell("45000");
    rt.presenceState([{ ...ana, id: "me", sub: "u2", name: "Bo" }]);
    // Yourself is neither drawn nor counted: no avatar stack for a table only you are on.
    await vi.waitFor(() => expect(screen.queryByTestId("avatar-stack")).toBeNull());
    expect(screen.queryByTestId("presence-cell")).toBeNull();
  });

  it("a_presence_diff_moves_a_member", async () => {
    const { rt } = mount();
    await join(rt);
    await cell("45000");
    rt.presenceState([ana]);
    await screen.findByTestId("presence-cell");
    rt.presenceDiff([{ ...ana, state: { ...ana.state, row: key("3f9a-1") } }], [ana]);
    await vi.waitFor(() => expect(screen.getByTestId("presence-cell")).toHaveAttribute("data-row", key("3f9a-1")));
    rt.presenceDiff([], [ana]);
    await vi.waitFor(() => expect(screen.queryByTestId("presence-cell")).toBeNull());
  });

  it("opening_an_editor_on_a_cell_someone_edits_says_so", async () => {
    const { rt, user } = mount();
    await join(rt);
    const c = await cell("45000");
    rt.presenceState([{ ...ana, state: { ...ana.state, editing: true } }]);
    await screen.findByTestId("presence-cell");
    await user.dblClick(c);
    expect(await screen.findByText("Ana is editing this cell")).toBeInTheDocument();
  });

  it("selecting_a_cell_sends_my_state_and_editing_sends_editing_true", async () => {
    const { rt, user } = mount();
    await join(rt);
    const c = await cell("45000");
    await user.click(c);
    await vi.waitFor(() => expect(rt.table.presence.at(-1)).toEqual({ row: key("81c4-2"), column: "amount_cents", editing: false }));
    await user.dblClick(c);
    await vi.waitFor(() => expect(rt.table.presence.at(-1)).toEqual({ row: key("81c4-2"), column: "amount_cents", editing: true }));
    await user.keyboard("{Escape}");
    await vi.waitFor(() => expect(rt.table.presence.at(-1)?.editing).toBe(false));
  });
});

describe("the conflict check (§6.3, R18, W1)", () => {
  const edit = async (user: ReturnType<typeof mount>["user"], from: string, to: string, name = "amount_cents") => {
    await user.dblClick(await cell(from));
    const input = await screen.findByRole("textbox", { name });
    await user.clear(input);
    await user.type(input, `${to}{Enter}`);
  };

  it("an_open_editor_is_never_overwritten_by_a_splice_and_marks_the_change", async () => {
    const { rt, data, user } = mount();
    await join(rt);
    await user.dblClick(await cell("45000"));
    const input = await screen.findByRole("textbox", { name: "amount_cents" });
    data[1].amount_cents = 46000;
    rt.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    const marker = await screen.findByTestId("changed-marker");
    expect(screen.getByRole("textbox", { name: "amount_cents" })).toBe(input);
    expect(input).toHaveValue("45000");
    await user.hover(marker);
    expect(await screen.findByRole("tooltip")).toHaveTextContent("46000");
  });

  it("W1_the_where_uses_the_originals_from_when_the_editor_opened_not_the_spliced_page", async () => {
    const { rt, data, f, user } = mount();
    await join(rt);
    await user.dblClick(await cell("45000"));
    const input = await screen.findByRole("textbox", { name: "amount_cents" });
    data[1].amount_cents = 46000;
    rt.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    await screen.findByTestId("changed-marker");
    await user.clear(input);
    await user.type(input, "47000{Enter}");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.where).toEqual({ and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "45000", text: true }] });
  });

  it("zero_rows_re_reads_the_row_by_key_and_shows_theirs_against_yours", async () => {
    const { rt, data, f, user } = mount();
    await join(rt);
    await cell("45000");
    data[1].amount_cents = 46000; // someone changed it; this page still shows 45000
    await edit(user, "45000", "47000");
    const dialog = await screen.findByRole("dialog", { name: "Changed by someone else" });
    expect(within(dialog).getByRole("row", { name: /amount_cents/ })).toHaveTextContent(/46000.*47000/);
    const reread = (await selects(f)).at(-1);
    expect(reread.tree.where).toEqual({ col: "id", eq: "81c4-2", parse: true });
    expect((await writes(f))).toHaveLength(1);
  });

  it("overwrite_saves_again_with_the_primary_key_only", async () => {
    const { rt, data, f, user } = mount();
    await join(rt);
    await cell("45000");
    data[1].amount_cents = 46000;
    await edit(user, "45000", "47000");
    await user.click(await screen.findByRole("button", { name: "Overwrite" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(2));
    const second = (await writes(f))[1].tree;
    expect(second.set).toEqual({ amount_cents: 47000 });
    expect(second.where).toEqual({ col: "id", eq: "81c4-2", parse: true });
    expect(await cell("47000")).toBeInTheDocument();
    await vi.waitFor(() => expect(screen.queryByRole("dialog", { name: "Changed by someone else" })).toBeNull());
  });

  it("take_theirs_shows_their_value_and_writes_nothing_more", async () => {
    const { rt, data, f, user } = mount();
    await join(rt);
    await cell("45000");
    data[1].amount_cents = 46000;
    await edit(user, "45000", "47000");
    await user.click(await screen.findByRole("button", { name: "Take theirs" }));
    expect(await cell("46000")).toBeInTheDocument();
    expect(await writes(f)).toHaveLength(1);
    await vi.waitFor(() => expect(screen.queryByRole("dialog", { name: "Changed by someone else" })).toBeNull());
  });

  it("a_row_gone_or_hidden_says_so", async () => {
    const { rt, data, user } = mount();
    await join(rt);
    await cell("45000");
    data.splice(1, 1);
    await edit(user, "45000", "47000");
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent(GONE_TEXT);
    expect(within(dialog).queryByRole("button", { name: "Overwrite" })).toBeNull();
  });

  it("a_null_original_is_checked_with_is_null", async () => {
    const { rt, f, user } = mount();
    await join(rt);
    await edit(user, "NULL", "hello", "memo");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.where).toEqual({ and: [{ col: "id", eq: "3f9a-1", parse: true }, { col: "memo", is: null }] });
  });

  it("the_page_reads_every_column_explicitly_with_text_of_items_for_the_key_and_originals", async () => {
    const { f } = mount();
    await cell("45000");
    const tree = (await selects(f)).at(-1).tree;
    expect(Array.isArray(tree.select)).toBe(true);
    const items = tree.select as unknown[];
    expect(items.slice(0, 5)).toEqual(["id", "customer_id", "status", "amount_cents", "memo"]);
    expect(items).toContainEqual({ text_of: "id", as: "$pk0" });
    expect(items).toContainEqual({ text_of: "amount_cents", as: expect.stringMatching(/^\$o\d+$/) });
  });

  it("the_row_panel_checks_against_the_originals_from_when_it_opened", async () => {
    const { rt, data, f, user } = mount();
    await join(rt);
    await user.click(await screen.findByRole("button", { name: "Open row 2" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    const amount = within(panel).getByLabelText("amount_cents");
    data[1].amount_cents = 46000;
    rt.rows(1, [{ op: "update", keys: [key("81c4-2")] }]);
    await user.clear(amount);
    await user.type(amount, "47000");
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    expect((await writes(f))[0].tree.where).toEqual({ and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "45000", text: true }] });
    expect(await screen.findByRole("dialog", { name: "Changed by someone else" })).toBeInTheDocument();
  });
});
