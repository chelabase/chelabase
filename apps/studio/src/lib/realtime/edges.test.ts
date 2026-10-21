import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "@/test/fake-realtime";
import { HIDDEN_LEAVE_MS, createRealtimeClient, tableChannelId } from "./client";
import { isComparable, splitTextRows } from "./conflict";
import { pkAliases, typedKeyString } from "./keys";
import { INITIAL_LIVE, createLiveController, reduceLive } from "./live-rows";
import { editingNotice, presenceColorIndex } from "./presence";
import type { TableHandlers, TableJoinParams } from "./types";
import { member } from "@/test/fake-realtime";

// Edge cases beyond the RED set (coder's probes).

const PARAMS: TableJoinParams = { database_id: "d1", branch_id: "b1", pg_database: "app", schema: "public", table: "invoices", role: null };
const ID = tableChannelId(PARAMS);

class FakeDocument extends EventTarget {
  visibilityState: DocumentVisibilityState = "visible";
  set(v: DocumentVisibilityState) {
    this.visibilityState = v;
    this.dispatchEvent(new Event("visibilitychange"));
  }
}

function setup(token: string | null = "tok-1") {
  const fetchMock = vi.fn(async () => new Response(JSON.stringify({ ticket: "t", expires_in: 30 }), { status: 201 }));
  const doc = new FakeDocument();
  const client = createRealtimeClient({ apiUrl: "http://api.test", token: token ?? undefined, fetch: fetchMock as unknown as typeof fetch, WebSocket: FakeWebSocket, random: () => 0, document: doc });
  return { client, fetchMock, doc };
}
const h = (): Required<TableHandlers> => ({ onJoined: vi.fn(), onError: vi.fn(), onRows: vi.fn(), onStatus: vi.fn(), onResync: vi.fn(), onPresenceState: vi.fn(), onPresenceDiff: vi.fn() });
const flush = () => vi.advanceTimersByTimeAsync(0);
const ws = () => FakeWebSocket.instances.at(-1)!;
async function open() {
  await flush();
  ws().open();
  await flush();
  return ws();
}
const answer = (socket: FakeWebSocket, i = -1) => {
  const join = socket.frames("join").at(i)!;
  socket.receive({ type: "joined", ref: join.ref, channel: ID, info: { pk: ["id"], live: true } });
};

describe("client edges", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    FakeWebSocket.reset();
  });
  afterEach(() => vi.useRealTimers());

  it("no_token_no_ticket_until_setToken", async () => {
    const s = setup(null);
    s.client.joinTable(PARAMS, h());
    await flush();
    expect(s.fetchMock).not.toHaveBeenCalled();
    s.client.setToken("tok-9");
    await flush();
    expect(s.fetchMock).toHaveBeenCalledTimes(1);
  });

  it("the_same_token_again_sends_no_second_auth_frame", async () => {
    const s = setup();
    s.client.joinTable(PARAMS, h());
    const socket = await open();
    s.client.setToken("tok-1");
    expect(socket.frames("auth")).toHaveLength(1);
  });

  it("view_and_presence_before_joined_go_out_after_joined_and_again_after_a_rejoin", async () => {
    const s = setup();
    const sub = s.client.joinTable(PARAMS, h());
    sub.view(['["1"]']);
    sub.presence({ row: '["1"]', column: "memo", editing: false });
    const first = await open();
    expect(first.frames("view")).toHaveLength(0);
    answer(first);
    expect(first.frames("view").at(-1)).toEqual({ type: "view", channel: ID, keys: ['["1"]'] });
    expect(first.frames("presence")).toHaveLength(1);
    first.drop(1012);
    await vi.advanceTimersByTimeAsync(500);
    const second = await open();
    answer(second);
    expect(second.frames("view")).toHaveLength(1);
    expect(second.frames("presence")).toHaveLength(1);
  });

  it("a_refused_join_is_not_joined_again_after_a_reconnect", async () => {
    const s = setup();
    const handlers = h();
    s.client.joinTable(PARAMS, handlers);
    const first = await open();
    first.receive({ type: "error", ref: first.frames("join")[0].ref, code: "forbidden", message: "no" });
    first.drop(1012);
    await vi.advanceTimersByTimeAsync(500);
    const second = await open();
    expect(second.frames("join")).toHaveLength(0);
    expect(handlers.onStatus).not.toHaveBeenCalled();
  });

  it("frames_for_a_left_channel_never_reach_its_rejoin_before_the_rejoins_joined", async () => {
    const s = setup();
    const old = h();
    const sub = s.client.joinTable(PARAMS, old);
    const socket = await open();
    answer(socket);
    sub.leave();
    const next = h();
    s.client.joinTable({ ...PARAMS, role: "reporter" }, next);
    // The server still had the old join's frames in flight.
    socket.receive({ type: "rows", channel: ID, seq: 9, changes: [{ op: "changed" }] });
    expect(old.onRows).not.toHaveBeenCalled();
    expect(next.onRows).not.toHaveBeenCalled();
    answer(socket);
    socket.receive({ type: "rows", channel: ID, seq: 1, changes: [] });
    expect(next.onRows).toHaveBeenCalledTimes(1);
  });

  it("revoked_4003_reports_forbidden_and_a_later_join_connects_again", async () => {
    const s = setup();
    const handlers = h();
    s.client.joinTable(PARAMS, handlers);
    const socket = await open();
    answer(socket);
    socket.drop(4003);
    expect(handlers.onError).toHaveBeenCalledWith(expect.objectContaining({ code: "forbidden" }));
    await vi.advanceTimersByTimeAsync(60_000);
    expect(FakeWebSocket.instances).toHaveLength(1);
    s.client.joinTable({ ...PARAMS, table: "customers" }, h());
    await flush();
    expect(FakeWebSocket.instances).toHaveLength(2);
  });

  it("a_join_made_after_the_hidden_leave_waits_for_the_tab_to_be_shown", async () => {
    const s = setup();
    s.client.joinTable(PARAMS, h());
    const socket = await open();
    answer(socket);
    s.doc.set("hidden");
    await vi.advanceTimersByTimeAsync(HIDDEN_LEAVE_MS);
    s.client.joinTable({ ...PARAMS, table: "customers" }, h());
    expect(socket.frames("join")).toHaveLength(1);
    s.doc.set("visible");
    expect(socket.frames("join")).toHaveLength(3);
  });

  it("the_project_channel_reports_live_on_joined_and_not_live_on_a_drop", async () => {
    const s = setup();
    const onLive = vi.fn();
    s.client.joinProject("p1", { onEvent: vi.fn(), onLive });
    const socket = await open();
    socket.receive({ type: "joined", ref: socket.frames("join")[0].ref, channel: "project:p1", info: {} });
    expect(onLive).toHaveBeenLastCalledWith(true);
    socket.drop(1012);
    expect(onLive).toHaveBeenLastCalledWith(false);
  });
});

describe("keys and the conflict check, edges", () => {
  it("aliases_skip_every_taken_spelling", () => {
    expect(pkAliases(["id"], ["$pk0", "$pk0_"])).toEqual(["$pk0__"]);
  });

  it("an_inserted_row_is_keyed_only_when_its_typed_values_are_exactly_their_text", () => {
    const types = { id: { type: "uuid" }, n: { type: "bigint" }, amt: { type: "numeric(12,2)" }, at: { type: "timestamp with time zone" }, c: { type: "character(3)" }, s: { type: "text", enum: "public.st" } };
    expect(typedKeyString(["id"], { id: "a-1" }, types)).toBe('["a-1"]');
    expect(typedKeyString(["id", "n"], { id: "a-1", n: 7 }, types)).toBe('["a-1","7"]');
    expect(typedKeyString(["n"], { n: 9007199254740993 }, types)).toBeNull();
    expect(typedKeyString(["amt"], { amt: 1.5 }, types)).toBeNull();
    expect(typedKeyString(["at"], { at: "2026-10-01T00:00:00+00:00" }, types)).toBeNull();
    expect(typedKeyString(["c"], { c: "ab " }, types)).toBeNull();
    expect(typedKeyString(["s"], { s: "open" }, types)).toBe('["open"]');
    expect(typedKeyString([], { id: "x" }, types)).toBeNull();
  });

  it("comparable_ignores_modifiers_arrays_and_the_catalog_schema", () => {
    expect(isComparable({ type: "numeric(12,2)[]" })).toBe(true);
    expect(isComparable({ type: "pg_catalog.json" })).toBe(false);
    expect(isComparable({ type: " JSON " })).toBe(false);
    expect(isComparable({ type: "polygon[][]" })).toBe(false);
  });

  it("splitting_drops_the_aliases_but_keeps_a_real_column_spelled_like_one", () => {
    const got = splitTextRows([{ $pk0: 1, $pk0_: "1", amount: 5, $o0: "5" }], { pkAliases: ["$pk0_"], originalAliases: { amount: "$o0" } });
    expect(got.rows).toEqual([{ $pk0: 1, amount: 5 }]);
    expect(got.keys).toEqual(['["1"]']);
    expect(got.originals).toEqual([{ amount: "5" }]);
  });
});

describe("live rows, edges", () => {
  const k = (n: string) => JSON.stringify([n]);
  const live = { ...INITIAL_LIVE, live: true };

  it("a_struck_row_inserted_again_is_unstruck_and_refetched", () => {
    const struck = reduceLive(live, { type: "rows", seq: 1, pageKeys: new Set([k("a")]), changes: [{ op: "delete", keys: [k("a")] }] }).state;
    const r = reduceLive(struck, { type: "rows", seq: 2, pageKeys: new Set([k("a")]), changes: [{ op: "insert", keys: [k("a")] }] });
    expect(r.state.struck).toEqual([]);
    expect(r.effects).toEqual([{ type: "refetch_keys", keys: [k("a")] }]);
  });

  it("a_faded_key_a_later_refetch_returns_is_no_longer_fading", () => {
    const faded = reduceLive(live, { type: "refetched_keys", requested: [k("a")], returned: [] }).state;
    expect(reduceLive(faded, { type: "refetched_keys", requested: [k("a")], returned: [k("a")] }).state.fading).toEqual([]);
  });

  it("a_failed_by_key_read_falls_back_to_a_debounced_page_refetch", async () => {
    vi.useFakeTimers();
    try {
      const refetchPage = vi.fn();
      const c = createLiveController({ getPageKeys: () => new Set([k("a")]), refetchKeys: () => Promise.reject(new Error("boom")), refetchPage });
      c.dispatch({ type: "rows", seq: 1, changes: [{ op: "update", keys: [k("a")] }] });
      await vi.advanceTimersByTimeAsync(600);
      expect(refetchPage).toHaveBeenCalledTimes(1);
      c.dispose();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("presence, edges", () => {
  it("negative_and_fractional_seeds_stay_in_range", () => {
    for (const seed of [-1, -9, 3.7]) {
      const i = presenceColorIndex(seed);
      expect(i).toBeGreaterThanOrEqual(0);
      expect(i).toBeLessThan(8);
    }
  });

  it("three_editors_are_listed_and_one_person_in_two_tabs_once", () => {
    const at = (id: string, sub: string, name: string) => member({ id, sub, name, state: { row: '["1"]', column: "memo", editing: true } });
    expect(editingNotice([at("a", "s1", "Ana"), at("a2", "s1", "Ana"), at("b", "s2", "Bo"), at("c", "s3", "Cy")], "me", '["1"]', "memo")).toBe("Ana, Bo and Cy are editing this cell");
  });
});
