import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "@/test/fake-realtime";
import { HIDDEN_LEAVE_MS, MAX_VIEW_KEYS, SUBPROTOCOL, backoffDelay, createRealtimeClient, socketUrl, tableChannelId } from "./client";
import type { SessionEvents, TableHandlers, TableJoinParams } from "./types";

const PARAMS: TableJoinParams = { database_id: "d1", branch_id: "b1", pg_database: "app", schema: "public", table: "invoices", role: null };
const CHANNEL = '["d1","b1","app","public","invoices"]';
const ID = `table:${CHANNEL}`;
const PROJECT = "11111111-1111-4111-8111-111111111111";

class FakeDocument extends EventTarget {
  visibilityState: DocumentVisibilityState = "visible";
  setVisibility(v: DocumentVisibilityState) {
    this.visibilityState = v;
    this.dispatchEvent(new Event("visibilitychange"));
  }
}

class FakeSessionEvents implements SessionEvents {
  loaded = new Set<(u: { access_token: string }) => void>();
  unloaded = new Set<() => void>();
  signedOut = new Set<() => void>();
  addUserLoaded(cb: (u: { access_token: string }) => void) {
    this.loaded.add(cb);
  }
  removeUserLoaded(cb: (u: { access_token: string }) => void) {
    this.loaded.delete(cb);
  }
  addUserUnloaded(cb: () => void) {
    this.unloaded.add(cb);
  }
  removeUserUnloaded(cb: () => void) {
    this.unloaded.delete(cb);
  }
  addUserSignedOut(cb: () => void) {
    this.signedOut.add(cb);
  }
  removeUserSignedOut(cb: () => void) {
    this.signedOut.delete(cb);
  }
  load(token: string) {
    this.loaded.forEach((cb) => cb({ access_token: token }));
  }
}

function tickets(over: { status?: number }[] = []) {
  let n = 0;
  return vi.fn(async (_url: RequestInfo | URL, _init?: RequestInit) => {
    const i = n++;
    const status = over[i]?.status ?? 201;
    if (status !== 201) return new Response(JSON.stringify({ error: { code: "unavailable", message: "x" } }), { status });
    return new Response(JSON.stringify({ ticket: `tkt-${i + 1}`, expires_in: 30 }), { status: 201, headers: { "Content-Type": "application/json" } });
  });
}

function setup(o: { fetch?: ReturnType<typeof tickets>; random?: () => number; token?: string } = {}) {
  const fetchMock = o.fetch ?? tickets();
  const doc = new FakeDocument();
  const client = createRealtimeClient({
    apiUrl: "http://api.test",
    token: o.token ?? "tok-1",
    fetch: fetchMock as unknown as typeof fetch,
    WebSocket: FakeWebSocket,
    random: o.random ?? (() => 0),
    document: doc,
  });
  return { client, fetchMock, doc };
}

const flush = () => vi.advanceTimersByTimeAsync(0);
function handlers(): Required<TableHandlers> & Record<string, ReturnType<typeof vi.fn>> {
  return { onJoined: vi.fn(), onError: vi.fn(), onRows: vi.fn(), onStatus: vi.fn(), onResync: vi.fn(), onPresenceState: vi.fn(), onPresenceDiff: vi.fn() } as never;
}
/** Joins a table, lets the ticket arrive, opens the socket and has the server answer `joined`. */
async function joinOpen(s: ReturnType<typeof setup>, h = handlers(), params = PARAMS) {
  const sub = s.client.joinTable(params, h);
  await flush();
  const ws = FakeWebSocket.instances.at(-1)!;
  ws.open();
  await flush();
  const join = ws.frames("join").at(-1)!;
  ws.receive({ type: "joined", ref: join.ref, channel: tableChannelId(params), info: { pk: ["id"], live: true } });
  return { sub, ws, h };
}

beforeEach(() => {
  vi.useFakeTimers();
  FakeWebSocket.reset();
});
afterEach(() => vi.useRealTimers());

describe("pure helpers", () => {
  it("socketUrl_swaps_http_for_ws_and_adds_the_ticket", () => {
    expect(socketUrl("http://api.test", "t 1")).toBe("ws://api.test/v1/realtime/socket?ticket=t%201");
    expect(socketUrl("https://api.example.com", "abc")).toBe("wss://api.example.com/v1/realtime/socket?ticket=abc");
    expect(socketUrl("https://api.example.com/", "abc")).toBe("wss://api.example.com/v1/realtime/socket?ticket=abc");
  });

  it("the_channel_id_is_table_colon_compact_json_without_the_role", () => {
    expect(tableChannelId(PARAMS)).toBe(ID);
    expect(tableChannelId({ ...PARAMS, role: "billing" } as never)).toBe(ID);
  });

  it("backoff_doubles_to_30_s_with_jitter_between_half_and_full", () => {
    expect(backoffDelay(0, () => 0)).toBe(500);
    expect(backoffDelay(0, () => 1)).toBe(1000);
    expect(backoffDelay(1, () => 0)).toBe(1000);
    expect(backoffDelay(3, () => 1)).toBe(8000);
    expect(backoffDelay(10, () => 0)).toBe(15_000);
    expect(backoffDelay(50, () => 1)).toBe(30_000);
  });
});

describe("connecting", () => {
  it("opens_nothing_until_the_first_join", async () => {
    const { fetchMock } = setup();
    await flush();
    expect(fetchMock).not.toHaveBeenCalled();
    expect(FakeWebSocket.instances).toHaveLength(0);
  });

  it("asks_for_a_ticket_with_the_access_token_then_connects_with_the_subprotocol", async () => {
    const { client, fetchMock } = setup();
    client.joinTable(PARAMS, handlers());
    await flush();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const req = new Request(...(fetchMock.mock.calls[0] as [RequestInfo | URL, RequestInit?]));
    expect(req.method).toBe("POST");
    expect(new URL(req.url).pathname).toBe("/v1/realtime/tickets");
    expect(req.headers.get("authorization")).toBe("Bearer tok-1");
    expect(FakeWebSocket.instances).toHaveLength(1);
    const ws = FakeWebSocket.instances[0];
    expect(ws.url).toBe("ws://api.test/v1/realtime/socket?ticket=tkt-1");
    expect(([] as string[]).concat(ws.protocols ?? [])).toEqual([SUBPROTOCOL]);
  });

  it("sends_auth_first_the_moment_the_socket_opens_then_the_queued_joins_in_order", async () => {
    const { client } = setup();
    client.joinTable(PARAMS, handlers());
    client.joinProject(PROJECT, { onEvent: vi.fn() });
    await flush();
    const ws = FakeWebSocket.instances[0];
    ws.open();
    // Synchronously: the server closes 4001 when no auth frame comes within 5 s (R9).
    expect(ws.sent[0]).toEqual({ type: "auth", token: "tok-1" });
    await flush();
    expect(ws.sent.map((f) => f.type)).toEqual(["auth", "join", "join"]);
    expect(ws.sent[1]).toMatchObject({ type: "join", channel: "table", params: PARAMS });
    expect(typeof ws.sent[1].ref).toBe("string");
    expect(ws.sent[2]).toMatchObject({ type: "join", channel: `project:${PROJECT}` });
    expect(ws.sent[1].ref).not.toBe(ws.sent[2].ref);
  });

  it("one_socket_serves_every_join", async () => {
    const { client, fetchMock } = setup();
    client.joinTable(PARAMS, handlers());
    client.joinTable({ ...PARAMS, table: "customers" }, handlers());
    await flush();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(FakeWebSocket.instances).toHaveLength(1);
  });
});

describe("frames in", () => {
  it("joined_rows_status_resync_and_presence_reach_the_handlers", async () => {
    const s = setup();
    const { ws, h } = await joinOpen(s);
    expect(h.onJoined).toHaveBeenCalledWith({ pk: ["id"], live: true });
    ws.receive({ type: "rows", channel: ID, seq: 1, changes: [{ op: "update", keys: ['["7"]'] }, { op: "delete" }] });
    expect(h.onRows).toHaveBeenCalledWith({ seq: 1, changes: [{ op: "update", keys: ['["7"]'] }, { op: "delete" }] });
    ws.receive({ type: "status", channel: ID, live: false, reason: "no_slot" });
    expect(h.onStatus).toHaveBeenCalledWith({ live: false, reason: "no_slot" });
    ws.receive({ type: "resync", channel: ID, reason: "lease" });
    expect(h.onResync).toHaveBeenCalledWith("lease");
    const m = { id: "m1", sub: "s", name: "Ana", email: "a@x", role: null, color_seed: 3, state: { row: null, column: null, editing: false } };
    ws.receive({ type: "presence_state", channel: ID, members: [m] });
    expect(h.onPresenceState).toHaveBeenCalledWith([m]);
    ws.receive({ type: "presence_diff", channel: ID, joins: [m], leaves: [] });
    expect(h.onPresenceDiff).toHaveBeenCalledWith({ joins: [m], leaves: [] });
  });

  it("an_error_with_the_joins_ref_reaches_that_joins_handler", async () => {
    const s = setup();
    const h = handlers();
    s.client.joinTable(PARAMS, h);
    await flush();
    const ws = FakeWebSocket.instances[0];
    ws.open();
    await flush();
    ws.receive({ type: "error", ref: ws.frames("join")[0].ref, code: "forbidden", message: "nope" });
    expect(h.onError).toHaveBeenCalledWith({ code: "forbidden", message: "nope" });
    expect(h.onJoined).not.toHaveBeenCalled();
  });

  it("project_frames_deliver_the_flattened_event", async () => {
    const s = setup();
    const onEvent = vi.fn();
    s.client.joinProject(PROJECT, { onEvent });
    await flush();
    const ws = FakeWebSocket.instances[0];
    ws.open();
    await flush();
    ws.receive({ type: "joined", ref: ws.frames("join")[0].ref, channel: `project:${PROJECT}`, info: {} });
    ws.receive({ type: "project", channel: `project:${PROJECT}`, event: "branch.status", database_id: "d1", branch_id: "b1", status: "ready" });
    expect(onEvent).toHaveBeenCalledWith({ event: "branch.status", database_id: "d1", branch_id: "b1", status: "ready" });
  });

  it("garbage_and_other_channels_frames_are_ignored", async () => {
    const s = setup();
    const { ws, h } = await joinOpen(s);
    expect(() => ws.onmessage?.({ data: "not json" })).not.toThrow();
    ws.receive({ type: "rows", channel: 'table:["other"]', seq: 1, changes: [] });
    ws.receive({ type: "nonsense" });
    expect(h.onRows).not.toHaveBeenCalled();
    ws.receive({ type: "rows", channel: ID, seq: 1, changes: [] });
    expect(h.onRows).toHaveBeenCalledTimes(1);
  });
});

describe("frames out", () => {
  it("view_sends_the_key_strings_and_caps_at_1000", async () => {
    const s = setup();
    const { sub, ws } = await joinOpen(s);
    sub.view(['["1"]', '["a\\"b"]']);
    expect(ws.frames("view").at(-1)).toEqual({ type: "view", channel: ID, keys: ['["1"]', '["a\\"b"]'] });
    const many = Array.from({ length: 1500 }, (_, i) => JSON.stringify([String(i)]));
    sub.view(many);
    const sent = ws.frames("view").at(-1)!.keys as string[];
    expect(MAX_VIEW_KEYS).toBe(1000);
    expect(sent).toHaveLength(1000);
    expect(sent).toEqual(many.slice(0, 1000));
  });

  it("presence_sends_the_state_on_the_canonical_channel", async () => {
    const s = setup();
    const { sub, ws } = await joinOpen(s);
    sub.presence({ row: '["7"]', column: "memo", editing: true });
    expect(ws.frames("presence").at(-1)).toEqual({ type: "presence", channel: ID, state: { row: '["7"]', column: "memo", editing: true } });
  });

  it("leave_sends_leave_with_a_string_ref_and_stops_delivering", async () => {
    const s = setup();
    const { sub, ws, h } = await joinOpen(s);
    sub.leave();
    const leave = ws.frames("leave").at(-1)!;
    expect(leave).toMatchObject({ type: "leave", channel: ID });
    expect(typeof leave.ref).toBe("string");
    ws.receive({ type: "rows", channel: ID, seq: 1, changes: [] });
    expect(h.onRows).not.toHaveBeenCalled();
  });

  it("act_as_change_leaves_then_rejoins_with_the_new_role_in_that_order", async () => {
    const s = setup();
    const { sub, ws } = await joinOpen(s);
    const before = ws.sent.length;
    sub.leave();
    const h2 = handlers();
    s.client.joinTable({ ...PARAMS, role: "billing_admin" }, h2);
    await flush();
    const after = ws.sent.slice(before);
    expect(after.map((f) => f.type)).toEqual(["leave", "join"]);
    expect(after[1]).toMatchObject({ channel: "table", params: { ...PARAMS, role: "billing_admin" } });
    expect(after[1].ref).not.toBe(ws.frames("join")[0].ref);
    ws.receive({ type: "joined", ref: after[1].ref, channel: ID, info: { pk: ["id"], live: true } });
    expect(h2.onJoined).toHaveBeenCalledTimes(1);
  });
});

describe("renewal and sign-out (R9, W32)", () => {
  it("setToken_sends_an_auth_frame_on_the_open_socket", async () => {
    const s = setup();
    const { ws } = await joinOpen(s);
    s.client.setToken("tok-2");
    expect(ws.frames("auth").at(-1)).toEqual({ type: "auth", token: "tok-2" });
  });

  it("every_userLoaded_sends_an_auth_frame_with_the_new_token", async () => {
    const s = setup();
    const events = new FakeSessionEvents();
    s.client.attachAuth(events);
    const { ws } = await joinOpen(s);
    events.load("tok-2");
    events.load("tok-3");
    expect(ws.frames("auth").map((f) => f.token)).toEqual(["tok-1", "tok-2", "tok-3"]);
  });

  it("the_next_ticket_uses_the_renewed_token", async () => {
    const s = setup();
    const events = new FakeSessionEvents();
    s.client.attachAuth(events);
    const { ws } = await joinOpen(s);
    events.load("tok-2");
    ws.drop(1012);
    await vi.advanceTimersByTimeAsync(1000);
    const req = new Request(...(s.fetchMock.mock.calls[1] as [RequestInfo | URL, RequestInit?]));
    expect(req.headers.get("authorization")).toBe("Bearer tok-2");
    FakeWebSocket.instances.at(-1)!.open();
    await flush();
    expect(FakeWebSocket.instances.at(-1)!.frames("auth")[0].token).toBe("tok-2");
  });

  it("a_detached_listener_hears_nothing", async () => {
    const s = setup();
    const events = new FakeSessionEvents();
    const detach = s.client.attachAuth(events);
    const { ws } = await joinOpen(s);
    detach();
    expect(events.loaded.size + events.unloaded.size + events.signedOut.size).toBe(0);
    s.client.setToken("x");
    expect(ws.frames("auth").map((f) => f.token)).toEqual(["tok-1", "x"]);
  });

  it.each([["signed out"], ["unloaded"]])("%s_closes_the_socket_for_good", async (kind) => {
    const s = setup();
    const events = new FakeSessionEvents();
    s.client.attachAuth(events);
    const { ws } = await joinOpen(s);
    (kind === "unloaded" ? events.unloaded : events.signedOut).forEach((cb) => cb());
    expect(ws.closed).not.toBeNull();
    ws.drop(1000);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(FakeWebSocket.instances).toHaveLength(1);
    expect(s.fetchMock).toHaveBeenCalledTimes(1);
  });

  it("close_stops_reconnecting_and_ignores_later_joins", async () => {
    const s = setup();
    const { ws } = await joinOpen(s);
    s.client.close();
    expect(ws.closed).not.toBeNull();
    s.client.joinTable({ ...PARAMS, table: "customers" }, handlers());
    await vi.advanceTimersByTimeAsync(120_000);
    expect(FakeWebSocket.instances).toHaveLength(1);
  });

  it("close_before_the_ticket_arrives_opens_no_socket", async () => {
    const s = setup();
    s.client.joinTable(PARAMS, handlers());
    s.client.close();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(FakeWebSocket.instances).toHaveLength(0);
  });
});

describe("reconnecting", () => {
  it("a_dropped_socket_reports_reconnecting_and_reconnects_after_the_jittered_backoff", async () => {
    const s = setup({ random: () => 0 });
    const { ws, h } = await joinOpen(s);
    ws.drop(1012);
    expect(h.onStatus).toHaveBeenCalledWith({ live: false, reason: "reconnecting" });
    await vi.advanceTimersByTimeAsync(499);
    expect(FakeWebSocket.instances).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(s.fetchMock).toHaveBeenCalledTimes(2);
    expect(FakeWebSocket.instances).toHaveLength(2);
    expect(FakeWebSocket.instances[1].url).toContain("ticket=tkt-2");
  });

  it("jitter_stretches_the_delay", async () => {
    const s = setup({ random: () => 1 });
    const { ws } = await joinOpen(s);
    ws.drop(1006);
    await vi.advanceTimersByTimeAsync(999);
    expect(FakeWebSocket.instances).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(FakeWebSocket.instances).toHaveLength(2);
  });

  it("after_reconnecting_it_authenticates_rejoins_and_the_new_joined_reaches_the_handler_again", async () => {
    const s = setup();
    const { ws, h } = await joinOpen(s);
    ws.drop(1012);
    await vi.advanceTimersByTimeAsync(500);
    const ws2 = FakeWebSocket.instances[1];
    ws2.open();
    await flush();
    expect(ws2.sent.map((f) => f.type)).toEqual(["auth", "join"]);
    expect(ws2.sent[1]).toMatchObject({ channel: "table", params: PARAMS });
    ws2.receive({ type: "joined", ref: ws2.sent[1].ref, channel: ID, info: { pk: ["id"], live: true } });
    expect(h.onJoined).toHaveBeenCalledTimes(2);
  });

  it("failed_attempts_back_off_further_and_a_good_open_resets_the_counter", async () => {
    const s = setup({ random: () => 0 });
    const { ws } = await joinOpen(s);
    ws.drop(1006);
    await vi.advanceTimersByTimeAsync(500); // attempt 0
    FakeWebSocket.instances[1].drop(1006); // never opened
    await vi.advanceTimersByTimeAsync(999);
    expect(FakeWebSocket.instances).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1); // attempt 1: 1000 ms
    expect(FakeWebSocket.instances).toHaveLength(3);
    FakeWebSocket.instances[2].open();
    await flush();
    FakeWebSocket.instances[2].drop(1006);
    await vi.advanceTimersByTimeAsync(500); // back to attempt 0
    expect(FakeWebSocket.instances).toHaveLength(4);
  });

  it("a_failed_ticket_request_is_retried_with_backoff", async () => {
    const fetchMock = tickets([{ status: 503 }, { status: 201 }]);
    const s = setup({ fetch: fetchMock, random: () => 0 });
    s.client.joinTable(PARAMS, handlers());
    await flush();
    expect(FakeWebSocket.instances).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(500);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(FakeWebSocket.instances).toHaveLength(1);
  });

  it("revoked_4003_is_not_retried", async () => {
    const s = setup();
    const { ws } = await joinOpen(s);
    ws.drop(4003);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(FakeWebSocket.instances).toHaveLength(1);
  });

  it("expired_4001_reconnects_for_the_polling_fallback_to_end_when_a_fresh_token_exists", async () => {
    const s = setup();
    const { ws, h } = await joinOpen(s);
    ws.drop(4001);
    expect(h.onStatus).toHaveBeenCalledWith({ live: false, reason: "reconnecting" });
    await vi.advanceTimersByTimeAsync(500);
    expect(FakeWebSocket.instances).toHaveLength(2);
  });
});

describe("a hidden tab (P9, W23a)", () => {
  async function twoChannels(s: ReturnType<typeof setup>) {
    const h = handlers();
    s.client.joinTable(PARAMS, h);
    s.client.joinProject(PROJECT, { onEvent: vi.fn() });
    await flush();
    const ws = FakeWebSocket.instances[0];
    ws.open();
    await flush();
    for (const j of ws.frames("join")) {
      ws.receive({ type: "joined", ref: j.ref, channel: j.channel === "table" ? ID : j.channel, info: j.channel === "table" ? { pk: ["id"], live: true } : {} });
    }
    return { ws, h };
  }

  it("leaves_table_channels_only_after_5_minutes_hidden", async () => {
    const s = setup();
    const { ws } = await twoChannels(s);
    expect(HIDDEN_LEAVE_MS).toBe(5 * 60_000);
    s.doc.setVisibility("hidden");
    await vi.advanceTimersByTimeAsync(HIDDEN_LEAVE_MS - 1);
    expect(ws.frames("leave")).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(1);
    expect(ws.frames("leave").map((f) => f.channel)).toEqual([ID]);
    expect(ws.closed).toBeNull();
  });

  it("showing_the_tab_again_in_time_cancels_the_leave", async () => {
    const s = setup();
    const { ws } = await twoChannels(s);
    s.doc.setVisibility("hidden");
    await vi.advanceTimersByTimeAsync(4 * 60_000);
    s.doc.setVisibility("visible");
    await vi.advanceTimersByTimeAsync(10 * 60_000);
    expect(ws.frames("leave")).toHaveLength(0);
    expect(ws.frames("join")).toHaveLength(2);
  });

  it("a_second_hide_restarts_the_clock", async () => {
    const s = setup();
    const { ws } = await twoChannels(s);
    s.doc.setVisibility("hidden");
    await vi.advanceTimersByTimeAsync(4 * 60_000);
    s.doc.setVisibility("visible");
    s.doc.setVisibility("hidden");
    await vi.advanceTimersByTimeAsync(4 * 60_000);
    expect(ws.frames("leave")).toHaveLength(0);
  });

  it("showing_the_tab_after_the_leave_rejoins_and_the_new_joined_makes_the_handler_refetch", async () => {
    const s = setup();
    const { ws, h } = await twoChannels(s);
    s.doc.setVisibility("hidden");
    await vi.advanceTimersByTimeAsync(HIDDEN_LEAVE_MS);
    s.doc.setVisibility("visible");
    await flush();
    const joins = ws.frames("join");
    expect(joins).toHaveLength(3); // the table joined again; the project was never left
    const again = joins.at(-1)!;
    expect(again).toMatchObject({ channel: "table", params: PARAMS });
    expect(again.ref).not.toBe(joins[0].ref);
    ws.receive({ type: "joined", ref: again.ref, channel: ID, info: { pk: ["id"], live: true } });
    expect(h.onJoined).toHaveBeenCalledTimes(2);
  });

  it("a_subscription_left_by_the_caller_is_not_rejoined_when_the_tab_returns", async () => {
    const s = setup();
    const h = handlers();
    const sub = s.client.joinTable(PARAMS, h);
    await flush();
    const ws = FakeWebSocket.instances[0];
    ws.open();
    await flush();
    sub.leave();
    s.doc.setVisibility("hidden");
    await vi.advanceTimersByTimeAsync(HIDDEN_LEAVE_MS);
    s.doc.setVisibility("visible");
    await flush();
    expect(ws.frames("join")).toHaveLength(1);
  });
});
