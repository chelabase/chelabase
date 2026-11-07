import { ApiError } from "@chelabase/management-client";
import { errorText } from "@/lib/errors";
import type { UploadFailure } from "./queue-reducer";

/** Statuses worth trying again: stalled body, rate limit, busy or unreachable storage. */
export const RETRY_STATUSES = new Set([408, 429, 502, 503, 504]);

/** A failure a retry may get past (the runner retries these itself first). */
export function isRetryable(err: unknown): boolean {
  return err instanceof ApiError && (err.status === 0 || RETRY_STATUSES.has(err.status));
}

const sentence = (s: string) => (/[.!?]$/.test(s) ? s : `${s}.`);

/** Why one file's upload failed, for its row in the tray. `contentType` is the file's own. */
export function describeUploadError(err: unknown, contentType: string): UploadFailure {
  const fail = (message: string, retryable = false, conflict = false): UploadFailure => ({ message, retryable, conflict });
  if (!(err instanceof ApiError)) return fail(errorText(err));
  const code = err.code as string;
  if (code === "conflict") return fail("A file with this name already exists.", false, true);
  if (code === "write_once") return fail("The bucket is write-once: a file already stored under this name can't be replaced.");
  if (err.status === 413 || code === "too_large") return fail(`Too large for this bucket: ${sentence(err.message)}`);
  if (err.status === 415 || code === "mime_not_allowed") {
    return fail(contentType === "" ? "This bucket doesn't accept this file's type." : `This bucket doesn't accept ${contentType} files.`);
  }
  if (code === "busy") return fail("Storage is busy with other uploads; retry in a moment.", true);
  if (err.status === 408) return fail("The upload stalled and storage stopped waiting; retry.", true);
  if (err.status === 0) return fail("Can't reach Chelabase; check your connection and retry.", true);
  if (code === "upload_expired") return fail("The resumable upload expired; retry to start again.", true);
  if (err.status === 400) return fail(`Storage refused the upload: ${sentence(err.message)}`);
  return fail(errorText(err), RETRY_STATUSES.has(err.status));
}
