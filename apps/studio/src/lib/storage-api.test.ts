import { describe, expect, it } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import { ApiError } from "@chelabase/management-client";
import { bucketStatsKey, bucketsKey, invalidateStorage, objectCountText, parseBuckets, storageParams, storageProblem, summarizeObjects } from "./storage-api";

describe("storage query keys", () => {
  // #103 AC5: a key per branch and Act as role, so switching either reads the list again.
  it("bucket_list_keys_differ_by_branch_and_role", () => {
    const owner = bucketsKey("d1", "b1", null);
    const authenticated = bucketsKey("d1", "b1", "authenticated");
    const reporter = bucketsKey("d1", "b1", "reporter");
    const otherBranch = bucketsKey("d1", "b2", "authenticated");
    const keys = [owner, authenticated, reporter, otherBranch].map((k) => JSON.stringify(k));
    expect(new Set(keys).size).toBe(4);
    // The same place and role is the same key.
    expect(bucketsKey("d1", "b1", "reporter")).toEqual(reporter);
  });

  it("stats_keys_differ_by_bucket_and_role", () => {
    const keys = [
      bucketStatsKey("d1", "b1", null, "avatars"),
      bucketStatsKey("d1", "b1", "anon", "avatars"),
      bucketStatsKey("d1", "b1", null, "docs"),
      bucketStatsKey("d1", "b2", null, "avatars"),
    ].map((k) => JSON.stringify(k));
    expect(new Set(keys).size).toBe(4);
  });

  it("invalidating_a_branch_marks_every_role_and_bucket_stale_but_not_other_branches", async () => {
    const qc = new QueryClient();
    const mine = [bucketsKey("d1", "b1", null), bucketsKey("d1", "b1", "anon"), bucketStatsKey("d1", "b1", "anon", "avatars")];
    const other = bucketsKey("d1", "b2", null);
    for (const k of [...mine, other]) qc.setQueryData(k, []);
    await invalidateStorage(qc, "d1", "b1");
    for (const k of mine) expect(qc.getQueryState(k)?.isInvalidated).toBe(true);
    expect(qc.getQueryState(other)?.isInvalidated).toBe(false);
  });

  it("owner_mode_sends_no_role", () => {
    expect(storageParams(null)).toEqual({});
    expect(storageParams("authenticated")).toEqual({ role: "authenticated" });
  });
});

describe("parseBuckets", () => {
  it("keeps_well_formed_buckets_and_drops_the_rest", () => {
    const good = { id: "1", name: "avatars", public: true, file_size_limit: 1024, allowed_mime_types: ["image/*"], write_once: false, created_at: "2026-11-01T00:00:00Z" };
    expect(parseBuckets([good, { name: 3 }, null])).toEqual([good]);
    expect(parseBuckets({ not: "a list" })).toEqual([]);
  });
});

describe("summarizeObjects", () => {
  it("counts_files_and_sums_sizes", () => {
    const page = { items: [{ name: "a.png", folder: false, size: 100 }, { name: "b.png", folder: false, size: 50 }, { name: "x/", folder: true, size: null }], next_cursor: null };
    expect(summarizeObjects(page)).toEqual({ objects: 2, bytes: 150, more: false });
  });
  it("marks_a_truncated_listing", () => {
    expect(summarizeObjects({ items: [{ name: "a", folder: false, size: 1 }], next_cursor: "c" })).toEqual({ objects: 1, bytes: 1, more: true });
  });
  it("reads_a_malformed_body_as_empty", () => {
    expect(summarizeObjects(undefined)).toEqual({ objects: 0, bytes: 0, more: false });
  });
});

describe("objectCountText", () => {
  it("says_how_many_with_a_plus_when_truncated", () => {
    expect(objectCountText({ objects: 1, bytes: 0, more: false })).toBe("1 object");
    expect(objectCountText({ objects: 3, bytes: 0, more: false })).toBe("3 objects");
    expect(objectCountText({ objects: 1000, bytes: 0, more: true })).toBe("1,000+ objects");
  });
});

describe("storageProblem", () => {
  it("names_the_data_api_and_unavailable_cases", () => {
    expect(storageProblem(new ApiError(409, "data_api_disabled" as never, "off"))).toBe("data_api");
    expect(storageProblem(new ApiError(503, "storage_unavailable" as never, "no"))).toBe("unavailable");
    expect(storageProblem(new ApiError(403, "forbidden" as never, "no"))).toBeNull();
    expect(storageProblem(null)).toBeNull();
  });
});
