import { ApiError } from "./error";
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
      return (text === "" ? undefined : JSON.parse(text)) as T;
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

/** Orval mutator for the Management API. */
export const customFetch = async <T>(url: string, options: RequestInit): Promise<T> => {
  if (!config) throw new Error("Management client is not configured; call configureManagementClient first");
  const token = config.getToken();
  const headers = buildHeaders(options, token ? { Authorization: `Bearer ${token}` } : {});
  const fetchImpl = config.fetchImpl ?? globalThis.fetch.bind(globalThis);
  return requestJson<T>(fetchImpl, `${config.baseUrl}${url}`, { ...options, headers }, config.onUnauthorized);
};
