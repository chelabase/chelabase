import fc from "fast-check";
import { describe, expect, it } from "vitest";
import {
  EMPTY_QUEUE,
  MAX_CONCURRENT,
  hasActiveUploads,
  nextToStart,
  queueReducer,
  type QueueAction,
  type QueueState,
  type UploadStatus,
} from "./queue-reducer";

const file = (name: string, size = 10) => new File([new Uint8Array(size)], name);
const add = (...names: string[]): QueueAction => ({
  type: "add",
  uploads: names.map((n) => ({ id: n, file: file(n), bucket: "b", key: n })),
});
const run = (actions: QueueAction[], start: QueueState = EMPTY_QUEUE) => actions.reduce(queueReducer, start);
const statusOf = (s: QueueState, id: string) => s.items.find((i) => i.id === id)?.status;
const uploading = (s: QueueState) => s.items.filter((i) => i.status === "uploading").length;

describe("queueReducer", () => {
  it("added_files_wait_in_order_and_at_most_four_start", () => {
    const names = Array.from({ length: 10 }, (_, i) => `f${i}`);
    const s = run([add(...names)]);
    expect(s.items.map((i) => i.status)).toEqual(Array(10).fill("queued"));
    expect(MAX_CONCURRENT).toBe(4);
    expect(nextToStart(s)).toEqual(["f0", "f1", "f2", "f3"]);
    const started = run(nextToStart(s).map((id) => ({ type: "start", id }) as const), s);
    expect(uploading(started)).toBe(4);
    expect(nextToStart(started)).toEqual([]);
    // A fifth start is refused while four run.
    expect(statusOf(queueReducer(started, { type: "start", id: "f4" }), "f4")).toBe("queued");
    const one = queueReducer(started, { type: "done", id: "f1", name: "f1" });
    expect(nextToStart(one)).toEqual(["f4"]);
  });

  it("a_failure_of_one_file_leaves_the_others_running", () => {
    const s = run([add("a", "b", "c"), { type: "start", id: "a" }, { type: "start", id: "b" }]);
    const failed = queueReducer(s, { type: "fail", id: "a", error: { message: "Too large", retryable: false, conflict: false } });
    expect(statusOf(failed, "a")).toBe("failed");
    expect(failed.items.find((i) => i.id === "a")?.error?.message).toBe("Too large");
    expect(statusOf(failed, "b")).toBe("uploading");
    expect(nextToStart(failed)).toEqual(["c"]);
  });

  it("progress_tracks_bytes_and_speed_while_uploading", () => {
    const s = run([add("a"), { type: "start", id: "a", at: 0 }]);
    const p1 = queueReducer(s, { type: "progress", id: "a", loaded: 4, at: 1000 });
    expect(p1.items[0]).toMatchObject({ loaded: 4 });
    expect(p1.items[0]?.speed).toBeCloseTo(4);
    // Over the total is clamped; a paused file takes no progress.
    expect(queueReducer(p1, { type: "progress", id: "a", loaded: 99, at: 2000 }).items[0]?.loaded).toBe(10);
    const paused = queueReducer(p1, { type: "pause", id: "a" });
    expect(queueReducer(paused, { type: "progress", id: "a", loaded: 8, at: 3000 }).items[0]?.loaded).toBe(4);
  });

  it("pause_and_resume_requeue_and_keep_the_session", () => {
    const s = run([add("a"), { type: "start", id: "a" }, { type: "session", id: "a", session: "s1" }, { type: "pause", id: "a" }]);
    expect(statusOf(s, "a")).toBe("paused");
    expect(nextToStart(s)).toEqual([]);
    const resumed = queueReducer(s, { type: "resume", id: "a" });
    expect(statusOf(resumed, "a")).toBe("queued");
    expect(resumed.items[0]?.session).toBe("s1");
  });

  it("cancel_skips_the_file_and_marks_it_cancelled", () => {
    for (const before of [[], [{ type: "start", id: "a" }], [{ type: "pause", id: "a" }]] as QueueAction[][]) {
      const s = run([add("a"), ...before, { type: "cancel", id: "a" }]);
      expect(statusOf(s, "a")).toBe("skipped");
      expect(s.items[0]?.cancelled).toBe(true);
    }
    // A finished file can't be cancelled.
    const done = run([add("a"), { type: "start", id: "a" }, { type: "done", id: "a", name: "a" }, { type: "cancel", id: "a" }]);
    expect(statusOf(done, "a")).toBe("done");
  });

  it("a_conflict_waits_for_a_choice", () => {
    const conflict = { message: "taken", retryable: false, conflict: true };
    const s = run([add("a"), { type: "start", id: "a" }, { type: "fail", id: "a", error: conflict }]);
    const skipped = queueReducer(s, { type: "resolve", id: "a", choice: "skip" });
    expect(statusOf(skipped, "a")).toBe("skipped");
    expect(skipped.items[0]?.cancelled).toBe(false);
    const replace = queueReducer(s, { type: "resolve", id: "a", choice: "replace" });
    expect(replace.items[0]).toMatchObject({ status: "queued", choice: "replace", error: null });
    const both = queueReducer(s, { type: "resolve", id: "a", choice: "keep-both" });
    expect(both.items[0]).toMatchObject({ status: "queued", choice: "keep-both" });
    // A failure that isn't a conflict takes no choice.
    const other = run([add("b"), { type: "start", id: "b" }, { type: "fail", id: "b", error: { ...conflict, conflict: false } }]);
    expect(statusOf(queueReducer(other, { type: "resolve", id: "b", choice: "replace" }), "b")).toBe("failed");
  });

  it("retry_requeues_a_failed_file_and_done_records_where_it_landed", () => {
    const s = run([add("a"), { type: "start", id: "a" }, { type: "fail", id: "a", error: { message: "x", retryable: true, conflict: false } }]);
    expect(statusOf(queueReducer(s, { type: "retry", id: "a" }), "a")).toBe("queued");
    const done = run([{ type: "retry", id: "a" }, { type: "start", id: "a" }, { type: "done", id: "a", name: "a (1)" }], s);
    expect(done.items[0]).toMatchObject({ status: "done", name: "a (1)", loaded: 10, session: null });
  });

  it("clear_removes_only_finished_files", () => {
    const s = run([add("a", "b", "c"), { type: "start", id: "a" }, { type: "done", id: "a", name: "a" }, { type: "cancel", id: "b" }, { type: "clear" }]);
    expect(s.items.map((i) => i.id)).toEqual(["c"]);
  });

  it("an_id_added_twice_is_kept_once", () => {
    expect(run([add("a"), add("a")]).items).toHaveLength(1);
  });

  it("uploads_are_active_while_any_file_is_queued_uploading_or_paused", () => {
    expect(hasActiveUploads(EMPTY_QUEUE)).toBe(false);
    expect(hasActiveUploads(run([add("a")]))).toBe(true);
    expect(hasActiveUploads(run([add("a"), { type: "pause", id: "a" }]))).toBe(true);
    expect(hasActiveUploads(run([add("a"), { type: "start", id: "a" }, { type: "done", id: "a", name: "a" }]))).toBe(false);
    expect(hasActiveUploads(run([add("a"), { type: "cancel", id: "a" }]))).toBe(false);
  });
});

const STATUSES: UploadStatus[] = ["queued", "uploading", "paused", "done", "failed", "skipped"];
const IDS = ["a", "b", "c", "d", "e", "f", "g"];

const actionArb: fc.Arbitrary<QueueAction> = fc.oneof(
  fc.subarray(IDS, { minLength: 1 }).map((ids) => add(...ids)),
  fc.constantFrom(...IDS).map((id) => ({ type: "start", id }) as QueueAction),
  fc.record({ id: fc.constantFrom(...IDS), loaded: fc.nat(20), at: fc.nat(10_000) }).map((r) => ({ type: "progress", ...r }) as QueueAction),
  fc.constantFrom(...IDS).map((id) => ({ type: "done", id, name: id }) as QueueAction),
  fc.record({ id: fc.constantFrom(...IDS), conflict: fc.boolean() }).map(
    ({ id, conflict }) => ({ type: "fail", id, error: { message: "x", retryable: !conflict, conflict } }) as QueueAction,
  ),
  fc.constantFrom(...IDS).map((id) => ({ type: "session", id, session: `s-${id}` }) as QueueAction),
  fc.constantFrom("pause", "resume", "cancel", "retry").chain((type) => fc.constantFrom(...IDS).map((id) => ({ type, id }) as QueueAction)),
  fc.record({ id: fc.constantFrom(...IDS), choice: fc.constantFrom("skip", "replace", "keep-both") }).map((r) => ({ type: "resolve", ...r }) as QueueAction),
);

describe("queueReducer properties", () => {
  it("no_transition_loses_a_file_or_runs_more_than_four", () => {
    fc.assert(
      fc.property(fc.array(actionArb, { maxLength: 80 }), (actions) => {
        let s = EMPTY_QUEUE;
        const seen = new Set<string>();
        for (const a of actions) {
          if (a.type === "add") a.uploads.forEach((u) => seen.add(u.id));
          s = queueReducer(s, a);
          expect(uploading(s)).toBeLessThanOrEqual(MAX_CONCURRENT);
          expect(s.items.map((i) => i.id).sort()).toEqual([...seen].sort());
          for (const i of s.items) {
            expect(STATUSES).toContain(i.status);
            expect(i.loaded).toBeGreaterThanOrEqual(0);
            expect(i.loaded).toBeLessThanOrEqual(i.total);
            expect(i.error !== null).toBe(i.status === "failed");
          }
        }
      }),
    );
  });

  it("scheduling_from_next_to_start_finishes_every_file_within_the_limit", () => {
    fc.assert(
      fc.property(fc.integer({ min: 1, max: 30 }), fc.array(fc.nat(), { maxLength: 200 }), (n, picks) => {
        const ids = Array.from({ length: n }, (_, i) => `f${i}`);
        let s = run([add(...ids)]);
        let peak = 0;
        let k = 0;
        for (let step = 0; step < 10 * n && s.items.some((i) => i.status !== "done"); step++) {
          s = run(nextToStart(s).map((id) => ({ type: "start", id }) as const), s);
          peak = Math.max(peak, uploading(s));
          const running = s.items.filter((i) => i.status === "uploading");
          const pick = running[(picks[k++ % Math.max(1, picks.length)] ?? 0) % running.length];
          if (pick) s = queueReducer(s, { type: "done", id: pick.id, name: pick.id });
        }
        expect(s.items.every((i) => i.status === "done")).toBe(true);
        expect(peak).toBeLessThanOrEqual(MAX_CONCURRENT);
        expect(peak).toBe(Math.min(n, MAX_CONCURRENT));
      }),
    );
  });
});
