import { ApiError } from "@chelabase/management-client";
import { describe, expect, it } from "vitest";
import { describeUploadError } from "./errors";

const api = (status: number, code: string, message = "storage says") => new ApiError(status, code as never, message);

describe("describeUploadError", () => {
  // #105 AC5
  it("each_refusal_gets_its_own_message", () => {
    const too = describeUploadError(api(413, "too_large", "the object is over the bucket's limit of 1048576 bytes"), "image/png");
    expect(too).toEqual({ message: "Too large for this bucket: the object is over the bucket's limit of 1048576 bytes.", retryable: false, conflict: false });
    expect(describeUploadError(api(415, "mime_not_allowed"), "image/png").message).toBe("This bucket doesn't accept image/png files.");
    expect(describeUploadError(api(415, "mime_not_allowed"), "").message).toBe("This bucket doesn't accept this file's type.");
    expect(describeUploadError(api(409, "write_once"), "")).toMatchObject({ retryable: false, conflict: false });
    expect(describeUploadError(api(409, "write_once"), "").message).toMatch(/write-once/);
    expect(describeUploadError(api(400, "invalid_request", "content type is malformed"), "x").message).toBe("Storage refused the upload: content type is malformed.");
  });

  it("a_taken_name_is_a_conflict", () => {
    expect(describeUploadError(api(409, "conflict"), "")).toEqual({ message: "A file with this name already exists.", retryable: false, conflict: true });
  });

  it("busy_stalled_network_and_expired_are_retryable", () => {
    for (const e of [api(503, "busy"), api(408, "request_timeout"), api(0, "network"), api(410, "upload_expired"), api(502, "unavailable")]) {
      expect(describeUploadError(e, "")).toMatchObject({ retryable: true, conflict: false });
    }
    expect(describeUploadError(api(503, "busy"), "").message).toMatch(/busy/);
    expect(describeUploadError(api(408, "request_timeout"), "").message).toMatch(/stalled/);
  });

  it("anything_else_is_its_message", () => {
    expect(describeUploadError(api(403, "not_allowed", "policy says no"), "")).toEqual({ message: "policy says no", retryable: false, conflict: false });
    expect(describeUploadError(new Error("boom"), "").message).toBe("boom");
  });
});
