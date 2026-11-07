import { act, renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { storageChannelId } from "@/lib/realtime/client";
import { RealtimeContext } from "@/lib/realtime/context";
import { KEYLESS_DEBOUNCE_MS, KEYLESS_MIN_MS } from "@/lib/realtime/live-rows";
import type { StatusReason } from "@/lib/realtime/types";
import { FakeRealtime } from "@/test/fake-realtime";
import { objectKey, useLiveObjects, viewIds, type LiveTarget } from "./live-objects";

const TARGET: LiveTarget = { databaseId: "d1", branchId: "b1", pgDatabase: "app", bucketId: "bk-1", role: "authenticated" };
const O1 = "00000000-0000-4000-8000-000000000001";
const O2 = "00000000-0000-4000-8000-000000000002";

function setup(rt: FakeRealtime | null, initial: { target: LiveTarget; ids: string[] } = { target: TARGET, ids: [] }) {
  const refetch = vi.fn();
  const wrapper = ({ children }: { children: ReactNode }) => <RealtimeContext.Provider value={rt}>{children}</RealtimeContext.Provider>;
  const hook = renderHook((p: { target: LiveTarget; ids: string[] }) => useLiveObjects(p.target, p.ids, refetch), { wrapper, initialProps: initial });
  return { refetch, ...hook };
}

describe("useLiveObjects", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("joins_the_buckets_storage_channel_as_the_role", () => {
    const rt = new FakeRealtime();
    setup(rt);
    expect(rt.storage.params).toEqual({ database_id: "d1", branch_id: "b1", pg_database: "app", bucket_id: "bk-1", role: "authenticated" });
    expect(rt.storage.channel).toBe(storageChannelId({ database_id: "d1", branch_id: "b1", pg_database: "app", bucket_id: "bk-1" }));
  });

  it("without_realtime_there_is_no_live_state", () => {
    const { result } = setup(null);
    expect(result.current).toBeNull();
  });

  // AC 1: another member's change is a key: the listing (current folder, search, sort) is read again.
  it("re_lists_on_a_key", async () => {
    const rt = new FakeRealtime();
    const { refetch, result } = setup(rt);
    act(() => rt.storage.handlers.onJoined?.({ pk: ["id"], live: true }));
    expect(result.current).toEqual({ live: true });
    // The join read it at once; the next read is spaced from that one, as the table editor's.
    expect(refetch).toHaveBeenCalledTimes(1);
    await act(() => vi.advanceTimersByTimeAsync(KEYLESS_MIN_MS));
    refetch.mockClear();
    act(() => rt.storage.handlers.onRows?.({ seq: 1, changes: [{ op: "insert", keys: [objectKey(O1)] }] }));
    expect(refetch).not.toHaveBeenCalled();
    await act(() => vi.advanceTimersByTimeAsync(KEYLESS_DEBOUNCE_MS));
    expect(refetch).toHaveBeenCalledTimes(1);
  });

  // A bulk or keyless `changed` reloads the listing too.
  it("reloads_on_changed_and_on_resync", async () => {
    const rt = new FakeRealtime();
    const { refetch } = setup(rt);
    act(() => rt.storage.handlers.onJoined?.({ pk: ["id"], live: true }));
    await act(() => vi.advanceTimersByTimeAsync(KEYLESS_MIN_MS));
    refetch.mockClear();
    act(() => rt.storage.handlers.onRows?.({ seq: 1, changes: [{ op: "changed" }] }));
    await act(() => vi.advanceTimersByTimeAsync(KEYLESS_DEBOUNCE_MS));
    expect(refetch).toHaveBeenCalledTimes(1);
    act(() => rt.storage.handlers.onResync?.("overflow"));
    await act(() => vi.advanceTimersByTimeAsync(10_000));
    expect(refetch).toHaveBeenCalledTimes(2);
  });

  // AC 6: no slot (or NATS down) pauses live updates, as the table editor says it; live again, the listing is read again at once.
  it("says_paused_and_resumes_with_a_reload", () => {
    const rt = new FakeRealtime();
    const { refetch, result } = setup(rt);
    act(() => rt.storage.handlers.onJoined?.({ pk: ["id"], live: false, reason: "no_slot" as StatusReason }));
    expect(result.current).toEqual({ live: false, reason: "no_slot" });
    expect(refetch).not.toHaveBeenCalled();
    act(() => rt.storage.handlers.onStatus?.({ live: false, reason: "unavailable" }));
    expect(result.current).toEqual({ live: false, reason: "unavailable" });
    act(() => rt.storage.handlers.onStatus?.({ live: true }));
    expect(result.current).toEqual({ live: true });
    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it("a_lost_channel_pauses_and_a_transient_error_keeps_the_state", () => {
    const rt = new FakeRealtime();
    const { result } = setup(rt);
    act(() => rt.storage.handlers.onJoined?.({ pk: ["id"], live: true }));
    act(() => rt.storage.handlers.onError?.({ code: "limit", message: "x" }));
    expect(result.current).toEqual({ live: true });
    act(() => rt.storage.handlers.onError?.({ code: "forbidden", message: "access revoked" }));
    expect(result.current).toEqual({ live: false, reason: "forbidden" });
  });

  // R19: the files on screen, so a delete of one reaches this viewer as a key.
  it("sends_the_ids_on_screen_as_view_keys", () => {
    const rt = new FakeRealtime();
    const { rerender } = setup(rt, { target: TARGET, ids: [O1] });
    expect(rt.storage.views.at(-1)).toEqual([objectKey(O1)]);
    rerender({ target: TARGET, ids: [O1] });
    expect(rt.storage.views).toHaveLength(1);
    rerender({ target: TARGET, ids: [O1, O2] });
    expect(rt.storage.views.at(-1)).toEqual([objectKey(O1), objectKey(O2)]);
  });

  // S1: a folder row carries one object under it; its id is a view key too, so deleting the
  // folder's last object (that one, necessarily) reaches this viewer and the row goes.
  it("view_ids_are_the_files_and_each_folders_object", () => {
    const row = (name: string, folder: boolean, id: string | null) => ({ name, folder, size: null, etag: null, contentType: null, updatedAt: null, id });
    expect(viewIds([row("docs/", true, O2), row("a.png", false, O1), row("old/", true, null)])).toEqual([O2, O1]);
  });

  it("leaves_when_the_bucket_or_role_changes_and_on_unmount", () => {
    const rt = new FakeRealtime();
    const { rerender, unmount } = setup(rt);
    const first = rt.storage;
    rerender({ target: { ...TARGET, role: null }, ids: [] });
    expect(first.left).toBe(true);
    expect(rt.storage.params.role).toBeNull();
    const second = rt.storage;
    unmount();
    expect(second.left).toBe(true);
  });
});
