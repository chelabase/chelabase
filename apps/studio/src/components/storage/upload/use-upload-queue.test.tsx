import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { UploadApi } from "./runner";
import { useUploadQueue } from "./use-upload-queue";

/** An API whose single uploads finish only when the test says so. */
function gatedApi() {
  const pending = new Map<string, { resolve: () => void; reject: (e: unknown) => void }>();
  let running = 0;
  let peak = 0;
  const aborted: string[] = [];
  const api: UploadApi = {
    put: (_bucket, name, _body, o) =>
      new Promise<void>((resolve, reject) => {
        running++;
        peak = Math.max(peak, running);
        const end = () => {
          running--;
          pending.delete(name);
        };
        pending.set(name, { resolve: () => (end(), resolve()), reject: (e) => (end(), reject(e)) });
        o.signal.addEventListener("abort", () => {
          end();
          reject(new DOMException("aborted", "AbortError"));
        });
      }),
    create: async () => "s1",
    listParts: async () => new Map(),
    putPart: () => new Promise<void>(() => {}),
    complete: async () => {},
    abort: async (session) => {
      aborted.push(session);
    },
  };
  return { api, pending, aborted, peak: () => peak };
}

const files = (n: number) => Array.from({ length: n }, (_, i) => ({ path: `f${i}.txt`, file: new File(["x"], `f${i}.txt`) }));

afterEach(() => vi.restoreAllMocks());

describe("useUploadQueue", () => {
  // #105 AC2
  it("ten_files_run_at_most_four_at_once_and_all_finish", async () => {
    const g = gatedApi();
    const onUploaded = vi.fn();
    const { result } = renderHook(() => useUploadQueue({ api: g.api, onUploaded }));
    act(() => result.current.add("b", "docs/", files(10)));
    await waitFor(() => expect(g.pending.size).toBe(4));
    while (result.current.state.items.some((i) => i.status !== "done")) {
      await waitFor(() => expect(g.pending.size).toBeGreaterThan(0));
      const [name] = [...g.pending.keys()];
      act(() => g.pending.get(name!)!.resolve());
      await waitFor(() => expect(result.current.state.items.find((i) => i.name === name)?.status).toBe("done"));
    }
    expect(g.peak()).toBe(4);
    expect(result.current.state.items.map((i) => i.name)).toEqual(Array.from({ length: 10 }, (_, i) => `docs/f${i}.txt`));
    expect(onUploaded).toHaveBeenCalled();
  });

  // #105 AC6
  it("cancel_aborts_the_multipart_session", async () => {
    const g = gatedApi();
    const { result } = renderHook(() => useUploadQueue({ api: g.api, onUploaded: () => {} }));
    const big = new File([new Uint8Array(9 * 1024 * 1024)], "big.bin");
    act(() => result.current.add("b", "", [{ path: "big.bin", file: big }]));
    await waitFor(() => expect(result.current.state.items[0]?.session).toBe("s1"));
    const id = result.current.state.items[0]!.id;
    act(() => result.current.cancel(id));
    await waitFor(() => expect(g.aborted).toEqual(["s1"]));
    expect(result.current.state.items[0]).toMatchObject({ status: "skipped", cancelled: true });
  });

  it("a_failure_is_described_and_the_others_continue", async () => {
    const g = gatedApi();
    const { result } = renderHook(() => useUploadQueue({ api: g.api, onUploaded: () => {} }));
    act(() => result.current.add("b", "", files(2)));
    await waitFor(() => expect(g.pending.size).toBe(2));
    const { ApiError } = await import("@chelabase/management-client");
    act(() => g.pending.get("f0.txt")!.reject(new ApiError(413, "too_large" as never, "over the limit")));
    await waitFor(() => expect(result.current.state.items[0]?.status).toBe("failed"));
    expect(result.current.state.items[0]?.error?.message).toMatch(/^Too large/);
    act(() => g.pending.get("f1.txt")!.resolve());
    await waitFor(() => expect(result.current.state.items[1]?.status).toBe("done"));
  });

  // #105 AC8
  it("leaving_the_page_with_uploads_running_asks_first", async () => {
    const g = gatedApi();
    const { result, unmount } = renderHook(() => useUploadQueue({ api: g.api, onUploaded: () => {} }));
    const quiet = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(quiet);
    expect(quiet.defaultPrevented).toBe(false);
    act(() => result.current.add("b", "", files(1)));
    await waitFor(() => expect(g.pending.size).toBe(1));
    const busy = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(busy);
    expect(busy.defaultPrevented).toBe(true);
    act(() => g.pending.get("f0.txt")!.resolve());
    await waitFor(() => expect(result.current.state.items[0]?.status).toBe("done"));
    const after = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(after);
    expect(after.defaultPrevented).toBe(false);
    unmount();
  });
});
