/**
 * The stable codes a `StorageError` carries, one per kind of failure (the server's own, finer
 * code is `serverCode`):
 *
 * - `invalid_request` (400): a bad name, option or `Content-Type` (one `type/subtype` with
 *   unquoted parameters; a signed upload URL refuses it before it is spent);
 * - `unauthorized` (401): a bad or expired key, token or signed URL;
 * - `forbidden` (403): no policy allows it, or a signed URL's key was revoked;
 * - `not_found` (404): missing, or hidden from the caller;
 * - `timeout` (408, 504): the body sent nothing for `CHELABASE_STORAGE_BODY_IDLE_SECS`
 *   (60 s by default), or the gateway timed out;
 * - `conflict` (409): the name is taken without `upsert`, another write won, or a bucket isn't empty;
 * - `write_once` (409): the bucket is write-once, so the object is never overwritten, moved or deleted;
 * - `expired` (410): a resumable upload's session ended;
 * - `too_large` (413): over the bucket's file size limit;
 * - `unsupported_type` (415): a type the bucket's `allowed_mime_types` doesn't list;
 * - `range_not_satisfiable` (416);
 * - `rate_limited` (429);
 * - `unavailable` (502, 503): storage is down or busy;
 * - `internal`: anything else, including a redirect (never followed).
 */
export type StorageErrorCode =
  | "invalid_request"
  | "unauthorized"
  | "forbidden"
  | "not_found"
  | "timeout"
  | "conflict"
  | "write_once"
  | "expired"
  | "too_large"
  | "unsupported_type"
  | "range_not_satisfiable"
  | "rate_limited"
  | "unavailable"
  | "internal";

/**
 * A storage failure: the HTTP status, a stable `code`, and the server's own `code` as
 * `serverCode` when it sent one. Network failures and aborts are not wrapped: `fetch`'s own
 * error (or the signal's reason) is rethrown.
 */
export class StorageError extends Error {
  override name = "StorageError";
  status: number;
  code: StorageErrorCode;
  serverCode?: string;
  sqlstate?: string;

  constructor(init: { status: number; code: StorageErrorCode; message: string; serverCode?: string; sqlstate?: string }) {
    super(init.message);
    this.status = init.status;
    this.code = init.code;
    if (init.serverCode !== undefined) this.serverCode = init.serverCode;
    if (init.sqlstate !== undefined) this.sqlstate = init.sqlstate;
  }
}

const BY_STATUS: Record<number, StorageErrorCode> = {
  400: "invalid_request",
  401: "unauthorized",
  403: "forbidden",
  404: "not_found",
  408: "timeout",
  409: "conflict",
  410: "expired",
  413: "too_large",
  415: "unsupported_type",
  416: "range_not_satisfiable",
  429: "rate_limited",
  502: "unavailable",
  503: "unavailable",
  504: "timeout",
};

/** The `StorageError` for a non-2xx answer (internal). */
export function toStorageError(res: Response, text: string): StorageError {
  if (res.type === "opaqueredirect" || (res.status >= 300 && res.status < 400)) {
    return new StorageError({
      status: res.status,
      code: "internal",
      message: `the server answered with a redirect${res.status ? ` (HTTP ${res.status})` : ""}; the client never follows one`,
    });
  }
  let serverCode: string | undefined;
  let message: string | undefined;
  let sqlstate: string | undefined;
  try {
    const e = (JSON.parse(text) as { error?: Record<string, unknown> }).error;
    if (e && typeof e["code"] === "string" && typeof e["message"] === "string") {
      serverCode = e["code"];
      message = e["message"];
      if (typeof e["sqlstate"] === "string") sqlstate = e["sqlstate"];
    }
  } catch {
    // not JSON: the status decides
  }
  if (message === undefined) {
    const snippet = text.length > 200 ? `${text.slice(0, 200)}…` : text;
    message = `HTTP ${res.status}${res.statusText ? ` ${res.statusText}` : ""}${snippet ? `: ${snippet}` : ""}`;
  }
  const code = serverCode === "write_once" ? "write_once" : (BY_STATUS[res.status] ?? "internal");
  return new StorageError({ status: res.status, code, message, serverCode, sqlstate });
}
