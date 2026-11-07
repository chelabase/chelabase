import {
  ApiError,
  NETWORK_MESSAGE,
  getStorageUploadObjectUrl,
  getStorageUploadPartUrl,
  managementConnection,
  readApiError,
  storageAbortUpload,
  storageCompleteUpload,
  storageCreateUpload,
  storageListParts,
} from "@chelabase/management-client";
import { storageParams } from "@/lib/storage-api";
import type { UploadApi } from "./runner";

/**
 * The runner's routes over Studio's storage console routes, as the Act as role (owner mode when
 * none). Bytes (a single upload, a part) go by XHR, since `fetch` reports no upload progress;
 * the rest use the generated calls.
 */

/** A file's own type, else storage's default (storage reads the declared type, then sniffs). */
const typeOf = (contentType: string) => (contentType === "" ? "application/octet-stream" : contentType);

function sendBytes(path: string, method: "POST" | "PUT", body: Blob, contentType: string, signal: AbortSignal, onProgress: (loaded: number) => void): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal.aborted) return reject(new DOMException("The upload was stopped", "AbortError"));
    const conn = managementConnection();
    const xhr = new XMLHttpRequest();
    const stop = () => xhr.abort();
    const settle = () => signal.removeEventListener("abort", stop);
    xhr.open(method, `${conn.baseUrl}${path}`);
    if (conn.token) xhr.setRequestHeader("Authorization", `Bearer ${conn.token}`);
    xhr.setRequestHeader("Accept", "application/json");
    xhr.setRequestHeader("Content-Type", contentType);
    xhr.upload.onprogress = (e) => onProgress(e.loaded);
    xhr.onload = () => {
      settle();
      if (xhr.status >= 200 && xhr.status < 300) return resolve();
      const restarted = xhr.status === 401 ? conn.onUnauthorized() === true : false;
      void readApiError(new Response(xhr.responseText, { status: xhr.status }), restarted).then(reject);
    };
    xhr.onerror = () => {
      settle();
      reject(new ApiError(0, "network", NETWORK_MESSAGE));
    };
    xhr.onabort = () => {
      settle();
      reject(new DOMException("The upload was stopped", "AbortError"));
    };
    signal.addEventListener("abort", stop, { once: true });
    xhr.send(body);
  });
}

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);

/** A list-parts answer's parts: part number to size. */
export function parseParts(body: unknown): Map<number, number> {
  const out = new Map<number, number>();
  if (!isObject(body) || !Array.isArray(body.parts)) return out;
  for (const p of body.parts) {
    if (isObject(p) && typeof p.part_number === "number" && typeof p.size === "number") out.set(p.part_number, p.size);
  }
  return out;
}

/** `database`: the Postgres database when it isn't the branch's main one (the table editor's). */
export function consoleUploadApi(databaseId: string, branchId: string, role: string | null, database: string | null = null): UploadApi {
  const params = storageParams(role, database);
  const withUpsert = (upsert: boolean) => (upsert ? { ...params, upsert: true } : params);
  return {
    put: (bucket, name, body, o) =>
      sendBytes(getStorageUploadObjectUrl(databaseId, branchId, bucket, name, withUpsert(o.upsert)), "POST", body, typeOf(o.contentType), o.signal, o.onProgress),
    async create(bucket, name, o) {
      const answer = await storageCreateUpload(databaseId, branchId, bucket, name, undefined, withUpsert(o.upsert), {
        signal: o.signal,
        headers: { "Content-Type": typeOf(o.contentType) },
      });
      if (!isObject(answer) || typeof answer.id !== "string") throw new Error("Storage opened an upload without an id");
      return answer.id;
    },
    listParts: async (session, signal) => parseParts(await storageListParts(databaseId, branchId, session, params, { signal })),
    putPart: (session, n, body, o) =>
      sendBytes(getStorageUploadPartUrl(databaseId, branchId, session, n, params), "PUT", body, "application/octet-stream", o.signal, o.onProgress),
    async complete(session, signal) {
      await storageCompleteUpload(databaseId, branchId, session, undefined, params, { signal });
    },
    async abort(session) {
      await storageAbortUpload(databaseId, branchId, session, params);
    },
  };
}
