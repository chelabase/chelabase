import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket, member } from "@/test/fake-realtime";
import { createRealtimeClient } from "./client";
import { conflictWhere } from "./conflict";
import { insertedKey } from "./keys";
import { people } from "./presence";
import type { TableJoinParams } from "./types";

// RT1 final round, batch C: the coder's edge cases beyond the RED set.

describe("insertedKey (the canonical key comes from the server)", () => {
  const t = (type: string) => ({ id: { type } });
  const at = { schema: "public", table: "t", pk: ["id"] };

  it("a_text_like_key_is_exact_and_needs_no_read", async () => {
    const send = vi.fn();
    expect(await insertedKey({ ...at, row: { id: "a b" }, types: t("text"), send })).toBe(JSON.stringify(["a b"]));
    expect(send).not.toHaveBeenCalled();
  });

  it("other_types_read_by_the_json_text_with_parse_and_adopt_the_servers_pk_text", async () => {
    const send = vi.fn(async () => [{ id: "2026-10-16T12:00:00.5+05:30", $pk0: "2026-10-16 12:00:00.5+05:30" }]);
    const key = await insertedKey({ ...at, row: { id: "2026-10-16T12:00:00.5+05:30" }, types: t("timestamp with time zone"), send });
    expect(key).toBe(JSON.stringify(["2026-10-16 12:00:00.5+05:30"]));
    const tree = (send.mock.calls[0] as unknown[])[0] as { where: unknown; select: unknown[]; limit: number };
    expect(tree.where).toEqual({ col: "id", eq: "2026-10-16T12:00:00.5+05:30", parse: true });
    expect(tree.select).toContainEqual({ text_of: "id", as: "$pk0" });
    expect(tree.limit).toBe(1);
  });

  it("a_number_is_looked_up_by_its_plain_string_never_reformatted", async () => {
    const send = vi.fn(async () => [{ id: 3, $pk0: "3.000" }]);
    expect(await insertedKey({ ...at, row: { id: 3 }, types: t("numeric(8,3)"), send })).toBe(JSON.stringify(["3.000"]));
    expect(((send.mock.calls[0] as unknown[])[0] as { where: { eq: string } }).where.eq).toBe("3");
  });

  it("a_composite_key_reads_each_column_by_parse", async () => {
    const send = vi.fn(async () => [{ d: "2026-10-16", n: 4, $pk0: "10-16-2026", $pk1: "4" }]);
    const key = await insertedKey({ schema: "public", table: "t", pk: ["d", "n"], row: { d: "2026-10-16", n: 4 }, types: { d: { type: "date" }, n: { type: "numeric" } }, send });
    expect(key).toBe(JSON.stringify(["10-16-2026", "4"]));
  });

  it("no_row_back_or_a_non_scalar_key_gives_no_key", async () => {
    expect(await insertedKey({ ...at, row: { id: "2026-10-16" }, types: t("date"), send: async () => [] })).toBeNull();
    const send = vi.fn();
    expect(await insertedKey({ ...at, row: { id: [1] }, types: { id: { type: "integer", array: true } }, send })).toBeNull();
    expect(await insertedKey({ ...at, row: { id: null }, types: t("date"), send })).toBeNull();
    expect(await insertedKey({ ...at, row: { id: { a: 1 } }, types: t("jsonb"), send })).toBeNull();
    expect(send).not.toHaveBeenCalled();
  });
});


describe("people (one avatar per member)", () => {
  const m = (id: string, sub: string) => member({ id, sub, name: sub });

  it("dedupes_by_sub_in_first_seen_order_and_leaves_out_the_viewer", () => {
    expect(people([m("1", "a"), m("2", "me"), m("3", "b"), m("4", "a")], "me").map((x) => x.id)).toEqual(["1", "3"]);
  });

  it("without_a_known_viewer_every_member_counts_once", () => {
    expect(people([m("1", "a"), m("2", "a")], "")).toHaveLength(1);
  });
});

describe("conflictWhere with a key column", () => {
  it("a_key_columns_original_is_never_added_to_the_where", () => {
    const r = conflictWhere({ pk: ["id"], key: JSON.stringify(["1"]), edited: ["id", "memo"], originals: { id: "1", memo: "m" } });
    expect(r.where).toEqual({ and: [{ col: "id", eq: "1", parse: true }, { col: "memo", eq: "m", text: true }] });
    expect(r.unchecked).toEqual([]);
  });
});

describe("join retries (edges)", () => {
  const PARAMS: TableJoinParams = { database_id: "d1", branch_id: "b1", pg_database: "app", schema: "public", table: "invoices", role: null };
  const flush = () => vi.advanceTimersByTimeAsync(0);
  const joins = () => FakeWebSocket.instances.flatMap((w) => w.frames("join"));
  async function openNew() {
    for (const ws of FakeWebSocket.instances) if (ws.readyState === 0) ws.open();
    await flush();
  }
  function setup() {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({ ticket: "tkt", expires_in: 30 }), { status: 201, headers: { "Content-Type": "application/json" } }));
    return createRealtimeClient({
      apiUrl: "http://api.test",
      token: "tok-1",
      fetch: fetchMock as unknown as typeof fetch,
      WebSocket: FakeWebSocket,
      random: () => 0,
      document: { visibilityState: "visible", addEventListener() {}, removeEventListener() {} } as never,
    });
  }
  const last = () => FakeWebSocket.instances.at(-1)!;

  beforeEach(() => {
    vi.useFakeTimers();
    FakeWebSocket.reset();
  });
  afterEach(() => vi.useRealTimers());

  it("a_retry_pending_across_a_reconnect_joins_once_not_twice", async () => {
    const client = setup();
    client.joinTable(PARAMS, {});
    await flush();
    await openNew();
    last().receive({ type: "error", ref: joins()[0].ref, code: "unavailable", message: "x" });
    await vi.advanceTimersByTimeAsync(500);
    expect(joins()).toHaveLength(2);
    // The second retry is due in 1 s.
    last().receive({ type: "error", ref: joins()[1].ref, code: "unavailable", message: "x" });
    // The socket drops and comes back (0.5 s) before it: the reconnect doesn't join, the retry does.
    last().drop(1006);
    await vi.advanceTimersByTimeAsync(500);
    await openNew();
    expect(FakeWebSocket.instances).toHaveLength(2);
    expect(joins()).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(600);
    expect(last().frames("join")).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(joins()).toHaveLength(3);
  });

  it("a_joined_resets_the_backoff", async () => {
    const client = setup();
    client.joinTable(PARAMS, {});
    await flush();
    await openNew();
    const ws = last();
    ws.receive({ type: "error", ref: joins()[0].ref, code: "unavailable", message: "x" });
    await vi.advanceTimersByTimeAsync(500);
    ws.receive({ type: "error", ref: joins()[1].ref, code: "unavailable", message: "x" });
    await vi.advanceTimersByTimeAsync(1000);
    expect(joins()).toHaveLength(3);
    ws.receive({ type: "joined", ref: joins()[2].ref, channel: "c", info: { pk: ["id"], live: true } });
    ws.receive({ type: "error", ref: undefined, channel: "c", code: "unavailable", message: "x" });
    // A channel error (no ref) is no refusal: nothing is retried.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(joins()).toHaveLength(3);
  });

  it("close_cancels_a_pending_retry", async () => {
    const client = setup();
    client.joinTable(PARAMS, {});
    await flush();
    await openNew();
    last().receive({ type: "error", ref: joins()[0].ref, code: "limit", message: "x" });
    client.close();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(joins()).toHaveLength(1);
  });
});
