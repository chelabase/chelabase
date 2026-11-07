import { ApiError } from "./error";
import { getConsoleQueryUrl, getExportMergePlanUrl, getGetDataApiTypesUrl, getStorageDownloadObjectUrl } from "./generated/endpoints";
import type { ConsoleRequest, GetDataApiTypesParams, StorageDownloadObjectParams } from "./generated/model";
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

export const NETWORK_MESSAGE = "Can't reach Chelabase; check your connection";

let config: ManagementClientConfig | undefined;

export function configureManagementClient(next: ManagementClientConfig): void {
  config = next;
}

/**
 * What a request sent outside `customFetch` needs to reach the gateway as the member: the base
 * URL, the current token and the 401 hook. Studio's uploads send their bytes by XHR, since
 * `fetch` reports no upload progress.
 */
export function managementConnection(): { baseUrl: string; token: string | undefined; onUnauthorized: () => boolean | void } {
  if (!config) throw new Error("Management client is not configured; call configureManagementClient first");
  return { baseUrl: config.baseUrl, token: config.getToken(), onUnauthorized: config.onUnauthorized };
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
      throw new ApiError(response.status, "internal", "Unexpected response from Chelabase");
    }
  }
  const restarted = response.status === 401 && onUnauthorized ? onUnauthorized() === true : false;
  throw await readApiError(response, restarted);
}

/**
 * The request's headers with Accept, the `extra` ones, and `application/json` for a string body
 * (the JSON the client serialised) that names no type. A caller's Content-Type is kept, and a
 * Blob, buffer, stream or form body is never labelled JSON.
 */
export function buildHeaders(init: RequestInit, extra: Record<string, string> = {}): Headers {
  const headers = new Headers(init.headers);
  headers.set("Accept", "application/json");
  if (typeof init.body === "string" && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
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

/** Answers that are text, not JSON: the merge plan's SQL. */
const TEXT_ANSWER = /\/plan\.sql$/;

/** Orval mutator for the Management API. */
export const customFetch = async <T>(url: string, options: RequestInit): Promise<T> =>
  send<T>(url, options, TEXT_ANSWER.test(url) ? (text) => text : undefined);

type Reviver = (this: unknown, key: string, value: unknown, context?: { source?: string }) => unknown;

/** A number whose source text a double doesn't print back identically, until `parseExact` decides its form. */
class Literal {
  constructor(
    readonly value: number,
    readonly source: string,
  ) {}
}

/** A nested number (inside a json/jsonb value) stays a number unless digits would be lost. */
function nestedForm(n: Literal): number | string {
  // At most 15 significant digits in plain decimal form always survive a double (`1.50` reads as 1.5).
  if (/^-?\d+(\.\d+)?$/.test(n.source) && n.source.replace(/^-/, "").replace(".", "").replace(/^0+/, "").length <= 15) return n.value;
  return n.source;
}

/** Sets an own field as `JSON.parse` does (a `__proto__` key stays a plain field, never the prototype). */
function put(o: Record<string, unknown>, k: string, v: unknown): void {
  Object.defineProperty(o, k, { value: v, writable: true, enumerable: true, configurable: true });
}

/** Replaces every `Literal` below `value` (in place) by its nested form. */
function settleNested(value: unknown): unknown {
  if (value instanceof Literal) return nestedForm(value);
  if (Array.isArray(value)) {
    for (let i = 0; i < value.length; i++) value[i] = settleNested(value[i]);
  } else if (value !== null && typeof value === "object") {
    const o = value as Record<string, unknown>;
    for (const k of Object.keys(o)) {
      const v = o[k];
      const next = settleNested(v);
      if (next !== v) put(o, k, next);
    }
  }
  return value;
}

/**
 * Parses the console's rows without losing what Postgres wrote. A column value (a field of a row: an object in the
 * top-level array) that is a number stays a JS number only when `String(number)` is exactly its source text; otherwise
 * it comes back as that source text (`2.50`, `1e+20`, `-0`, digits a double can't hold), so Studio shows, edits and
 * saves Postgres's own literal. Numbers nested inside a column's json/jsonb value stay numbers (`{"n":1.50}` reads as
 * `{"n":1.5}`) unless a double would lose digits, which come back as their source text. Runtimes without source-text
 * access fall back to plain `JSON.parse`.
 */
export function parseExact(text: string): unknown {
  let literals = 0;
  const reviver: Reviver = (_key, value, context) => {
    const source = context?.source;
    if (typeof value !== "number" || typeof source !== "string" || String(value) === source) return value;
    literals++;
    return new Literal(value, source);
  };
  const parsed = (JSON.parse as (t: string, r: Reviver) => unknown)(text, reviver);
  if (literals === 0) return parsed;
  if (!Array.isArray(parsed)) return settleNested(parsed);
  for (let i = 0; i < parsed.length; i++) {
    const row: unknown = parsed[i];
    if (row === null || typeof row !== "object" || Array.isArray(row) || row instanceof Literal) {
      parsed[i] = settleNested(row);
      continue;
    }
    const fields = row as Record<string, unknown>;
    for (const k of Object.keys(fields)) {
      const v = fields[k];
      const next = v instanceof Literal ? v.source : settleNested(v);
      if (next !== v) put(fields, k, next);
    }
  }
  return parsed;
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

/** A merge's plan as SQL text (the generated `exportMergePlan` returns the same). */
export const fetchMergePlanSql = async (id: string, mergeId: string): Promise<string> =>
  send<string>(getExportMergePlanUrl(id, mergeId), { method: "GET" }, (text) => text);

/**
 * A branch's generated types as text: the `db.ts` module, or with `format: "json"` the runtime map's JSON (the
 * generated `getDataApiTypes` would parse the module as JSON).
 */
export const fetchDataApiTypes = async (id: string, branchId: string, params?: GetDataApiTypesParams): Promise<string> =>
  send<string>(getGetDataApiTypesUrl(id, branchId, params), { method: "GET" }, (text) => text);

/**
 * An object's bytes through Studio's storage console route (`GET …/storage/object/{bucket}/{name}`), as the role
 * (the generated `storageDownloadObject` would parse them as JSON). `range` (one `bytes=` range) passes through and a
 * 206 is a success; the Blob's type is the served Content-Type.
 */
export async function fetchStorageObject(
  id: string,
  branchId: string,
  bucket: string,
  name: string,
  params?: StorageDownloadObjectParams,
  options: { range?: string; signal?: AbortSignal } = {},
): Promise<Blob> {
  if (!config) throw new Error("Management client is not configured; call configureManagementClient first");
  const token = config.getToken();
  const init: RequestInit = { method: "GET", signal: options.signal };
  const headers = buildHeaders(init, token ? { Authorization: `Bearer ${token}` } : {});
  headers.set("Accept", "*/*");
  if (options.range) headers.set("Range", options.range);
  const fetchImpl = config.fetchImpl ?? globalThis.fetch.bind(globalThis);
  let response: Response;
  try {
    response = await fetchImpl(`${config.baseUrl}${getStorageDownloadObjectUrl(id, branchId, bucket, name, params)}`, { ...init, headers });
  } catch (e) {
    if (options.signal?.aborted || (e instanceof Error && e.name === "AbortError")) throw e;
    throw new ApiError(0, "network", NETWORK_MESSAGE);
  }
  if (!response.ok) {
    const restarted = response.status === 401 ? config.onUnauthorized() === true : false;
    throw await readApiError(response, restarted);
  }
  return response.blob();
}

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
  if (!response.body) throw new ApiError(response.status, "internal", "Unexpected response from Chelabase");
  return response.body;
}
