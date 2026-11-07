import { describe, expect, it } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import { ApiError } from "@chelabase/management-client";
import { apiHref, bucketStatsKey, bucketsKey, invalidateStorage, objectActionError, objectsKey, objectCountText, parseBuckets, parseObjectPage, parseSigned, publicObjectUrl, storageParams, storageProblem, summarizeObjects } from "./storage-api";

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

describe("parseObjectPage", () => {
  it("reads_items_and_the_cursor_dropping_malformed_ones", () => {
    const page = parseObjectPage({
      items: [
        { name: "photos/", folder: true, size: null, etag: null, content_type: null, updated_at: null, id: "not-a-file" },
        { name: "photos/a.png", folder: false, size: 10, etag: "e1", content_type: "image/png", updated_at: 1762000000, id: "o-1" },
        { name: "photos/b.png", folder: false, size: 1, etag: "e2", content_type: "image/png", updated_at: 1 },
        { name: 4 },
      ],
      next_cursor: "c",
    });
    expect(page.items).toEqual([
      // m17: a file's object id (live view keys); a folder never has one.
      { name: "photos/", folder: true, size: null, etag: null, contentType: null, updatedAt: null, id: null },
      { name: "photos/a.png", folder: false, size: 10, etag: "e1", contentType: "image/png", updatedAt: 1762000000, id: "o-1" },
      { name: "photos/b.png", folder: false, size: 1, etag: "e2", contentType: "image/png", updatedAt: 1, id: null },
    ]);
    expect(page.nextCursor).toBe("c");
    expect(parseObjectPage({ items: [], next_cursor: null }).nextCursor).toBeNull();
    expect(parseObjectPage("nope")).toEqual({ items: [], nextCursor: null });
  });
});

describe("objectsKey", () => {
  it("is_under_the_branch_and_differs_by_role_folder_and_query", () => {
    const q = { prefix: "", sort: "name", order: "asc", search: "" };
    const keys = [
      objectsKey("d1", "b1", null, "avatars", q),
      objectsKey("d1", "b1", "anon", "avatars", q),
      objectsKey("d1", "b1", null, "avatars", { ...q, prefix: "x/" }),
      objectsKey("d1", "b1", null, "avatars", { ...q, search: "a" }),
    ].map((k) => JSON.stringify(k));
    expect(new Set(keys).size).toBe(4);
    expect(objectsKey("d1", "b1", null, "avatars", q)[0]).toEqual(bucketsKey("d1", "b1", null)[0]);
  });
});

describe("objectActionError", () => {
  it("explains_write_once_and_references_and_keeps_the_servers_conflict_message", () => {
    expect(objectActionError(new ApiError(409, "write_once" as never, "write-once"))).toMatch(/write-once/);
    expect(objectActionError(new ApiError(409, "referenced" as never, "x"))).toMatch(/references/);
    expect(objectActionError(new ApiError(409, "conflict" as never, "the object `a.png` already exists"))).toBe("the object `a.png` already exists");
    expect(objectActionError(new ApiError(400, "invalid_request" as never, "bad content type"))).toBe("bad content type");
  });
});

describe("URLs on the API host", () => {
  it("a_signed_path_joins_the_api_url", () => {
    expect(apiHref("https://api.test/", "/storage/v1/object/sign/a/b.png?token=t")).toBe("https://api.test/storage/v1/object/sign/a/b.png?token=t");
    expect(parseSigned({ signed_url: "/storage/v1/x?token=t", expires_at: 5 })).toEqual({ path: "/storage/v1/x?token=t", expiresAt: 5 });
    expect(parseSigned({})).toBeNull();
  });
  it("a_public_url_names_the_branch_bucket_object_and_database", () => {
    expect(publicObjectUrl("https://api.test", "b1", "app db", "avatars", "my photos/a b.png")).toBe(
      "https://api.test/storage/v1/object/public/b1/avatars/my%20photos/a%20b.png?database=app%20db",
    );
  });
});
