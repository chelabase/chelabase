import type { IncomingMessage } from "node:http";
import { APIError } from "better-auth/api";
import { PoolNotFound } from "./registry.ts";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export class HttpError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

const MAX_BODY_BYTES = 64 * 1024;

/** The JSON object body, `{}` when there is none. */
export async function readBody(req: IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of req as AsyncIterable<Buffer>) {
    size += chunk.length;
    if (size > MAX_BODY_BYTES) throw new HttpError(413, "body_too_large", "request body is too large");
    chunks.push(chunk);
  }
  if (size === 0) return {};
  let parsed: unknown;
  try {
    parsed = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw new HttpError(400, "invalid_request", "body is not valid JSON");
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new HttpError(400, "invalid_request", "body must be a JSON object");
  }
  return parsed as Record<string, unknown>;
}

/** Better Auth's error codes mapped to ours; anything unlisted is a plain 400 or a 500. */
const BETTER_AUTH_CODES: Record<string, [number, string]> = {
  INVALID_EMAIL_OR_PASSWORD: [401, "bad_credentials"],
  BANNED_USER: [403, "banned"],
  USER_ALREADY_EXISTS: [409, "conflict"],
  USER_ALREADY_EXISTS_USE_ANOTHER_EMAIL: [409, "conflict"],
  INVALID_EMAIL: [400, "invalid_email"],
  PASSWORD_TOO_SHORT: [400, "weak_password"],
  PASSWORD_TOO_LONG: [400, "weak_password"],
  USER_NOT_FOUND: [404, "user_not_found"],
};

function pgCode(err: unknown): unknown {
  for (let e: unknown = err, depth = 0; e && typeof e === "object" && depth < 3; e = (e as { cause?: unknown }).cause, depth++) {
    if ("code" in e && typeof e.code === "string" && /^[0-9A-Z]{5}$/.test(e.code)) return e.code;
  }
  return undefined;
}

export function toHttpError(err: unknown): HttpError {
  if (err instanceof HttpError) return err;
  if (err instanceof PoolNotFound) return new HttpError(404, "pool_not_found", err.message);
  // undefined_table: the pool's schema was dropped while this request was using it.
  if (pgCode(err) === "42P01") return new HttpError(404, "pool_not_found", "pool does not exist");
  if (err instanceof APIError) {
    const code = typeof err.body?.code === "string" ? err.body.code : "";
    const mapped = BETTER_AUTH_CODES[code];
    if (mapped) return new HttpError(mapped[0], mapped[1], err.body?.message ?? code);
    if (err.statusCode >= 400 && err.statusCode < 500) {
      return new HttpError(400, "bad_request", err.body?.message ?? "request refused");
    }
  }
  return new HttpError(500, "internal", "internal error");
}
