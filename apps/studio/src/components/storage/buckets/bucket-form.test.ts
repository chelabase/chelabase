import { describe, expect, it } from "vitest";
import { bucketFormFrom, parseBucketForm, type BucketFormInput } from "./bucket-form";

const base: BucketFormInput = { name: "avatars", public: false, sizeLimit: "", sizeUnit: "MB", allowedTypes: "" };
const create = (over: Partial<BucketFormInput> = {}) => parseBucketForm({ ...base, ...over }, null);

describe("parseBucketForm (create)", () => {
  it("sends_the_name_and_public_with_no_limits_when_blank", () => {
    expect(create()).toEqual({ ok: true, body: { name: "avatars", public: false } });
    expect(create({ public: true })).toEqual({ ok: true, body: { name: "avatars", public: true } });
  });

  it("checks_the_name_as_storage_does", () => {
    for (const ok of ["a", "my-bucket", "v1.2_x", "0", "a".repeat(63)]) expect(create({ name: ok })).toMatchObject({ ok: true });
    for (const bad of ["", "Avatars", "-x", ".x", "a b", "a/b", "a".repeat(64), "é"]) {
      expect(create({ name: bad })).toMatchObject({ ok: false, name: expect.any(String) });
    }
  });

  it("refuses_the_names_the_storage_routes_reserve", () => {
    for (const r of ["list", "copy", "move", "sign", "public", "upload", "resolve", "session"]) {
      expect(create({ name: r })).toMatchObject({ ok: false, name: expect.stringMatching(/reserved/) });
    }
  });

  it("converts_the_size_limit_to_bytes", () => {
    expect(create({ sizeLimit: "5", sizeUnit: "MB" })).toMatchObject({ ok: true, body: { file_size_limit: 5 * 1024 * 1024 } });
    expect(create({ sizeLimit: "2", sizeUnit: "GB" })).toMatchObject({ ok: true, body: { file_size_limit: 2 * 1024 ** 3 } });
    expect(create({ sizeLimit: "100", sizeUnit: "KB" })).toMatchObject({ ok: true, body: { file_size_limit: 102400 } });
    for (const bad of ["0", "-1", "1.5", "abc"]) expect(create({ sizeLimit: bad })).toMatchObject({ ok: false, sizeLimit: expect.any(String) });
  });

  it("splits_allowed_types_on_commas_and_lines_and_checks_each", () => {
    expect(create({ allowedTypes: "image/*, application/pdf\ntext/csv" })).toMatchObject({
      ok: true,
      body: { allowed_mime_types: ["image/*", "application/pdf", "text/csv"] },
    });
    for (const bad of ["image", "image/", "/png", "a/b/c", "image/png;x"]) {
      expect(create({ allowedTypes: bad })).toMatchObject({ ok: false, allowedTypes: expect.stringContaining(bad) });
    }
  });
});

describe("parseBucketForm (edit)", () => {
  const bucket = { id: "1", name: "avatars", public: false, file_size_limit: 1024 * 1024, allowed_mime_types: ["image/*"], write_once: false };

  it("starts_from_the_bucket", () => {
    expect(bucketFormFrom(bucket)).toEqual({ name: "avatars", public: false, sizeLimit: "1", sizeUnit: "MB", allowedTypes: "image/*" });
    expect(bucketFormFrom({ ...bucket, file_size_limit: 1536, allowed_mime_types: null })).toMatchObject({ sizeLimit: "1536", sizeUnit: "B" });
  });

  it("sends_only_what_changed_and_never_the_name", () => {
    const unchanged = bucketFormFrom(bucket);
    expect(parseBucketForm(unchanged, bucket)).toEqual({ ok: true, body: {} });
    expect(parseBucketForm({ ...unchanged, public: true, sizeLimit: "2" }, bucket)).toEqual({ ok: true, body: { public: true, file_size_limit: 2 * 1024 * 1024 } });
  });

  it("clearing_allowed_types_allows_any_type", () => {
    expect(parseBucketForm({ ...bucketFormFrom(bucket), allowedTypes: "" }, bucket)).toEqual({ ok: true, body: { allowed_mime_types: ["*/*"] } });
  });

  it("a_size_limit_once_set_cant_be_removed", () => {
    expect(parseBucketForm({ ...bucketFormFrom(bucket), sizeLimit: "" }, bucket)).toMatchObject({ ok: false, sizeLimit: expect.stringMatching(/can't be removed/) });
    // A bucket with no limit may stay without one.
    const open = { ...bucket, file_size_limit: null };
    expect(parseBucketForm(bucketFormFrom(open), open)).toEqual({ ok: true, body: {} });
  });
});
