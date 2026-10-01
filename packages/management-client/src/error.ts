import type { ErrorCode } from "./generated/model/errorCode";

/** Error thrown by every generated hook and function when a request does not succeed. */
export class ApiError extends Error {
  /** HTTP status; 0 for network failures. */
  readonly status: number;
  /** The server's error code, or the special value "network". */
  readonly code: ErrorCode | "network";
  /** Every key of the error body object other than code and message. */
  readonly extra: Record<string, unknown>;
  /** True when a 401 made the client restart sign-in. */
  readonly signInRestarted: boolean;

  constructor(status: number, code: ErrorCode | "network", message: string, extra: Record<string, unknown> = {}, signInRestarted = false) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.extra = extra;
    this.signInRestarted = signInRestarted;
  }
}
