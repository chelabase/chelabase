/**
 * A data API failure: the server's `{ "error": { code, message, detail?, hint?, sqlstate? } }`
 * with the HTTP status. A non-2xx answer without that body (a proxy's error page, say)
 * gets a `code` from its status (401 `unauthorized`, 413 `too_large`, 429 `rate_limited`,
 * 502/503 `unavailable`, 504 `timeout`, else `internal`); a 2xx answer that isn't JSON
 * gets `internal`. Network failures are not wrapped: `fetch`'s own error is rethrown.
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
