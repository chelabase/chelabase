/**
 * A data API failure: the server's `{ "error": { code, message, detail?, hint?, sqlstate? } }`
 * with the HTTP status. A non-2xx answer without that body (a proxy's error page, say)
 * gets a `code` from its status (401 `unauthorized`, 413 `too_large`, 429 `rate_limited`,
 * 502/503 `unavailable`, 504 `timeout`, else `internal`); a 2xx answer that isn't JSON
 * gets `internal`, as does a redirect (never followed). Network failures are not wrapped:
 * `fetch`'s own error is rethrown.
 */
export class DataApiError extends Error {
  override name = "DataApiError";
  status: number;
  code: string;
  detail?: string;
  hint?: string;
  sqlstate?: string;

  constructor(init: {
    status: number;
    code: string;
    message: string;
    detail?: string;
    hint?: string;
    sqlstate?: string;
  }) {
    super(init.message);
    this.status = init.status;
    this.code = init.code;
    if (init.detail !== undefined) this.detail = init.detail;
    if (init.hint !== undefined) this.hint = init.hint;
    if (init.sqlstate !== undefined) this.sqlstate = init.sqlstate;
  }
}

/**
 * An integer the client can't read as a `number` without rounding it: outside
 * ±(2^53 − 1). Thrown instead of returning a wrong value. `column` is the result key
 * (or the function's name for a scalar return); `source` is the exact digits.
 */
export class UnsafeIntegerError extends Error {
  override name = "UnsafeIntegerError";
  column: string;
  source: string;

  constructor(column: string, source: string) {
    super(
      `The integer ${source} in "${column}" is outside JavaScript's safe integer range ` +
        `(±(2^53 − 1)) and would lose precision as a number. Create the client with ` +
        "`bigint: true` (and the generated `types`) to read int8 values exactly as bigint.",
    );
    this.column = column;
    this.source = source;
  }
}

/**
 * How every request the client sends treats a redirect: it isn't followed (each carries a
 * data key, and maybe an app user's token or a refresh token), and [`toError`] makes the
 * answer a `DataApiError`. In Node the 3xx itself comes back; in a browser, an
 * `opaqueredirect` with status 0.
 */
export const NO_REDIRECTS: RequestRedirect = "manual";

/** The `DataApiError` for a non-2xx answer (internal: not exported from the package). */
export function toError(res: Response, text: string): DataApiError {
  if (res.type === "opaqueredirect" || (res.status >= 300 && res.status < 400)) {
    return new DataApiError({
      status: res.status,
      code: "internal",
      message: `the server answered with a redirect${res.status ? ` (HTTP ${res.status})` : ""}; the client never follows one, so the request was not sent again`,
    });
  }
  try {
    const body = JSON.parse(text) as { error?: Record<string, unknown> };
    const e = body.error;
    if (e && typeof e["code"] === "string" && typeof e["message"] === "string") {
      const opt = (k: string) => (typeof e[k] === "string" ? (e[k] as string) : undefined);
      return new DataApiError({
        status: res.status,
        code: e["code"],
        message: e["message"],
        detail: opt("detail"),
        hint: opt("hint"),
        sqlstate: opt("sqlstate"),
      });
    }
  } catch {
    // not JSON: fall through
  }
  return plainError(res, text, STATUS_CODES[res.status] ?? "internal");
}

/** The `code` for a non-2xx answer without the data API's error body (a proxy's page). */
const STATUS_CODES: Record<number, string> = {
  401: "unauthorized",
  413: "too_large",
  429: "rate_limited",
  502: "unavailable",
  503: "unavailable",
  504: "timeout",
};

export function plainError(res: Response, text: string, code: string): DataApiError {
  const snippet = text.length > 200 ? `${text.slice(0, 200)}…` : text;
  return new DataApiError({
    status: res.status,
    code,
    message: `HTTP ${res.status}${res.statusText ? ` ${res.statusText}` : ""}${snippet ? `: ${snippet}` : ""}`,
  });
}
