import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  INITIAL_LIVE,
  KEYLESS_DEBOUNCE_MS,
  KEYLESS_MAX_MS,
  KEYLESS_MIN_MS,
  createLiveController,
  createRefetchGate,
  reduceLive,
  type LiveEvent,
  type LiveState,
} from "./live-rows";

const k = (n: string) => JSON.stringify([n]);
const page = (...ns: string[]) => new Set(ns.map(k));

function run(state: LiveState, ...events: LiveEvent[]) {
  let s = state;
  const effects: ReturnType<typeof reduceLive>["effects"] = [];
  for (const e of events) {
    const r = reduceLive(s, e);
    s = r.state;
    effects.push(...r.effects);
  }
  return { state: s, effects };
}
const joined = (live = true): LiveEvent => ({ type: "joined", live });
/** The state right after `joined {live: true}`. */
const live1: LiveState = { live: true, seq: 0, newKeys: [], struck: [], fading: [] };

describe("reduceLive: rows frames", () => {
  it("constants_follow_R15", () => {
    expect([KEYLESS_DEBOUNCE_MS, KEYLESS_MIN_MS, KEYLESS_MAX_MS]).toEqual([500, 2000, 10_000]);
  });

  it("keys_on_the_page_are_refetched_once_by_key", () => {
    const r = reduceLive(live1, { type: "rows", seq: 1, pageKeys: page("a", "b", "c"), changes: [{ op: "update", keys: [k("a")] }, { op: "insert", keys: [k("c")] }] });
    expect(r.effects).toEqual([{ type: "refetch_keys", keys: [k("a"), k("c")] }]);
    expect(r.state.seq).toBe(1);
    expect(r.state.newKeys).toEqual([]);
  });

  it("the_same_key_in_two_changes_is_refetched_once", () => {
    const r = reduceLive(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [{ op: "update", keys: [k("a")] }, { op: "update", keys: [k("a")] }] });
    expect(r.effects).toEqual([{ type: "refetch_keys", keys: [k("a")] }]);
  });

  it("an_update_of_a_key_off_the_page_does_nothing", () => {
    const r = reduceLive(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [{ op: "update", keys: [k("zzz")] }] });
    expect(r.effects).toEqual([]);
    expect(r.state.newKeys).toEqual([]);
  });

  it("an_insert_off_the_page_counts_as_a_new_row", () => {
    const r = run(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [{ op: "insert", keys: [k("n1"), k("n2")] }] }, { type: "rows", seq: 2, pageKeys: page("a"), changes: [{ op: "insert", keys: [k("n2"), k("n3")] }] });
    expect(r.effects).toEqual([]);
    expect(r.state.newKeys).toEqual([k("n1"), k("n2"), k("n3")]);
  });

  it("a_keyed_delete_strikes_a_page_row_through", () => {
    const r = reduceLive(live1, { type: "rows", seq: 1, pageKeys: page("a", "b"), changes: [{ op: "delete", keys: [k("b")] }] });
    expect(r.state.struck).toEqual([k("b")]);
    expect(r.effects).toEqual([]);
  });

  it("a_keyed_delete_off_the_page_is_ignored_and_forgets_a_new_row", () => {
    const withNew = run(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [{ op: "insert", keys: [k("n1")] }] }).state;
    const r = reduceLive(withNew, { type: "rows", seq: 2, pageKeys: page("a"), changes: [{ op: "delete", keys: [k("n1")] }, { op: "delete", keys: [k("other")] }] });
    expect(r.state.struck).toEqual([]);
    expect(r.state.newKeys).toEqual([]);
    expect(r.effects).toEqual([]);
  });

  it("a_keyless_delete_refetches_the_page_debounced", () => {
    const r = reduceLive(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [{ op: "delete" }] });
    expect(r.effects).toEqual([{ type: "refetch_page", mode: "debounced" }]);
  });

  it("changed_refetches_the_page_debounced", () => {
    const r = reduceLive(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [{ op: "changed" }] });
    expect(r.effects).toEqual([{ type: "refetch_page", mode: "debounced" }]);
  });

  it("a_page_refetch_in_the_same_frame_supersedes_the_by_key_refetch", () => {
    const r = reduceLive(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [{ op: "update", keys: [k("a")] }, { op: "changed" }] });
    expect(r.effects).toEqual([{ type: "refetch_page", mode: "debounced" }]);
  });

  it("a_seq_gap_is_a_debounced_page_refetch_and_the_new_seq_is_kept", () => {
    const r = run(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [] }, { type: "rows", seq: 3, pageKeys: page("a"), changes: [{ op: "update", keys: [k("a")] }] });
    expect(r.effects).toEqual([{ type: "refetch_page", mode: "debounced" }]);
    expect(r.state.seq).toBe(3);
  });

  it("the_first_rows_frame_must_be_seq_1", () => {
    const r = reduceLive(live1, { type: "rows", seq: 2, pageKeys: page("a"), changes: [] });
    expect(r.effects).toEqual([{ type: "refetch_page", mode: "debounced" }]);
  });

  it("a_repeated_or_older_seq_is_also_a_gap", () => {
    const r = run(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [] }, { type: "rows", seq: 1, pageKeys: page("a"), changes: [] });
    expect(r.effects).toEqual([{ type: "refetch_page", mode: "debounced" }]);
  });

  it("resync_refetches_the_page_debounced", () => {
    expect(reduceLive(live1, { type: "resync" }).effects).toEqual([{ type: "refetch_page", mode: "debounced" }]);
  });
});

describe("reduceLive: joins, status and the page", () => {
  it("joined_live_refetches_at_once_R8_and_resets_seq", () => {
    const seen = run(live1, { type: "rows", seq: 1, pageKeys: page("a"), changes: [{ op: "insert", keys: [k("n")] }] }).state;
    const r = reduceLive(seen, joined(true));
    expect(r.effects).toEqual([{ type: "refetch_page", mode: "now" }]);
    expect(r.state).toMatchObject({ live: true, seq: 0, newKeys: [], struck: [] });
  });

  it("joined_not_live_does_not_refetch", () => {
    const r = reduceLive(INITIAL_LIVE, joined(false));
    expect(r.effects).toEqual([]);
    expect(r.state.live).toBe(false);
  });

  it("status_live_true_refetches_at_once_R8", () => {
    const paused = run(live1, { type: "status", live: false }).state;
    expect(paused.live).toBe(false);
    const r = reduceLive(paused, { type: "status", live: true });
    expect(r.effects).toEqual([{ type: "refetch_page", mode: "now" }]);
    expect(r.state.live).toBe(true);
  });

  it("status_live_false_pauses_without_a_refetch", () => {
    const r = reduceLive(live1, { type: "status", live: false });
    expect(r.effects).toEqual([]);
    expect(r.state.live).toBe(false);
  });

  it("a_by_key_refetch_that_returns_nothing_for_a_key_fades_it", () => {
    const r = reduceLive(live1, { type: "refetched_keys", requested: [k("a"), k("b")], returned: [k("b")] });
    expect(r.state.fading).toEqual([k("a")]);
  });

  it("a_page_load_clears_chip_strikes_and_fades", () => {
    const dirty: LiveState = { ...live1, newKeys: [k("n")], struck: [k("b")], fading: [k("a")] };
    const r = reduceLive(dirty, { type: "page_loaded" });
    expect(r.state).toMatchObject({ newKeys: [], struck: [], fading: [], live: true });
  });

  it("leaving_resets_to_the_initial_state", () => {
    expect(reduceLive({ ...live1, seq: 4, newKeys: [k("n")] }, { type: "left" }).state).toEqual(INITIAL_LIVE);
  });

  it("the_reducer_does_not_mutate_its_input", () => {
    const frozen = Object.freeze({ ...live1, newKeys: Object.freeze([k("n")]) as unknown as string[] });
    expect(() => reduceLive(frozen, { type: "rows", seq: 1, pageKeys: page("a"), changes: [{ op: "insert", keys: [k("m")] }, { op: "delete", keys: [k("a")] }] })).not.toThrow();
  });
});

describe("the keyless refetch gate (R15)", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
  });
  afterEach(() => vi.useRealTimers());

  const at = (ms: number) => vi.setSystemTime(ms);
  const advance = (to: number) => vi.advanceTimersByTime(to - Date.now());

  it("debounces_500_ms_and_coalesces_requests", () => {
    const run = vi.fn();
    const gate = createRefetchGate(run);
    gate.request();
    advance(499);
    expect(run).not.toHaveBeenCalled();
    gate.request();
    advance(500);
    expect(run).toHaveBeenCalledTimes(1);
  });

  it("spaces_runs_2_s_apart_then_backs_off_to_10_s_while_requests_keep_arriving", () => {
    const times: number[] = [];
    const gate = createRefetchGate(() => times.push(Date.now()));
    gate.request(); // t=0 -> 500
    advance(600);
    gate.request(); // throttled -> 500+2000
    advance(2600);
    gate.request(); // -> 2500+4000
    advance(6600);
    gate.request(); // -> 6500+8000
    advance(14_600);
    gate.request(); // -> 14500+10000 (capped)
    advance(30_000);
    expect(times).toEqual([500, 2500, 6500, 14_500, 24_500]);
  });

  it("goes_back_to_2_s_after_a_quiet_stretch", () => {
    const times: number[] = [];
    const gate = createRefetchGate(() => times.push(Date.now()));
    gate.request();
    advance(600);
    gate.request();
    advance(2600);
    gate.request();
    advance(40_000);
    gate.request();
    advance(41_000);
    expect(times.at(-1)).toBe(40_500);
  });

  it("runNow_runs_at_once_and_counts_for_the_spacing", () => {
    const times: number[] = [];
    const gate = createRefetchGate(() => times.push(Date.now()));
    at(1000);
    gate.runNow();
    gate.request();
    advance(10_000);
    expect(times).toEqual([1000, 3000]);
  });

  it("cancel_drops_a_pending_run", () => {
    const run = vi.fn();
    const gate = createRefetchGate(run);
    gate.request();
    gate.cancel();
    advance(5000);
    expect(run).not.toHaveBeenCalled();
  });
});

describe("createLiveController", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
  });
  afterEach(() => vi.useRealTimers());

  function setup(pageKeys = page("a", "b")) {
    const refetchKeys = vi.fn(async (keys: string[]) => keys);
    const refetchPage = vi.fn();
    const onChange = vi.fn();
    const c = createLiveController({ getPageKeys: () => pageKeys, refetchKeys, refetchPage, onChange });
    return { c, refetchKeys, refetchPage, onChange };
  }

  it("joined_live_refetches_the_page_immediately", () => {
    const { c, refetchPage } = setup();
    c.dispatch({ type: "joined", live: true });
    expect(refetchPage).toHaveBeenCalledTimes(1);
    c.dispose();
  });

  it("keys_on_the_page_make_one_by_key_refetch_and_no_page_refetch", () => {
    const { c, refetchKeys, refetchPage } = setup();
    c.dispatch({ type: "joined", live: false });
    c.dispatch({ type: "rows", seq: 1, changes: [{ op: "update", keys: [k("a")] }, { op: "update", keys: [k("b")] }] });
    expect(refetchKeys).toHaveBeenCalledTimes(1);
    expect(refetchKeys).toHaveBeenCalledWith([k("a"), k("b")]);
    vi.advanceTimersByTime(60_000);
    expect(refetchPage).not.toHaveBeenCalled();
    c.dispose();
  });

  it("a_key_the_refetch_did_not_return_fades", async () => {
    const { c, refetchKeys, onChange } = setup();
    refetchKeys.mockResolvedValueOnce([k("b")]);
    c.dispatch({ type: "joined", live: false });
    c.dispatch({ type: "rows", seq: 1, changes: [{ op: "update", keys: [k("a")] }, { op: "update", keys: [k("b")] }] });
    await vi.waitFor(() => expect(c.state().fading).toEqual([k("a")]));
    expect(onChange).toHaveBeenCalled();
    c.dispose();
  });

  it("a_keyless_delete_refetches_the_page_after_the_debounce", () => {
    const { c, refetchPage } = setup();
    c.dispatch({ type: "joined", live: false });
    c.dispatch({ type: "rows", seq: 1, changes: [{ op: "delete" }] });
    vi.advanceTimersByTime(499);
    expect(refetchPage).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(refetchPage).toHaveBeenCalledTimes(1);
    c.dispose();
  });

  it("dispose_cancels_a_pending_refetch", () => {
    const { c, refetchPage } = setup();
    c.dispatch({ type: "resync" });
    c.dispose();
    vi.advanceTimersByTime(60_000);
    expect(refetchPage).not.toHaveBeenCalled();
  });
});
