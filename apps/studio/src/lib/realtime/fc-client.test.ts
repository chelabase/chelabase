import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "@/test/fake-realtime";
import { createRealtimeClient } from "./client";
import type { TableJoinParams } from "./types";

// RT1 final round, batch C, item 2: a transient join refusal is retried with backoff; only
// `forbidden` and `not_found` stick.

const PARAMS: TableJoinParams = { database_id: "d1", branch_id: "b1", pg_database: "app", schema: "public", table: "invoices", role: null };

function setup() {
  const fetchMock = vi.fn(async () => new Response(JSON.stringify({ ticket: "tkt", expires_in: 30 }), { status: 201, headers: { "Content-Type": "application/json" } }));
  const client = createRealtimeClient({
    apiUrl: "http://api.test",
    token: "tok-1",
    fetch: fetchMock as unknown as typeof fetch,
    WebSocket: FakeWebSocket,
    random: () => 0,
    document: { visibilityState: "visible", addEventListener() {}, removeEventListener() {} } as never,
  });
  return client;
}

const flush = () => vi.advanceTimersByTimeAsync(0);
const allJoins = () => FakeWebSocket.instances.flatMap((w) => w.frames("join"));
/** Opens a socket the client has just dialled (the server accepts the upgrade). */
async function openNew() {
  for (const ws of FakeWebSocket.instances) if (ws.readyState === 0) ws.open();
  await flush();
}
/** The server refuses every join it hasn't answered yet. */
function refuseAll(code: string, answered: Set<unknown>) {
  for (const ws of FakeWebSocket.instances) {
    if (ws.readyState !== 1) continue;
    for (const j of ws.frames("join")) {
      if (answered.has(j.ref)) continue;
      answered.add(j.ref);
      ws.receive({ type: "error", ref: j.ref, code, message: "refused" });
    }
  }
}

async function joinAndRefuse(code: string) {
  const client = setup();
  const sub = client.joinTable(PARAMS, { onError: vi.fn() });
  await flush();
  await openNew();
  const answered = new Set<unknown>();
  refuseAll(code, answered);
  return { client, sub, answered };
}

beforeEach(() => {
  vi.useFakeTimers();
  FakeWebSocket.reset();
});
afterEach(() => vi.useRealTimers());

describe("join refusals (review I2)", () => {
  it.each(["unavailable", "limit"])("a_%s_refusal_is_joined_again_after_a_backoff", async (code) => {
    await joinAndRefuse(code);
    expect(allJoins()).toHaveLength(1);
    // Not a tight loop: nothing within the shortest backoff.
    await vi.advanceTimersByTimeAsync(400);
    expect(allJoins()).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(30_000);
    await openNew();
    expect(allJoins().length).toBeGreaterThanOrEqual(2);
  });

  it.each(["forbidden", "not_found"])("a_%s_refusal_sticks_even_across_a_reconnect", async (code) => {
    await joinAndRefuse(code);
    await vi.advanceTimersByTimeAsync(5 * 60_000);
    await openNew();
    expect(allJoins()).toHaveLength(1);
    FakeWebSocket.instances[0].drop(1006);
    await vi.advanceTimersByTimeAsync(60_000);
    await openNew();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(allJoins()).toHaveLength(1);
  });

  it("repeated_transient_refusals_back_off_and_do_not_hammer_the_server", async () => {
    await joinAndRefuse("unavailable");
    const answered = new Set<unknown>(allJoins().map((j) => j.ref));
    // Five seconds of a server that refuses every join as soon as it arrives.
    for (let t = 0; t < 50; t++) {
      await vi.advanceTimersByTimeAsync(100);
      await openNew();
      refuseAll("unavailable", answered);
    }
    // Doubling delays (0.5 s, 1 s, 2 s, ...) allow at most four joins in the first five seconds.
    expect(allJoins().length).toBeLessThanOrEqual(4);
    expect(allJoins().length).toBeGreaterThanOrEqual(2);
  });

  it("leaving_cancels_a_pending_retry", async () => {
    const { sub } = await joinAndRefuse("unavailable");
    sub.leave();
    await vi.advanceTimersByTimeAsync(5 * 60_000);
    await openNew();
    expect(allJoins()).toHaveLength(1);
  });
});
