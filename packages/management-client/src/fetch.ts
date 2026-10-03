import { ApiError } from "./error";
import { getConsoleQueryUrl } from "./generated/endpoints";
import type { ConsoleRequest } from "./generated/model";
import type { ErrorCode } from "./generated/model/errorCode";

export interface ManagementClientConfig {
  baseUrl: string;
  getToken: () => string | undefined;
  /** Called on a 401; return true when sign-in is restarting. */
  onUnauthorized: () => boolean | void;
  fetchImpl?: typeof fetch;
}

/** Error type orval puts on the generated hooks. */
export type ErrorType<_E> = ApiError;

export const NETWORK_MESSAGE = "Can't reach Cheladb; check your connection";

let config: ManagementClientConfig | undefined;

export function configureManagementClient(next: ManagementClientConfig): void {
  config = next;
}

/** Reads a non-2xx response into an ApiError (our `{"error":{...}}` body, else a generic one). */
export async function readApiError(response: Response, signInRestarted = false): Promise<ApiError> {
  const status = response.status;
  try {
    const parsed: unknown = JSON.parse(await response.text());
    const err = (parsed as { error?: unknown } | null)?.error;
    if (err && typeof err === "object" && typeof (err as { code?: unknown }).code === "string" && typeof (err as { message?: unknown }).message === "string") {
      const { code, message, ...extra } = err as { code: ErrorCode; message: string } & Record<string, unknown>;
      return new ApiError(status, code, message, extra, signInRestarted);
    }
  } catch {
    // not JSON; fall through
  }
  const code = status === 502 || status === 503 || status === 504 ? "unavailable" : "internal";
  return new ApiError(status, code, `Request failed with status ${status}`, {}, signInRestarted);
}

/** Runs a request and returns the parsed success body (undefined when empty); throws ApiError otherwise. */
export async function requestJson<T>(
  fetchImpl: typeof fetch,
  url: string,
  init: RequestInit,
  onUnauthorized?: () => boolean | void,
  parse: (text: string) => unknown = JSON.parse,
): Promise<T> {
  let response: Response;
  try {
    response = await fetchImpl(url, init);
  } catch (e) {
    // TanStack Query cancels on unmount and must see its own abort.
    if (init.signal?.aborted || (e instanceof Error && e.name === "AbortError")) throw e;
    throw new ApiError(0, "network", NETWORK_MESSAGE);
  }
  if (response.ok) {
    try {
      const text = await response.text();
      return (text === "" ? undefined : parse(text)) as T;
    } catch {
      throw new ApiError(response.status, "internal", "Unexpected response from Cheladb");
    }
  }
  const restarted = response.status === 401 && onUnauthorized ? onUnauthorized() === true : false;
  throw await readApiError(response, restarted);
}

export function buildHeaders(init: RequestInit, extra: Record<string, string> = {}): Headers {
  const headers = new Headers(init.headers);
  headers.set("Accept", "application/json");
  if (init.body != null) headers.set("Content-Type", "application/json");
  for (const [k, v] of Object.entries(extra)) headers.set(k, v);
  return headers;
}

/** Sends one authenticated request to the configured gateway; `parse` reads a success body. */
function send<T>(path: string, options: RequestInit, parse?: (text: string) => unknown): Promise<T> {
  if (!config) throw new Error("Management client is not configured; call configureManagementClient first");
  const token = config.getToken();
  const headers = buildHeaders(options, token ? { Authorization: `Bearer ${token}` } : {});
  const fetchImpl = config.fetchImpl ?? globalThis.fetch.bind(globalThis);
  return requestJson<T>(fetchImpl, `${config.baseUrl}${path}`, { ...options, headers }, config.onUnauthorized, parse);
}

/** Orval mutator for the Management API. */
export const customFetch = async <T>(url: string, options: RequestInit): Promise<T> => send<T>(url, options);

type Reviver = (this: unknown, key: string, value: unknown, context?: { source?: string }) => unknown;

/**
 * Parses JSON without losing digits: a number that doesn't read back identically from a double
 * (an int8 or numeric the server wrote in full) comes back as its source text. Runtimes without
 * source-text access fall back to plain `JSON.parse`.
 */
export function parseExact(text: string): unknown {
  const reviver: Reviver = (_key, value, context) => {
    const source = context?.source;
    if (typeof value !== "number" || typeof source !== "string") return value;
    // At most 15 significant digits in plain decimal form always survive a double.
    if (/^-?\d+(\.\d+)?$/.test(source) && source.replace(/^-/, "").replace(".", "").replace(/^0+/, "").length <= 15) return value;
    return String(value) === source ? value : source;
  };
  return (JSON.parse as (t: string, r: Reviver) => unknown)(text, reviver);
}

/**
 * The console route with an exact read: numbers outside what a double holds arrive as strings.
 * Studio uses it so a primary key read back is the key that gets written.
 */
export const consoleQueryExact = async (
  id: string,
  branchId: string,
  request: ConsoleRequest,
): Promise<unknown> => send<unknown>(getConsoleQueryUrl(id, branchId), { method: "POST", body: JSON.stringify(request) }, parseExact);

/**
 * POSTs `body` as JSON and returns the response body stream (for NDJSON endpoints).
 * Throws ApiError on non-2xx (calling onUnauthorized on a 401); `signal` aborts the request and the read.
 */
export async function streamRequest(path: string, body: unknown, signal: AbortSignal): Promise<ReadableStream<Uint8Array>> {
  if (!config) throw new Error("Management client is not configured; call configureManagementClient first");
  const token = config.getToken();
  const init: RequestInit = { method: "POST", body: JSON.stringify(body), signal };
  const headers = buildHeaders(init, token ? { Authorization: `Bearer ${token}` } : {});
  headers.set("Accept", "application/x-ndjson, application/json");
  const fetchImpl = config.fetchImpl ?? globalThis.fetch.bind(globalThis);
  let response: Response;
  try {
    response = await fetchImpl(`${config.baseUrl}${path}`, { ...init, headers });
  } catch (e) {
    if (signal.aborted || (e instanceof Error && e.name === "AbortError")) throw e;
    throw new ApiError(0, "network", NETWORK_MESSAGE);
  }
  if (!response.ok) {
    const restarted = response.status === 401 ? config.onUnauthorized() === true : false;
    throw await readApiError(response, restarted);
  }
  if (!response.body) throw new ApiError(response.status, "internal", "Unexpected response from Cheladb");
  return response.body;
}
