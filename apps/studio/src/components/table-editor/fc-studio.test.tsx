import { act, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CHANGED_TITLE } from "@/lib/realtime/conflict";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime, member } from "@/test/fake-realtime";
import { B, ROWS, bodies, filterByTextKey, invoices, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

// RT1 final round, batch C (Studio): reviews I1, I3, I4 and the minors.

const Q = `${B}/console/query`;
const key = (id: string) => JSON.stringify([id]);
const grid = () => screen.findByRole("grid", { name: "public.invoices" });
const cell = async (text: string) => within(await grid()).findByRole("gridcell", { name: text });
const selects = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "select");
const writes = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op !== "select" && b.tree.op !== "count");
const updates = async (f: Parameters<typeof bodies>[0]) => (await bodies(f, "POST", Q)).filter((b) => b.tree.op === "update");
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

type Data = Record<string, unknown>[];
/** Evaluates the `where`s Studio sends for rows read as text (`eq`/`is` conditions, `and`). */
function matches(row: Record<string, unknown>, w: any): boolean {
  if (!w) return true;
  if (w.and) return w.and.every((p: unknown) => matches(row, p));
  if ("is" in w) return (row[w.col] ?? null) === w.is;
  if ("eq" in w) return row[w.col] !== null && row[w.col] !== undefined && String(row[w.col]) === w.eq;
  return true;
}
/** A console answering selects from `data`, applying updates whose `where` still matches, and optionally slow or held on selects. */
function server(data: Data, gate: () => Promise<unknown> | undefined = () => undefined): FakeHandler {
  return async (req) => {
    const { tree } = await req.clone().json();
    if (tree.op === "select") {
      await gate();
      return jsonResponse(filterByTextKey(tree.where, data));
    }
    if (tree.op === "update") {
      const hit = data.filter((r) => matches(r, tree.where));
      for (const r of hit) Object.assign(r, tree.set);
      return jsonResponse(hit);
    }
    return jsonResponse(data);
  };
}

function mount(o: { data?: Data; gate?: () => Promise<unknown> | undefined } = {}) {
  const rt = new FakeRealtime();
  const data: Data = o.data ?? ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ realtime: rt, routes: { [`POST ${Q}`]: server(data, o.gate) } });
  return { rt, data, ...s };
}
const live = async (rt: FakeRealtime) => {
  await vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));
  rt.joined();
};
const edit = async (user: ReturnType<typeof mount>["user"], from: string, to: string, name = "amount_cents") => {
  // The editor may only open once the row's texts are read again: try until it is there.
  await vi.waitFor(
    async () => {
      await user.dblClick(await cell(from));
      expect(screen.getByRole("textbox", { name })).toBeInTheDocument();
    },
    { timeout: 4000 },
  );
  const input = screen.getByRole("textbox", { name });
  await user.clear(input);
  await user.type(input, `${to}{Enter}`);
};

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("non-fatal channel errors (review I1)", () => {
  it.each(["invalid", "limit", "unavailable"] as const)("a_%s_error_on_a_live_channel_does_not_pause_live", async (code) => {
    const { rt } = mount();
    await live(rt);
    await cell("45000");
    expect(screen.getByTestId("live-state")).toHaveTextContent(/^Live$/);
    act(() => rt.table.handlers.onError?.({ code, message: "x" }));
    expect(screen.getByTestId("live-state")).toHaveTextContent(/^Live$/);
  });

  it.each([
    ["forbidden", "no access"],
    ["not_found", "not found"],
  ] as const)("a_%s_error_still_pauses_live", async (code, reason) => {
    const { rt } = mount();
    await live(rt);
    await cell("45000");
    act(() => rt.table.handlers.onError?.({ code, message: "x" }));
    expect(screen.getByTestId("live-state")).toHaveTextContent(`Live updates paused · ${reason}`);
  });
});

describe("by-key reads, saves, deletes and live refetches parse the key (review I3)", () => {
  it("every_condition_on_the_key_column_carries_parse_and_never_text", async () => {
    const { rt, data, f, user } = mount();
    await live(rt);
    await cell("45000");
    // A live refetch by key, a cell save, the panel's own read and its delete.
    data[0].amount_cents = 13000;
    rt.rows(1, [{ op: "update", keys: [key("3f9a-1")] }]);
    await cell("13000");
    await edit(user, "45000", "46000");
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(1));
    await user.click(await screen.findByRole("button", { name: "Open row 3" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    await within(panel).findByLabelText("amount_cents");
    await user.click(within(panel).getByRole("button", { name: "Delete row" }));
    await user.click(within(await screen.findByRole("dialog", { name: "Delete this row?" })).getByRole("button", { name: "Delete row" }));
    await vi.waitFor(async () => expect(await writes(f)).toHaveLength(2));

    const conditions: Record<string, unknown>[] = [];
    const walk = (w: any) => {
      if (!w) return;
      if (w.and || w.or) return (w.and ?? w.or).forEach(walk);
      if (w.col === "id") conditions.push(w);
    };
    for (const b of await bodies(f, "POST", Q)) walk(b.tree.where);
    expect(conditions.length).toBeGreaterThanOrEqual(5);
    for (const c of conditions) {
      expect(c).toHaveProperty("parse", true);
      expect(c).not.toHaveProperty("text");
    }
    // The panel's read of its row.
    const panelRead = (await selects(f)).find((b) => b.tree.limit === 1 && b.tree.where?.eq === "c2d1-3");
    expect(panelRead.tree.where).toEqual({ col: "id", eq: "c2d1-3", parse: true });
  });
});

describe("the row panel never saves without originals (review I4)", () => {
  it("after_overwrite_a_quick_second_save_is_checked_against_the_value_just_written", async () => {
    let held: Promise<void> | undefined;
    let release = () => {};
    const { data, f, user } = mount({ gate: () => held });
    await cell("45000");
    await user.click(await screen.findByRole("button", { name: "Open row 2" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    const amount = await within(panel).findByLabelText("amount_cents");
    await vi.waitFor(() => expect(amount).toHaveValue("45000"));
    data[1].amount_cents = 46000; // someone else changed it
    await user.clear(amount);
    await user.type(amount, "47000");
    await user.click(within(panel).getByRole("button", { name: "Save row" }));
    const dialog = await screen.findByRole("dialog", { name: CHANGED_TITLE });
    // The reads that follow the overwrite stay pending: the panel has no fresh texts yet.
    held = new Promise<void>((r) => (release = r));
    await user.click(within(dialog).getByRole("button", { name: "Overwrite" }));
    await vi.waitFor(async () => expect(await updates(f)).toHaveLength(2));
    await vi.waitFor(() => expect(within(panel).getByLabelText("amount_cents")).toHaveValue("47000"));
    const before = (await updates(f)).length;
    const again = within(panel).getByLabelText("amount_cents");
    await user.clear(again);
    await user.type(again, "48000");
    const save = within(panel).getByRole("button", { name: /Save row|Saving/ });
    if (!save.hasAttribute("disabled")) await user.click(save);
    // Whatever the panel did, no write went out without the original of the column.
    for (const w of (await updates(f)).slice(before)) {
      expect(w.tree.where).toEqual({ and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "47000", text: true }] });
    }
    release();
    // Once it has read the row again, the save goes through with its check.
    await vi.waitFor(async () => {
      if ((await updates(f)).length === before) {
        const b = within(panel).getByRole("button", { name: /Save row|Saving/ });
        if (!b.hasAttribute("disabled")) await user.click(b);
      }
      expect((await updates(f)).length).toBeGreaterThan(before);
    });
    const last = (await updates(f)).at(-1);
    expect(last.tree.set).toEqual({ amount_cents: 48000 });
    expect(last.tree.where).toEqual({ and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "47000", text: true }] });
    expect(data[1].amount_cents).toBe(48000);
  });
});

describe("an own save refreshes the originals before the next edit (minor)", () => {
  it("an_immediate_re_edit_of_the_cell_just_saved_is_no_false_conflict", async () => {
    let slow = false;
    const { data, f, user } = mount({ gate: () => (slow ? sleep(700) : undefined) });
    await cell("45000");
    slow = true; // the reads after the first save arrive late
    await edit(user, "45000", "47000");
    await cell("47000");
    await edit(user, "47000", "48000");
    await vi.waitFor(async () => expect(await updates(f)).toHaveLength(2));
    expect((await updates(f))[1].tree.where).toEqual({ and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "47000", text: true }] });
    expect(await cell("48000")).toBeInTheDocument();
    expect(screen.queryByRole("dialog", { name: CHANGED_TITLE })).toBeNull();
    expect(data[1].amount_cents).toBe(48000);
  });
});

describe("avatars are per member (minor)", () => {
  const sel = { row: key("81c4-2"), column: "amount_cents", editing: false };
  const ana = (id: string) => member({ id, sub: "u-ana", name: "Ana", email: "ana@x.test", color_seed: 2, state: sel });
  const me = (id: string) => member({ id, sub: "u2", name: "Bo", email: "bo@example.test", color_seed: 5 });

  it("two_sockets_of_one_member_are_one_avatar_and_yourself_is_not_listed", async () => {
    const { rt } = mount();
    await live(rt);
    await cell("45000");
    act(() => rt.presenceState([ana("ana-1"), me("me-1"), ana("ana-2"), me("me-2")]));
    await screen.findByTestId("avatar-stack");
    expect(screen.getAllByTestId("avatar")).toHaveLength(1);
    expect(screen.getByTestId("avatar-stack")).toHaveAccessibleName("1 person here");
  });

  it("the_overflow_counts_people_not_sockets", async () => {
    const { rt } = mount();
    await live(rt);
    await cell("45000");
    const people = Array.from({ length: 7 }, (_, i) => [1, 2].map((s) => member({ id: `p${i}-${s}`, sub: `u-p${i}`, name: `Person ${i}`, color_seed: i }))).flat();
    act(() => rt.presenceState(people));
    await screen.findByTestId("avatar-stack");
    expect(screen.getAllByTestId("avatar")).toHaveLength(5);
    expect(screen.getByTestId("avatar-stack")).toHaveTextContent("+2");
    expect(screen.getByTestId("avatar-stack")).toHaveAccessibleName("7 people here");
  });

  it("alone_you_get_no_stack", async () => {
    const { rt } = mount();
    await live(rt);
    await cell("45000");
    act(() => rt.presenceState([me("me-1")]));
    act(() => rt.presenceDiff([ana("ana-1")]));
    await screen.findByTestId("avatar-stack");
    act(() => rt.presenceDiff([], [ana("ana-1")]));
    await vi.waitFor(() => expect(screen.queryByTestId("avatar-stack")).toBeNull());
  });
});

describe("the insert opens the panel for every key type (minor)", () => {
  const col = (attnum: number, name: string, data_type: string, type_oid: number, over: Record<string, unknown> = {}) => ({
    attnum, name, data_type, type_oid, not_null: false, default: null, comment: null, collation: null, generated: null, identity: null, grants: [], ...over,
  });

  /** A one-column key table whose key the console prints differently as JSON and as text. */
  function keyed(dataType: string, oid: number, json: unknown, text: string) {
    const existing: { json: unknown; text: string; memo: string } = { json: "old", text: "old-text", memo: "first" };
    const rows = [existing];
    const handler: FakeHandler = async (req) => {
      const { tree } = await req.clone().json();
      const items = (Array.isArray(tree.select) ? tree.select : Array.isArray(tree.returning) ? tree.returning : []) as unknown[];
      const render = (r: { json: unknown; text: string; memo: string }) => {
        const out: Record<string, unknown> = { id: r.json, memo: r.memo };
        // Selects pass through `serverAliases`, which would overwrite the text aliases with the JSON form: `$text` is the session text it honours.
        if (tree.op === "select") out.$text = { id: r.text };
        for (const i of items) if (typeof i === "object" && i !== null && "text_of" in i) {
          const item = i as unknown as { text_of: string; as: string };
          out[item.as] = item.text_of === "id" ? r.text : r.memo;
        }
        return out;
      };
      if (tree.op === "select") {
        const w = tree.where;
        // Like Postgres: a `parse` condition matches when the value parses to the row's value (its JSON form or its session text).
        const parses = (r: { json: unknown; text: string }, v: string) => (w.parse === true ? v === r.text || v === String(r.json) : v === r.text);
        const hit = w && (w.parse === true || w.text === true) && "eq" in w ? rows.filter((r) => parses(r, w.eq)) : w && w.parse === true && Array.isArray(w.in) ? rows.filter((r) => w.in.some((v: string) => parses(r, v))) : rows;
        return jsonResponse(hit.map(render));
      }
      if (tree.op === "insert") {
        const row = { json, text, memo: String(tree.values[0].memo) };
        rows.push(row);
        return jsonResponse([render(row)]);
      }
      return jsonResponse(rows.map(render));
    };
    const table = invoices({
      columns: [col(1, "id", dataType, oid, { not_null: true, default: "generated" }), col(2, "memo", "text", 25)],
      constraints: [{ name: "invoices_pkey", kind: "primary_key", columns: [1], definition: "PRIMARY KEY (id)", deferrable: false, initially_deferred: false, oid: 1, references: null, validated: true }],
    } as never);
    return { handler, table, text };
  }

  it.each([
    ["timestamptz", "timestamp with time zone", 1184, "2026-10-16T12:00:00+00:00", "Fri Oct 16 12:00:00 2026 UTC"],
    ["timestamp", "timestamp without time zone", 1114, "2026-10-16T12:00:00", "Fri Oct 16 12:00:00 2026"],
    ["numeric", "numeric(10,2)", 1700, 12.5, "12.50"],
    ["date", "date", 1082, "2026-10-16", "10-16-2026"],
  ])("a_%s_key_row_opens_in_the_panel_after_the_insert", async (_name, dataType, oid, json, text) => {
    const k = keyed(dataType, oid, json, text);
    const { f, user } = renderEditor({ table: k.table, routes: { [`POST ${Q}`]: k.handler } });
    await user.click(await screen.findByRole("button", { name: "Insert row" }));
    const fresh = await screen.findByRole("complementary", { name: "New row" });
    await user.type(within(fresh).getByLabelText("memo"), "hello");
    await user.click(within(fresh).getByRole("button", { name: "Save row" }));
    const panel = await screen.findByRole("complementary", { name: "Row" });
    expect(within(panel).getByRole("heading")).toHaveTextContent("id =");
    const read = (await selects(f)).filter((b) => b.tree.limit === 1).at(-1);
    expect(read.tree.where).toEqual({ col: "id", eq: k.text, parse: true });
    // The key adopted is the server's text (`$pk0`), never one the client formatted.
    expect(within(panel).getByRole("heading")).toHaveTextContent(`id = ${String(json)}`);
  });
});
