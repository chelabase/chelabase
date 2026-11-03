import { ApiError } from "@chelabase/management-client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createSession, realtimeAuthEvents, type MintedToken } from "./session";

const unauthorized = () => new ApiError(401, "unauthorized", "Not signed in");

/** A mint that answers `tokens` in order and counts its calls. */
function minter(...answers: (MintedToken | Error)[]) {
  let i = 0;
  const mint = vi.fn(async () => {
    const a = answers[Math.min(i++, answers.length - 1)]!;
    if (a instanceof Error) throw a;
    return a;
  });
  return mint;
}
const tok = (access_token: string, expires_in = 300): MintedToken => ({ access_token, expires_in });

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("session", () => {
  it("start_keeps_the_token_in_memory", async () => {
    const session = createSession({ mint: minter(tok("t1")) });
    expect(await session.start()).toBe(true);
    expect(session.snapshot()).toEqual({ status: "signed-in", token: "t1" });
    expect(session.token()).toBe("t1");
    expect(JSON.stringify({ ...localStorage })).not.toContain("t1");
    expect(JSON.stringify({ ...sessionStorage })).not.toContain("t1");
  });

  it("start_with_no_session_is_signed_out_not_an_error", async () => {
    const session = createSession({ mint: minter(unauthorized()) });
    expect(await session.start()).toBe(false);
    expect(session.snapshot()).toEqual({ status: "signed-out", token: undefined });
  });

  it("single_flight_refresh", async () => {
    let release!: (t: MintedToken) => void;
    const mint = vi.fn(() => new Promise<MintedToken>((r) => (release = r)));
    const session = createSession({ mint });
    const calls = [session.refresh(), session.refresh(), session.refresh()];
    expect(mint).toHaveBeenCalledTimes(1);
    release(tok("t1"));
    expect(await Promise.all(calls)).toEqual(["t1", "t1", "t1"]);
    expect(mint).toHaveBeenCalledTimes(1);
  });

  it("refresh_before_expiry", async () => {
    const mint = minter(tok("t1", 300), tok("t2", 300));
    const session = createSession({ mint });
    await session.start();
    // 60 s before the 300 s expiry the timer fires: exactly one more call.
    await vi.advanceTimersByTimeAsync(239_000);
    expect(mint).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(mint).toHaveBeenCalledTimes(2);
    expect(session.token()).toBe("t2");
  });

  it("getToken_inside_the_window_refreshes_once_for_concurrent_callers", async () => {
    const mint = minter(tok("t1", 300), tok("t2", 300));
    const session = createSession({ mint });
    await session.start();
    vi.setSystemTime(Date.now() + 250_000); // 50 s left, timer not yet run
    const [a, b, c] = await Promise.all([session.getToken(), session.getToken(), session.getToken()]);
    expect([a, b, c]).toEqual(["t2", "t2", "t2"]);
    expect(mint).toHaveBeenCalledTimes(2);
  });

  it("getToken_with_time_left_does_not_call", async () => {
    const mint = minter(tok("t1", 300));
    const session = createSession({ mint });
    await session.start();
    expect(await session.getToken()).toBe("t1");
    expect(mint).toHaveBeenCalledTimes(1);
  });

  it("a_401_on_refresh_signs_out_and_tells_listeners_once", async () => {
    const session = createSession({ mint: minter(tok("t1", 120), unauthorized()) });
    const seen: (string | null)[] = [];
    session.onChange((t) => seen.push(t));
    await session.start();
    await vi.advanceTimersByTimeAsync(61_000);
    expect(session.snapshot().status).toBe("signed-out");
    expect(session.token()).toBeUndefined();
    expect(seen).toEqual(["t1", null]);
    // No timer is left behind to try again.
    await vi.advanceTimersByTimeAsync(600_000);
    expect(seen).toEqual(["t1", null]);
  });

  it("a_transient_failure_keeps_the_token_and_retries_later_not_in_a_loop", async () => {
    const down = new ApiError(503, "unavailable", "down");
    const mint = minter(tok("t1", 120), down, tok("t2", 300));
    const session = createSession({ mint });
    await session.start();
    await vi.advanceTimersByTimeAsync(61_000); // the refresh fails
    expect(mint).toHaveBeenCalledTimes(2);
    expect(session.token()).toBe("t1");
    expect(session.snapshot().status).toBe("signed-in");
    await vi.advanceTimersByTimeAsync(1_000); // far less than the retry delay: no hot loop
    expect(mint).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(30_000);
    expect(mint).toHaveBeenCalledTimes(3);
    expect(session.token()).toBe("t2");
  });

  it("clear_drops_the_token_and_stops_refreshing", async () => {
    const mint = minter(tok("t1", 120));
    const session = createSession({ mint });
    await session.start();
    session.clear();
    expect(session.token()).toBeUndefined();
    await vi.advanceTimersByTimeAsync(600_000);
    expect(mint).toHaveBeenCalledTimes(1);
  });

  it("an_in_flight_refresh_that_lands_after_clear_is_dropped", async () => {
    let release!: (t: MintedToken) => void;
    const session = createSession({ mint: () => new Promise<MintedToken>((r) => (release = r)) });
    const p = session.refresh().catch(() => undefined);
    session.clear();
    release(tok("late"));
    await p;
    expect(session.token()).toBeUndefined();
    expect(session.snapshot().status).toBe("signed-out");
  });

  it("realtime_events_follow_the_session", async () => {
    const session = createSession({ mint: minter(tok("t1", 120), tok("t2", 120)) });
    const events = realtimeAuthEvents(session);
    const loaded = vi.fn();
    const out = vi.fn();
    events.addUserLoaded(loaded);
    events.addUserSignedOut(out);
    await session.start();
    expect(loaded).toHaveBeenLastCalledWith({ access_token: "t1" });
    await vi.advanceTimersByTimeAsync(61_000);
    expect(loaded).toHaveBeenLastCalledWith({ access_token: "t2" });
    session.clear();
    expect(out).toHaveBeenCalledTimes(1);
    events.removeUserLoaded(loaded);
    events.removeUserSignedOut(out);
    await session.start().catch(() => undefined);
    expect(loaded).toHaveBeenCalledTimes(2);
  });
});
