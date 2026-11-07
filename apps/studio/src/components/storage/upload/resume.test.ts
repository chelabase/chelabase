import { ApiError } from "@chelabase/management-client";
import { describe, expect, it } from "vitest";
import { MiB, planParts, runUpload } from "./runner";
import { fakeApi } from "./fake-api";

const network = () => new ApiError(0, "network", "Can't reach Chelabase");
const big = (mib: number) => new File([new Uint8Array(mib * MiB)], "big.bin", { type: "application/x-big" });

describe("planParts", () => {
  it("splits_a_file_into_parts_with_a_smaller_last_one", () => {
    expect(planParts(20, 8)).toEqual([
      { n: 1, start: 0, end: 8 },
      { n: 2, start: 8, end: 16 },
      { n: 3, start: 16, end: 20 },
    ]);
    expect(planParts(0, 8)).toEqual([{ n: 1, start: 0, end: 0 }]);
  });
});

describe("runUpload resume", () => {
  // #105 AC4
  it("a_40_mib_file_whose_network_drops_sends_only_the_missing_parts_when_resumed", async () => {
    // The network drops at part 3 and stays down until the test brings it back.
    let dropping = true;
    let down = false;
    const api = fakeApi({
      failPart: (n) => {
        if (dropping && n === 3) down = true;
        return down ? network() : null;
      },
    });
    const file = big(40);
    const job = { bucket: "b", key: "big.bin", name: "big.bin", file, choice: null, session: null as string | null };
    const hooks = api.hooks();
    const err = await runUpload(api, job, hooks, { retries: 2 }).catch((e: unknown) => e);
    expect((err as ApiError).code).toBe("network");
    const session = hooks.sessions.at(-1) ?? null;
    expect(session).not.toBeNull();
    expect(api.partsSent()).toEqual([1, 2, 3, 3, 3]);

    dropping = false;
    down = false;
    api.calls.length = 0;
    const again = api.hooks();
    const out = await runUpload(api, { ...job, session }, again);
    expect(out.name).toBe("big.bin");
    expect(api.partsSent()).toEqual([3, 4, 5]);
    expect(api.calls.map((c) => c.op)).not.toContain("create");
    expect((await api.blob("big.bin"))?.size).toBe(40 * MiB);
    expect((await api.blob("big.bin"))?.type).toBe("application/x-big");
    // Progress starts from what the server already had and ends at the whole size.
    expect(again.progress[0]).toBeGreaterThanOrEqual(16 * MiB);
    expect(again.progress.at(-1)).toBe(40 * MiB);
    expect(again.sessions.at(-1)).toBeNull();
  });

  it("a_session_the_server_no_longer_has_starts_again", async () => {
    const api = fakeApi();
    const hooks = api.hooks();
    const out = await runUpload(api, { bucket: "b", key: "big.bin", name: "big.bin", file: big(12), choice: null, session: "gone" }, hooks);
    expect(out.name).toBe("big.bin");
    expect(api.calls.map((c) => c.op)).toContain("create");
    expect(api.partsSent()).toEqual([1, 2]);
  });

  it("a_busy_or_stalled_answer_is_retried_with_backoff", async () => {
    let failures = [new ApiError(503, "busy" as never, "busy"), new ApiError(408, "request_timeout" as never, "stalled")];
    const api = fakeApi({ failPart: () => failures.shift() ?? null, failPut: () => failures.shift() ?? null });
    const hooks = api.hooks();
    await runUpload(api, { bucket: "b", key: "big.bin", name: "big.bin", file: big(12), choice: null, session: null }, hooks);
    expect(api.partsSent()).toEqual([1, 1, 1, 2]);
    expect(hooks.waits).toEqual([500, 1000]);
    failures = [new ApiError(503, "busy" as never, "busy")];
    await runUpload(api, { bucket: "b", key: "s.txt", name: "s.txt", file: new File(["x"], "s.txt"), choice: null, session: null }, api.hooks());
    expect(await api.text("s.txt")).toBe("x");
  });

  // #105 AC5: refusals aren't retried.
  it("a_413_or_415_fails_at_once", async () => {
    for (const e of [new ApiError(413, "too_large" as never, "over"), new ApiError(415, "mime_not_allowed" as never, "type")]) {
      const api = fakeApi({ failPut: () => e });
      const err = await runUpload(api, { bucket: "b", key: "s.txt", name: "s.txt", file: new File(["x"], "s.txt"), choice: null, session: null }, api.hooks()).catch((x: unknown) => x);
      expect(err).toBe(e);
      expect(api.calls.filter((c) => c.op === "put")).toHaveLength(1);
    }
  });

  it("a_refused_completion_forgets_the_session", async () => {
    const api = fakeApi({ failComplete: () => new ApiError(413, "too_large" as never, "over") });
    const hooks = api.hooks();
    await expect(runUpload(api, { bucket: "b", key: "big.bin", name: "big.bin", file: big(12), choice: null, session: null }, hooks)).rejects.toBeInstanceOf(ApiError);
    expect(hooks.sessions.at(-1)).toBeNull();
  });

  it("an_aborted_signal_stops_before_the_next_part", async () => {
    const controller = new AbortController();
    const api = fakeApi({ failPart: (n) => (n === 1 ? (controller.abort(), null) : null) });
    const hooks = { ...api.hooks(), signal: controller.signal };
    await expect(runUpload(api, { bucket: "b", key: "big.bin", name: "big.bin", file: big(20), choice: null, session: null }, hooks)).rejects.toMatchObject({ name: "AbortError" });
    expect(api.partsSent()).toEqual([1]);
  });
});
